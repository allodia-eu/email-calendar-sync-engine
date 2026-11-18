//! How important the sender marked a message.

use serde::{Deserialize, Serialize};

/// The importance a message's sender gave it: the `Importance` header of RFC 2156 §5.3,
/// registered by RFC 4021 §2.1.52 (`low` / `normal` / `high`).
///
/// It is content, not state: the sender wrote it, and nothing the recipient does moves it.
///
/// A message that says nothing is [`Normal`](Self::Normal), which is what the absence of the
/// header means; that is the RFC's default, not a value the engine invents. Many clients write
/// only the non-standard `X-Priority` (Thunderbird and Apple Mail among them), so
/// [`from_headers`](Self::from_headers) reads it when `Importance` is absent or unreadable.
/// RFC 2156's `Priority` is the delivery priority an MTA acts on, a different property, and is
/// not read.
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

impl Importance {
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

    /// Reads an RFC 2156 `Importance` value (or Graph's `importance`), ignoring case and the
    /// surrounding whitespace a folded header leaves. `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        [Self::Low, Self::Normal, Self::High]
            .into_iter()
            .find(|importance| value.eq_ignore_ascii_case(importance.as_str()))
    }

    /// Reads an `X-Priority` value: its leading digit, `1` the highest and `5` the lowest, with
    /// any comment after it (`1 (Highest)`) ignored. `1` and `2` are high, `3` is normal, `4` and
    /// `5` are low, which is how the clients that write it read it back. `None` for anything else.
    #[must_use]
    pub fn from_x_priority(value: &str) -> Option<Self> {
        let digit = value.trim_start().chars().next()?;
        let rest = value.trim_start()[digit.len_utf8()..].chars().next();
        if rest.is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        match digit {
            '1' | '2' => Some(Self::High),
            '3' => Some(Self::Normal),
            '4' | '5' => Some(Self::Low),
            _ => None,
        }
    }

    /// The importance a message's headers state: its `Importance` when that is readable, else its
    /// `X-Priority`, else [`Normal`](Self::Normal). Each argument is the header's unfolded value,
    /// `None` when the message has no such header.
    #[must_use]
    pub fn from_headers(importance: Option<&str>, x_priority: Option<&str>) -> Self {
        importance
            .and_then(Self::parse)
            .or_else(|| x_priority.and_then(Self::from_x_priority))
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

#[cfg(test)]
#[path = "importance_tests.rs"]
mod tests;
