//! Exact-link connected components and stable person-id assignment.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    CanonicalEmail,
    model::{PeopleSnapshot, Person, PersonSource, PersonSourceId, SourcedValue},
};
use crate::{contact::ContactSourceClass, ids::PersonId};

/// A people rebuild could not produce a valid persistent id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeopleError {
    /// The store-local id counter overflowed.
    #[error("person id space exhausted")]
    IdExhausted,
}

#[derive(Debug)]
struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn root(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            self.parent[index] = self.root(self.parent[index]);
        }
        self.parent[index]
    }

    fn union(&mut self, left: usize, right: usize) {
        let left = self.root(left);
        let right = self.root(right);
        if left != right {
            let (keep, merge) = if left < right {
                (left, right)
            } else {
                (right, left)
            };
            self.parent[merge] = keep;
        }
    }
}

/// Rebuilds unified people from a complete, consistent source generation.
///
/// Invalid source emails remain visible on their source card but do not become
/// identity links. The returned snapshot deterministically preserves, merges,
/// splits, or mints [`PersonId`] values relative to `previous`.
///
/// # Errors
///
/// Returns [`PeopleError::IdExhausted`] if the store-local id counter overflows.
pub fn rebuild_people(
    sources: &[PersonSource],
    previous: &PeopleSnapshot,
) -> Result<PeopleSnapshot, PeopleError> {
    let mut ordered: Vec<&PersonSource> = sources.iter().collect();
    ordered.sort_by(|left, right| left.id.cmp(&right.id));
    let mut sets = DisjointSet::new(ordered.len());
    join_sources(&ordered, &mut sets);
    let components = collect_components(&ordered, &mut sets);
    let prior = prior_component_candidates(&components, previous);
    let owners = old_id_owners(&prior);

    let mut aliases = previous.aliases.clone();
    let mut next_id = previous.next_id.max(
        previous
            .people
            .iter()
            .map(|person| person.id.get() + 1)
            .max()
            .unwrap_or(1),
    );
    let mut people = Vec::with_capacity(components.len());
    for (component_index, component) in components.iter().enumerate() {
        let eligible: BTreeSet<PersonId> = prior[component_index]
            .iter()
            .copied()
            .filter(|old| owners.get(old) == Some(&component_index))
            .collect();
        let id = if let Some(keep) = eligible.first().copied() {
            for retired in eligible.iter().copied().skip(1) {
                aliases.insert(retired, keep);
            }
            keep
        } else {
            let id = PersonId::new(next_id).map_err(|_| PeopleError::IdExhausted)?;
            next_id = next_id.checked_add(1).ok_or(PeopleError::IdExhausted)?;
            id
        };
        people.push(materialize_person(id, component));
    }
    flatten_aliases(&mut aliases);
    Ok(PeopleSnapshot {
        people,
        aliases,
        next_id,
    })
}

/// Unions the sources that provably describe the same person.
///
/// Shared canonical email is the only safe join signal available: no provider
/// supplies a stable cross-source person handle, so there is nothing else to key on.
/// Names deliberately do not join — two different people commonly share one.
fn join_sources(sources: &[&PersonSource], sets: &mut DisjointSet) {
    let mut emails: BTreeMap<CanonicalEmail, usize> = BTreeMap::new();
    for (index, source) in sources.iter().enumerate() {
        for property in source.card.emails.values() {
            if let Ok(email) = CanonicalEmail::parse(&property.value.address)
                && let Some(other) = emails.insert(email, index)
            {
                sets.union(index, other);
            }
        }
    }
}

fn collect_components<'a>(
    sources: &[&'a PersonSource],
    sets: &mut DisjointSet,
) -> Vec<Vec<&'a PersonSource>> {
    let mut grouped: BTreeMap<usize, Vec<&PersonSource>> = BTreeMap::new();
    for (index, source) in sources.iter().enumerate() {
        grouped.entry(sets.root(index)).or_default().push(*source);
    }
    let mut components: Vec<Vec<&PersonSource>> = grouped.into_values().collect();
    components.sort_by(|left, right| left[0].id.cmp(&right[0].id));
    components
}

fn prior_component_candidates(
    components: &[Vec<&PersonSource>],
    previous: &PeopleSnapshot,
) -> Vec<BTreeSet<PersonId>> {
    let source_to_old: BTreeMap<&PersonSourceId, PersonId> = previous
        .people
        .iter()
        .flat_map(|person| person.sources.iter().map(move |source| (source, person.id)))
        .collect();
    components
        .iter()
        .map(|component| {
            component
                .iter()
                .filter_map(|source| source_to_old.get(&source.id).copied())
                .collect()
        })
        .collect()
}

fn old_id_owners(candidates: &[BTreeSet<PersonId>]) -> BTreeMap<PersonId, usize> {
    let mut owners = BTreeMap::new();
    for (index, ids) in candidates.iter().enumerate() {
        for id in ids {
            owners.entry(*id).or_insert(index);
        }
    }
    owners
}

fn materialize_person(id: PersonId, sources: &[&PersonSource]) -> Person {
    let source_ids: BTreeSet<PersonSourceId> =
        sources.iter().map(|source| source.id.clone()).collect();
    let kinds = sources
        .iter()
        .map(|source| source.card.kind.clone())
        .collect();
    let names = sourced_strings(sources, |source| {
        source.card.display_name().into_iter().collect()
    });
    let emails = sourced_emails(sources);
    let phones = sourced_strings(sources, |source| {
        source
            .card
            .phones
            .values()
            .map(|property| property.value.number.clone())
            .collect()
    });
    let organizations = sourced_strings(sources, |source| {
        source
            .card
            .organizations
            .values()
            .map(|property| property.value.name.clone())
            .collect()
    });
    let titles = sourced_strings(sources, |source| {
        source
            .card
            .titles
            .values()
            .map(|property| property.value.name.clone())
            .collect()
    });
    let display_name = preferred_name(sources)
        .or_else(|| emails.first().map(|value| value.value.to_string()))
        .unwrap_or_else(|| "Unnamed contact".into());
    Person {
        id,
        display_name,
        sources: source_ids,
        kinds,
        names,
        emails,
        phones,
        organizations,
        titles,
        is_saved: sources
            .iter()
            .any(|source| source.source_class == ContactSourceClass::Personal),
        is_writable: sources.iter().any(|source| source.writable),
    }
}

fn sourced_strings(
    sources: &[&PersonSource],
    values: impl Fn(&PersonSource) -> Vec<String>,
) -> Vec<SourcedValue<String>> {
    let mut union: BTreeMap<String, BTreeSet<PersonSourceId>> = BTreeMap::new();
    for source in sources {
        for value in values(source).into_iter().filter(|value| !value.is_empty()) {
            union.entry(value).or_default().insert(source.id.clone());
        }
    }
    union
        .into_iter()
        .map(|(value, sources)| SourcedValue { value, sources })
        .collect()
}

fn sourced_emails(sources: &[&PersonSource]) -> Vec<SourcedValue<CanonicalEmail>> {
    let mut union: BTreeMap<CanonicalEmail, BTreeSet<PersonSourceId>> = BTreeMap::new();
    for source in sources {
        for property in source.card.emails.values() {
            if let Ok(email) = CanonicalEmail::parse(&property.value.address) {
                union.entry(email).or_default().insert(source.id.clone());
            }
        }
    }
    union
        .into_iter()
        .map(|(value, sources)| SourcedValue { value, sources })
        .collect()
}

fn preferred_name(sources: &[&PersonSource]) -> Option<String> {
    sources
        .iter()
        .filter_map(|source| {
            let name = source.card.display_name()?;
            Some((source_priority(source), &source.id, name))
        })
        .min_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(right.1)))
        .map(|(_, _, name)| name)
}

fn source_priority(source: &PersonSource) -> u8 {
    match (source.source_class, source.writable) {
        (ContactSourceClass::Personal, true) => 0,
        (ContactSourceClass::Personal | ContactSourceClass::Suggested, false)
        | (ContactSourceClass::Suggested, true) => 1,
        (ContactSourceClass::Directory, _) => 2,
        (ContactSourceClass::MailHistory, _) => 3,
    }
}

fn flatten_aliases(aliases: &mut BTreeMap<PersonId, PersonId>) {
    let keys: Vec<PersonId> = aliases.keys().copied().collect();
    for key in keys {
        let mut target = aliases[&key];
        let mut remaining = aliases.len().saturating_add(1);
        while let Some(next) = aliases.get(&target).copied() {
            target = next;
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
        if key != target {
            aliases.insert(key, target);
        }
    }
}
