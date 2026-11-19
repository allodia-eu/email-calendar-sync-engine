//! Gmail's `X-GM-EXT-1` fetch items, read out of a `FETCH` response
//! (<https://developers.google.com/workspace/gmail/imap/imap-extensions>).
//!
//! `X-GM-MSGID` and `X-GM-THRID` are unsigned 64-bit decimals, the same for a message in every
//! folder it appears in; written in hex they are the Gmail API's message and thread ids.
//! `X-GM-LABELS` is a list of astrings: a system label as a backslash name (`"\\Inbox"`,
//! quoted on the wire), a label of the user's as its name, encoded the way the session encodes
//! mailbox names.

use crate::tokenize::Item;

/// The `X-GM-EXT-1` items of one `FETCH` row; each `None` when it was not asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GmailItems {
    /// `X-GM-MSGID`.
    pub message: Option<u64>,
    /// `X-GM-THRID`.
    pub thread: Option<u64>,
    /// `X-GM-LABELS`, each as the wire gave it (not yet decoded from modified UTF-7).
    pub labels: Option<Vec<String>>,
}

impl GmailItems {
    /// Takes `value` if `key` (upper-cased) is one of Gmail's items; ignores anything else.
    pub(crate) fn read(&mut self, key: &str, value: &Item) {
        match key {
            "X-GM-MSGID" => self.message = number(value),
            "X-GM-THRID" => self.thread = number(value),
            "X-GM-LABELS" => {
                self.labels = value
                    .as_list()
                    .map(|labels| labels.iter().filter_map(Item::as_nstring).collect());
            }
            _ => {}
        }
    }
}

/// A number given bare (`X-GM-MSGID 123`) or as a one-element list (`MODSEQ (123)`).
pub(crate) fn single_number(value: &Item) -> Option<u64> {
    match value.as_list() {
        Some([only]) => number(only),
        Some(_) => None,
        None => number(value),
    }
}

fn number(value: &Item) -> Option<u64> {
    value.as_atom()?.parse().ok()
}

#[cfg(test)]
#[path = "parse_gmail_tests.rs"]
mod tests;
