//! How important the sender marked a message, and the one rule every transport reads it by.

use serde::{Deserialize, Serialize};

/// The importance a message carries: the `Importance` header of RFC 2156 §5.3, registered by
/// RFC 4021 §2.1.52 (`low` / `normal` / `high`).
///
/// The sender writes it, but it is not fixed for the life of the message on every server.
/// Exchange keeps it as a property of the stored message, which Outlook lets a recipient change and
/// a draft's re-save rewrites, and Graph reports such a change as a changed message under the same
/// id. It is therefore read wherever the message's content is read, never only when the message
/// first arrives.
///
/// A message that says nothing is [`Normal`](Self::Normal): that is what the absence of every
/// header means, not a value the engine invents. Which headers are read, in which order, is
/// [`from_headers`](Self::from_headers). RFC 2156's `Priority` is the delivery priority an MTA acts
/// on, a different property, and is not read.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    /// `Importance: low`.
    Low,
    /// `Importance: normal`, or no importance stated.
    #[default]
    Normal,
    /// `Importance: high`.
    High,
}

/// The value of each header that can state an importance, as one message carries them: the
/// **last** occurrence of each, unfolded, or `None` for a header the message does not carry.
///
/// The last, because that is what Exchange reads and what a JMAP server's `header:…:asText` returns
/// (RFC 8621 §4.1.3): a message carrying a header twice then reads the same on every transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportanceHeaders<'a> {
    /// `Importance` (RFC 2156 §5.3).
    pub importance: Option<&'a str>,
    /// `X-MSMail-Priority`, Outlook's own (`High`, `Normal`, `Low`).
    pub x_msmail_priority: Option<&'a str>,
    /// `X-Priority`, what Thunderbird and Apple Mail write (`1` to `5`).
    pub x_priority: Option<&'a str>,
}

impl<'a> ImportanceHeaders<'a> {
    /// Reads the three through `last`, which answers a header's last value by its name in
    /// [`Importance::HEADERS`], or `None` when the message lacks it.
    #[must_use]
    pub fn from_lookup(mut last: impl FnMut(&str) -> Option<&'a str>) -> Self {
        let [importance, x_msmail_priority, x_priority] = Importance::HEADERS;
        Self {
            importance: last(importance),
            x_msmail_priority: last(x_msmail_priority),
            x_priority: last(x_priority),
        }
    }
}

impl Importance {
    /// Every header [`from_headers`](Self::from_headers) reads, in the order it consults them. A
    /// transport that reads headers by name asks for exactly these.
    pub const HEADERS: [&'static str; 3] = ["Importance", "X-MSMail-Priority", "X-Priority"];

    /// The RFC 2156 token, as the `Importance` header and Microsoft Graph's `importance` property
    /// both spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    /// Reads an RFC 2156 `Importance` value, an `X-MSMail-Priority` value, or Graph's `importance`:
    /// the token, in any case, with the whitespace and comments a structured header may carry
    /// around it (RFC 5322 §3.2.2). `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let value = without_comments(value);
        let value = value.trim();
        [Self::Low, Self::Normal, Self::High]
            .into_iter()
            .find(|importance| value.eq_ignore_ascii_case(importance.as_str()))
    }

    /// Reads an `X-Priority` value: a single digit, `1` the highest and `5` the lowest, with any
    /// comment beside it ignored (`1 (Highest)`). `1` and `2` are high, `3` is normal, `4` and `5`
    /// are low, as the clients that write it and Exchange read it. A word is not a priority: `None`
    /// for anything but those digits.
    #[must_use]
    pub fn from_x_priority(value: &str) -> Option<Self> {
        let value = without_comments(value);
        match value.split_whitespace().next()? {
            "1" | "2" => Some(Self::High),
            "3" => Some(Self::Normal),
            "4" | "5" => Some(Self::Low),
            _ => None,
        }
    }

    /// The importance a message's headers state, read as Exchange reads it, so a message shows the
    /// same importance on every account whichever transport carries it:
    ///
    /// 1. `Importance`, when the message carries one;
    /// 2. else `X-MSMail-Priority`, when it carries one;
    /// 3. else `X-Priority`;
    /// 4. else [`Normal`](Self::Normal).
    ///
    /// The first header present decides, and one that cannot be read states normal: a later header
    /// is not consulted in its place. Both are what Exchange does (observed in
    /// `provider-graph/tests/fixtures/mail/importance_header_shapes.json`). Where Exchange departs
    /// from RFC 5322 in reading a value, this follows the RFC (`docs/spec-deviations.md`,
    /// MICROSOFT-1).
    #[must_use]
    pub fn from_headers(headers: ImportanceHeaders<'_>) -> Self {
        if let Some(value) = headers.importance {
            return Self::parse(value).unwrap_or_default();
        }
        if let Some(value) = headers.x_msmail_priority {
            return Self::parse(value).unwrap_or_default();
        }
        headers
            .x_priority
            .and_then(Self::from_x_priority)
            .unwrap_or_default()
    }

    /// The `X-Priority` value written beside `Importance` on an outgoing message, so a client
    /// that reads only `X-Priority` sees it too. `None` for [`Normal`](Self::Normal), which an
    /// outgoing message states by carrying neither header.
    #[must_use]
    pub fn x_priority(self) -> Option<&'static str> {
        match self {
            Self::Low => Some("5 (Lowest)"),
            Self::Normal => None,
            Self::High => Some("1 (Highest)"),
        }
    }

    /// Whether this is [`Normal`](Self::Normal), the importance an absent header states.
    #[must_use]
    pub fn is_normal(self) -> bool {
        self == Self::Normal
    }
}

/// `value` with its RFC 5322 §3.2.2 comments taken out: parenthesised text, nested to any depth,
/// with a backslash quoting the character after it. An unclosed comment runs to the end.
fn without_comments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0_usize;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if depth > 0 => {
                chars.next();
            }
            '(' => depth += 1,
            ')' if depth > 0 => {
                depth -= 1;
                // A comment separates what is either side of it, as whitespace does.
                if depth == 0 {
                    out.push(' ');
                }
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
#[path = "importance_tests.rs"]
mod tests;
