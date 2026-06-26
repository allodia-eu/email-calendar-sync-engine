//! The search executor: compiling an `engine-search` query AST to SQLite.
//!
//! `engine-search` owns the store-agnostic half (AST, parser, RRF, coverage);
//! this module is the SQLite half (`north-star.md` Search Contract). It compiles a
//! [`MailQuery`]/[`CalendarQuery`] into:
//!
//! - a **structured-filter predicate** over the normalized scalar and junction
//!   tables (address/participant/membership lookups, scalar flags, date and
//!   occurrence ranges) — exact, indexed equality, AND across filters and OR
//!   within a repeated one;
//! - an optional **FTS5 `MATCH`** over `fts_index`, ranked by `bm25()`, for the
//!   free-text and `subject:`/`location:` terms;
//!
//! then fuses the ranked candidate sources with reciprocal-rank fusion
//! (`engine_search::fuse`). Vector KNN is a later, feature-gated source that joins
//! the same fusion; for now full-text is the only ranked source, so a query with
//! no text falls back to a deterministic order (mail by date, calendar by key).
//!
//! Address values are normalized with the same `engine_core::search_index`
//! function the projection used, so a query address matches the stored one.

use engine_core::coverage::SearchCoverage;
use engine_core::ids::ProviderKey;
use engine_core::search_index::normalize_addr;
use engine_core::time::CalendarDate;
use engine_search::{
    CalendarQuery, MailQuery, RrfK, SearchHit, SearchResults, TextField, TextQuery, assemble, fuse,
};
use engine_store::Result;
use rusqlite::{Connection, ToSql, params_from_iter, types::ToSqlOutput};

use crate::convert;

/// Converts ranked `(key, score)` rows into [`SearchResults`], assembling coverage
/// from the searched scopes.
///
/// v1 reports each searched scope as locally complete: real gap detection
/// (unsynced/unindexed objects via partial sync, and recurrence-horizon bounds)
/// arrives with sync-state and occurrence-horizon integration. The assembly path
/// (`engine_search::assemble`) is wired so those facts compose in unchanged.
pub(crate) fn assemble_results(
    ranked: Vec<(String, f64)>,
    scope_count: usize,
) -> Result<SearchResults> {
    let hits = ranked
        .into_iter()
        .map(|(key, score)| {
            Ok(SearchHit::new(
                ProviderKey::new(key).map_err(convert::backend)?,
                score,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let coverage = assemble((0..scope_count).map(|_| SearchCoverage::complete()));
    Ok(SearchResults::new(hits, coverage))
}

/// A bound query parameter (text or integer), so dynamically-built SQL can carry
/// mixed-type bindings positionally.
#[derive(Clone)]
enum Param {
    Text(String),
    Int(i64),
}

impl ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match self {
            Param::Text(value) => value.to_sql(),
            Param::Int(value) => value.to_sql(),
        }
    }
}

/// An accumulating `AND`-joined filter predicate and its positional parameters.
#[derive(Default)]
struct Filter {
    sql: String,
    params: Vec<Param>,
}

impl Filter {
    /// Appends `clause` (with its own `?` placeholders) and its parameters.
    fn and(&mut self, clause: &str, params: impl IntoIterator<Item = Param>) {
        self.sql.push_str(" AND ");
        self.sql.push_str(clause);
        self.params.extend(params);
    }
}

/// A domain's base table: its `FROM` clause and the alias the rest of the query
/// correlates against. Carrying both together avoids recovering the alias by
/// re-parsing the `FROM` string.
struct Source {
    from: &'static str,
    alias: &'static str,
}

const MAIL: Source = Source {
    from: "mail_index mi",
    alias: "mi",
};
const CALENDAR: Source = Source {
    from: "event_index ei",
    alias: "ei",
};

/// Runs a search for `account`'s mail in `scope_keys`, returning ranked
/// `(provider_key, score)`.
///
/// Free text is matched against two FTS sources and fused with RRF: the
/// scope-derived `fts_index` (subject + sender/recipient text), and the lease-free
/// `message_body_fts` over the on-demand-fetched body text. The body source is joined
/// to `mail_index` so only **live, in-scope** keys count (a stale body row for a
/// since-deleted message is dropped), and to `message_body.account` so IMAP keys that
/// collide across accounts cannot cross over.
pub(crate) fn search_mail(
    conn: &Connection,
    account: &str,
    scope_keys: &[String],
    query: &MailQuery,
    limit: usize,
) -> Result<Vec<(String, f64)>> {
    if scope_keys.is_empty() {
        return Ok(Vec::new());
    }
    let filter = mail_filter(query);
    match fts_match(&query.text) {
        Some(text) => {
            let metadata = fts_candidates(conn, &MAIL, scope_keys, &filter, &text, limit)?;
            let body = body_candidates(conn, account, scope_keys, &filter, &query.text, limit)?;
            Ok(fuse_keys(&[metadata.as_slice(), body.as_slice()], limit))
        }
        None => scalar_query(conn, &MAIL, scope_keys, &filter, "mi.date_utc DESC", limit),
    }
}

/// Runs a search for calendar events in `scope_keys`.
pub(crate) fn search_calendar(
    conn: &Connection,
    scope_keys: &[String],
    query: &CalendarQuery,
    limit: usize,
) -> Result<Vec<(String, f64)>> {
    if scope_keys.is_empty() {
        return Ok(Vec::new());
    }
    let filter = calendar_filter(query);
    match fts_match(&query.text) {
        Some(text) => {
            let keys = fts_candidates(conn, &CALENDAR, scope_keys, &filter, &text, limit)?;
            Ok(fuse_keys(&[keys.as_slice()], limit))
        }
        // No text and no relevance signal yet: order by key for determinism.
        None => scalar_query(
            conn,
            &CALENDAR,
            scope_keys,
            &filter,
            "ei.provider_key",
            limit,
        ),
    }
}

/// Builds the mail structured-filter predicate, correlated to base alias `mi`.
fn mail_filter(query: &MailQuery) -> Filter {
    let mut filter = Filter::default();
    if let Some(has_attachment) = query.has_attachment {
        filter.and(
            "mi.has_attachment = ?",
            [Param::Int(i64::from(has_attachment))],
        );
    }
    date_bounds(&mut filter, "mi.date_utc", query.after, query.before);
    address_filter(&mut filter, "from", &query.from);
    address_filter(&mut filter, "to", &query.to);
    address_filter(&mut filter, "cc", &query.cc);
    // The model unifies mailbox and label as one membership kind; both AND in.
    membership_filter(&mut filter, "mailbox", &query.mailbox, false);
    membership_filter(&mut filter, "mailbox", &query.label, false);
    membership_filter(&mut filter, "keyword", &query.keyword, true);
    filter
}

/// Builds the calendar structured-filter predicate, correlated to base alias `ei`.
fn calendar_filter(query: &CalendarQuery) -> Filter {
    let mut filter = Filter::default();
    if let Some(has_conference) = query.has_conference {
        filter.and(
            "ei.has_conference = ?",
            [Param::Int(i64::from(has_conference))],
        );
    }
    if !query.rsvp.is_empty() {
        let placeholders = in_list(query.rsvp.len());
        filter.and(
            &format!("ei.my_partstat IN ({placeholders})"),
            query
                .rsvp
                .iter()
                .map(|s| Param::Text(s.as_str().to_owned())),
        );
    }
    calendar_membership(&mut filter, &query.calendar);
    participant_filter(&mut filter, "attendee", &query.attendee);
    participant_filter(&mut filter, "organizer", &query.organizer);
    occurrence_bounds(&mut filter, query.after, query.before);
    filter
}

/// Adds inclusive-lower / exclusive-upper date bounds on `column` (an ISO instant
/// text column), interpreting each `YYYY-MM-DD` as that day's `00:00:00Z`.
fn date_bounds(
    filter: &mut Filter,
    column: &str,
    after: Option<CalendarDate>,
    before: Option<CalendarDate>,
) {
    if let Some(after) = after {
        filter.and(&format!("{column} >= ?"), [Param::Text(day_start(after))]);
    }
    if let Some(before) = before {
        filter.and(&format!("{column} < ?"), [Param::Text(day_start(before))]);
    }
}

fn day_start(date: CalendarDate) -> String {
    format!("{date}T00:00:00Z")
}

/// `EXISTS` on the address junction: the message has a `field` address among
/// `values` (normalized like the stored ones).
fn address_filter(filter: &mut Filter, field: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let placeholders = in_list(values.len());
    let clause = format!(
        "EXISTS (SELECT 1 FROM mail_address a WHERE a.scope_key = mi.scope_key \
         AND a.provider_key = mi.provider_key AND a.field = ? AND a.addr IN ({placeholders}))"
    );
    let mut params = vec![Param::Text(field.to_owned())];
    params.extend(values.iter().map(|v| Param::Text(normalize_addr(v))));
    filter.and(&clause, params);
}

/// `EXISTS` on the membership junction for a mail object (alias `mi`). Keyword
/// values are lowercased to match the stored canonical form.
fn membership_filter(filter: &mut Filter, kind: &str, values: &[String], lowercase: bool) {
    if values.is_empty() {
        return;
    }
    let placeholders = in_list(values.len());
    let clause = format!(
        "EXISTS (SELECT 1 FROM membership m WHERE m.scope_key = mi.scope_key \
         AND m.provider_key = mi.provider_key AND m.kind = ? AND m.value IN ({placeholders}))"
    );
    let mut params = vec![Param::Text(kind.to_owned())];
    params.extend(values.iter().map(|v| {
        Param::Text(if lowercase {
            v.to_lowercase()
        } else {
            v.clone()
        })
    }));
    filter.and(&clause, params);
}

/// `EXISTS` on the membership junction for an event (alias `ei`, kind `calendar`).
fn calendar_membership(filter: &mut Filter, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let placeholders = in_list(values.len());
    let clause = format!(
        "EXISTS (SELECT 1 FROM membership m WHERE m.scope_key = ei.scope_key \
         AND m.provider_key = ei.provider_key AND m.kind = 'calendar' AND m.value IN ({placeholders}))"
    );
    filter.and(&clause, values.iter().map(|v| Param::Text(v.clone())));
}

/// `EXISTS` on the participant junction: an event participant in `role` whose
/// address is among `values`.
fn participant_filter(filter: &mut Filter, role: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let placeholders = in_list(values.len());
    let clause = format!(
        "EXISTS (SELECT 1 FROM event_participant p WHERE p.scope_key = ei.scope_key \
         AND p.provider_key = ei.provider_key AND p.role = ? AND p.addr IN ({placeholders}))"
    );
    let mut params = vec![Param::Text(role.to_owned())];
    params.extend(values.iter().map(|v| Param::Text(normalize_addr(v))));
    filter.and(&clause, params);
}

/// `EXISTS` on the occurrence table: the event has a materialized occurrence whose
/// start falls in the requested half-open day range.
fn occurrence_bounds(
    filter: &mut Filter,
    after: Option<CalendarDate>,
    before: Option<CalendarDate>,
) {
    if after.is_none() && before.is_none() {
        return;
    }
    let mut conds: Vec<&str> = Vec::new();
    let mut params: Vec<Param> = Vec::new();
    if let Some(after) = after {
        conds.push("o.start_utc >= ?");
        params.push(Param::Text(day_start(after)));
    }
    if let Some(before) = before {
        conds.push("o.start_utc < ?");
        params.push(Param::Text(day_start(before)));
    }
    let clause = format!(
        "EXISTS (SELECT 1 FROM event_occurrence o WHERE o.scope_key = ei.scope_key \
         AND o.event = ei.provider_key AND {})",
        conds.join(" AND ")
    );
    filter.and(&clause, params);
}

/// Builds the FTS5 `MATCH` string for a domain's text, or `None` if empty. Every
/// term is a quoted-phrase **prefix** query (`"term"*`) so search-as-you-type
/// matches partial words (`allo` matches `allodia`); the quoting still keeps user
/// input from injecting FTS operators, and scoped terms carry a column filter.
fn fts_match(text: &TextQuery) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = text.unscoped.iter().map(|t| quote_term(t)).collect();
    for scoped in &text.scoped {
        let column = match scoped.field {
            TextField::Subject => "subject",
            TextField::Location => "location",
        };
        parts.push(format!("{column}:{}", quote_term(&scoped.text)));
    }
    Some(parts.join(" "))
}

/// Wraps a term as an FTS5 prefix query: a quoted phrase (embedded quotes doubled)
/// followed by `*`, so the term matches any token that *starts with* it.
fn quote_term(term: &str) -> String {
    format!("\"{}\"*", term.replace('"', "\"\""))
}

/// `?,?,…` for an `IN` list of `n` values (`n >= 1`).
fn in_list(n: usize) -> String {
    let mut out = String::from("?");
    for _ in 1..n {
        out.push_str(",?");
    }
    out
}

/// Runs the scope-derived FTS-ranked query, returning the candidate keys in rank
/// order (best first) — one input list to [`fuse_keys`].
fn fts_candidates(
    conn: &Connection,
    source: &Source,
    scope_keys: &[String],
    filter: &Filter,
    text: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let Source { from, alias } = *source;
    let sql = format!(
        "SELECT {alias}.provider_key, bm25(fts_index) AS rank \
         FROM {from} \
         JOIN fts_doc f ON f.scope_key = {alias}.scope_key AND f.provider_key = {alias}.provider_key \
         JOIN fts_index ON fts_index.rowid = f.rowid \
         WHERE fts_index MATCH ? AND {alias}.scope_key IN ({}){} \
         ORDER BY rank LIMIT ?",
        in_list(scope_keys.len()),
        filter.sql,
    );
    let mut params = vec![Param::Text(text.to_owned())];
    params.extend(scope_keys.iter().map(|s| Param::Text(s.clone())));
    params.extend(filter.params.iter().cloned());
    params.push(Param::Int(limit_param(limit)));
    run(conn, &sql, &params)
}

/// Runs the lease-free body-text FTS, returning the candidate keys in rank order, or
/// empty when the query has no free-text terms (a purely `subject:`-scoped query does
/// not search the body). The body `message_body_fts` is joined to `mail_index` (live,
/// in-scope keys only) and filtered by `account`, and the same structured `filter`
/// applies (it correlates to the joined `mi`).
fn body_candidates(
    conn: &Connection,
    account: &str,
    scope_keys: &[String],
    filter: &Filter,
    text: &TextQuery,
    limit: usize,
) -> Result<Vec<String>> {
    let Some(match_text) = body_match(text) else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT mb.provider_key, bm25(message_body_fts) AS rank \
         FROM message_body mb \
         JOIN message_body_fts ON message_body_fts.rowid = mb.rowid \
         JOIN mail_index mi ON mi.provider_key = mb.provider_key AND mi.scope_key IN ({}) \
         WHERE mb.account = ? AND message_body_fts MATCH ?{} \
         ORDER BY rank LIMIT ?",
        in_list(scope_keys.len()),
        filter.sql,
    );
    let mut params: Vec<Param> = scope_keys.iter().map(|s| Param::Text(s.clone())).collect();
    params.push(Param::Text(account.to_owned()));
    params.push(Param::Text(match_text));
    params.extend(filter.params.iter().cloned());
    params.push(Param::Int(limit_param(limit)));
    run(conn, &sql, &params)
}

/// The FTS5 `MATCH` for the body source: the **unscoped** free-text terms only (the
/// body has a single `plain` column; `subject:`/`location:` qualifiers do not apply),
/// or `None` if there are none.
fn body_match(text: &TextQuery) -> Option<String> {
    if text.unscoped.is_empty() {
        return None;
    }
    Some(
        text.unscoped
            .iter()
            .map(|t| quote_term(t))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Fuses one or more rank-ordered candidate lists with RRF and truncates to `limit`
/// (the integration point a vector source later joins).
fn fuse_keys(lists: &[&[String]], limit: usize) -> Vec<(String, f64)> {
    fuse(lists, RrfK::DEFAULT)
        .into_iter()
        .take(limit)
        .map(|f| (f.key, f.score))
        .collect()
}

/// Runs the no-text structured query, ordered by `order`, with score `0.0`.
fn scalar_query(
    conn: &Connection,
    source: &Source,
    scope_keys: &[String],
    filter: &Filter,
    order: &str,
    limit: usize,
) -> Result<Vec<(String, f64)>> {
    let Source { from, alias } = *source;
    let sql = format!(
        "SELECT {alias}.provider_key FROM {from} \
         WHERE {alias}.scope_key IN ({}){} ORDER BY {order} LIMIT ?",
        in_list(scope_keys.len()),
        filter.sql,
    );
    let mut params: Vec<Param> = scope_keys.iter().map(|s| Param::Text(s.clone())).collect();
    params.extend(filter.params.iter().cloned());
    params.push(Param::Int(limit_param(limit)));
    let keys = run(conn, &sql, &params)?;
    Ok(keys.into_iter().map(|k| (k, 0.0)).collect())
}

fn limit_param(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// Executes a prepared query returning the first column (provider key) of each row.
fn run(conn: &Connection, sql: &str, params: &[Param]) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql).map_err(convert::backend)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |r| r.get::<_, String>(0))
        .map_err(convert::backend)?;
    let mut keys = Vec::new();
    for row in rows {
        keys.push(row.map_err(convert::backend)?);
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_search::ScopedTerm;

    /// Each term becomes a quoted-phrase prefix query (`"term"*`); scoped terms
    /// keep their column filter. This is the search-as-you-type form, so a typed
    /// `allo` matches a stored `allodia`.
    #[test]
    fn fts_match_builds_prefix_phrases() {
        let text = TextQuery {
            unscoped: vec!["allo".into(), "bar".into()],
            scoped: vec![ScopedTerm {
                field: TextField::Subject,
                text: "allo".into(),
            }],
        };
        assert_eq!(
            fts_match(&text).as_deref(),
            Some(r#""allo"* "bar"* subject:"allo"*"#)
        );
    }

    #[test]
    fn fts_match_is_none_for_empty_text() {
        assert_eq!(fts_match(&TextQuery::default()), None);
    }

    /// Embedded quotes are doubled (injection-safe) and the `*` is appended after
    /// the closing quote, not inside it.
    #[test]
    fn quote_term_doubles_quotes_then_appends_star() {
        assert_eq!(quote_term(r#"a"b"#), r#""a""b"*"#);
    }
}
