//! Finding the web and mail addresses written as plain text, so a reader can click them.
//!
//! One finder for every surface: the reading view's HTML ([`sanitize`](crate::sanitize)) and the
//! text a host draws itself (a plain-text body, an event's notes, an invitation's description), so
//! a link is a link in all of them or in none. Recognised: `http://` and `https://` addresses,
//! `mailto:` addresses and a bare `www.` host, which is opened over `https`. Bare domains and bare
//! email addresses are not: too much ordinary prose matches them. A `tel:` address is not either;
//! a composer's link editor is the only way to make one, so a number in a message is never
//! dialled by accident.

use std::ops::Range;

use crate::link::has_link_scheme;

/// An address found in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundLink {
    range: Range<usize>,
    href: String,
}

impl FoundLink {
    /// Byte range of the address in the text, always on character boundaries.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// The target to open: the address itself, or `https://` and the address for a `www.` host.
    #[must_use]
    pub fn href(&self) -> &str {
        &self.href
    }
}

/// A run of text and, when it is an address, where it leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSegment {
    text: String,
    link: Option<String>,
}

impl TextSegment {
    /// The text exactly as written.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The target to open, when this segment is an address.
    #[must_use]
    pub fn link(&self) -> Option<&str> {
        self.link.as_deref()
    }
}

/// Where each prefix leads: the address as written, or with `https://` in front.
const PREFIXES: [(&str, bool); 4] = [
    ("https://", false),
    ("http://", false),
    ("mailto:", false),
    ("www.", true),
];

/// Characters that end an address outright: whitespace and controls are checked separately.
const STOPS: [char; 6] = ['<', '>', '"', '`', '{', '}'];

/// Characters an address does not end with, though it may contain them: the punctuation of the
/// sentence around it ("see https://example.com.").
const TRAILING: [char; 9] = ['.', ',', ';', ':', '!', '?', '\'', '*', '_'];

/// Every address in `text`, in order, never overlapping.
#[must_use]
pub fn find_links(text: &str) -> Vec<FoundLink> {
    let mut links = Vec::new();
    let mut from = 0;
    while let Some(found) = next_link(text, from) {
        from = found.range.end;
        links.push(found);
    }
    links
}

/// `text` split into plain runs and addresses, covering all of it. Empty text gives no segments.
#[must_use]
pub fn segments(text: &str) -> Vec<TextSegment> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for link in find_links(text) {
        if link.range.start > cursor {
            out.push(TextSegment {
                text: text[cursor..link.range.start].to_owned(),
                link: None,
            });
        }
        out.push(TextSegment {
            text: text[link.range.clone()].to_owned(),
            link: Some(link.href),
        });
        cursor = link.range.end;
    }
    if cursor < text.len() {
        out.push(TextSegment {
            text: text[cursor..].to_owned(),
            link: None,
        });
    }
    out
}

fn next_link(text: &str, from: usize) -> Option<FoundLink> {
    let mut search = from;
    loop {
        // Every prefix starts with one of these letters, so the scan skips straight to candidates.
        let offset = text[search..].find(['h', 'H', 'm', 'M', 'w', 'W'])?;
        let start = search + offset;
        if let Some(found) = link_at(text, start) {
            return Some(found);
        }
        search = start + 1;
    }
}

fn link_at(text: &str, start: usize) -> Option<FoundLink> {
    // An address starts a word: `xhttps://` and `foo.www.example.com` are not one.
    if text[..start].chars().next_back().is_some_and(|c| {
        c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '/' | ':' | '+' | '%')
    }) {
        return None;
    }
    let rest = &text[start..];
    let (prefix, secure) = PREFIXES.iter().find(|(prefix, _)| {
        rest.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })?;
    let raw_end = rest
        .find(|c: char| c.is_whitespace() || c.is_control() || STOPS.contains(&c))
        .unwrap_or(rest.len());
    let address = trim_trailing(&rest[..raw_end]);
    // Trimming can eat into the prefix itself (`mailto:` loses its colon), which leaves no address.
    let body = address.get(prefix.len()..)?;
    let plausible = match *prefix {
        "mailto:" => body
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty()),
        "www." => host_of(body).contains('.') && starts_alphanumeric(body),
        _ => starts_alphanumeric(body),
    };
    if !plausible {
        return None;
    }
    let href = if *secure {
        format!("https://{address}")
    } else {
        address.to_owned()
    };
    has_link_scheme(&href).then(|| FoundLink {
        range: start..start + address.len(),
        href,
    })
}

/// Drops the sentence's punctuation from the end of an address, and a closing bracket the address
/// did not open: `(see https://example.com/a)` links `https://example.com/a`, while
/// `https://en.wikipedia.org/wiki/Mail_(protocol)` keeps its own.
fn trim_trailing(address: &str) -> &str {
    let mut end = address;
    loop {
        let Some(last) = end.chars().next_back() else {
            return end;
        };
        let unbalanced = |open: char| end.matches(open).count() < end.matches(last).count();
        let drop = TRAILING.contains(&last)
            || (last == ')' && unbalanced('('))
            || (last == ']' && unbalanced('['));
        if !drop {
            return end;
        }
        end = &end[..end.len() - last.len_utf8()];
    }
}

fn host_of(rest: &str) -> &str {
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

fn starts_alphanumeric(rest: &str) -> bool {
    rest.chars().next().is_some_and(char::is_alphanumeric)
}

#[cfg(test)]
#[path = "linkify_tests.rs"]
mod tests;
