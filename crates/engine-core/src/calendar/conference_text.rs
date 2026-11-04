//! The block of text an event's notes carry for a meeting stored where an invitee's calendar
//! may not look.
//!
//! iCalendar `CONFERENCE` and JSCalendar `virtualLocations` are invisible to Outlook, Google
//! Calendar and Thunderbird, and Graph has no field for a meeting it did not make, so a meeting
//! put on such an event also goes into its notes: a rule, the meeting's name, `Join: <link>`,
//! its ID and passcode, each dial-in number with the code a caller enters after dialling, and
//! the rule again. The rule is fixed ASCII and the
//! same in every language, so [`super::conference_block`] recognises the block whoever wrote
//! the words.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::{
    Conference, EntryKind, EntryPoint, conference_block::without_blocks, conference_detect::links,
};

/// The line above and below the block. No Markdown reader takes it for a heading or a fence.
pub(super) const RULE: &str = "-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=";

/// The words of the block, in the language the event is written in. The engine has no
/// locale, so the caller supplies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceText {
    /// The heading for a meeting without a name of its own ("Video meeting").
    pub meeting: String,
    /// Before the join link ("Join").
    pub join: String,
    /// Before the meeting ID ("Meeting ID").
    pub meeting_id: String,
    /// Before the passcode ("Passcode").
    pub passcode: String,
    /// Before each dial-in number ("Dial in").
    pub dial_in: String,
    /// Before the code a caller enters after dialling ("Conference ID"). Empty in a write
    /// queued before the word existed, which then leaves the codes out as it did.
    #[serde(default)]
    pub dial_in_code: String,
}

impl Conference {
    /// The block the notes carry for this meeting, or `None` without a join link: a block
    /// with nothing to follow is no way in.
    #[must_use]
    pub fn text_block(&self, words: &ConferenceText) -> Option<String> {
        let join = self
            .entry_points
            .iter()
            .find(|entry| entry.kind == EntryKind::Video)?;
        let mut lines = vec![
            RULE.to_owned(),
            self.label.clone().unwrap_or_else(|| words.meeting.clone()),
            format!("{}: {}", words.join, join.uri),
        ];
        if let Some(id) = &self.meeting_id {
            lines.push(format!("{}: {id}", words.meeting_id));
        }
        let passcode = join
            .access
            .passcode
            .as_ref()
            .or(join.access.password.as_ref());
        if let Some(passcode) = passcode {
            lines.push(format!("{}: {passcode}", words.passcode));
        }
        let dial_ins: Vec<(&str, Option<&str>)> = self
            .entry_points
            .iter()
            .filter_map(|entry| {
                let code = dial_in_code(entry).filter(|_| !words.dial_in_code.is_empty());
                Some((entry.phone_number()?, code))
            })
            .collect();
        // One code every number shares (a Teams phone conference ID) is said once, after them.
        let shared = match dial_ins.split_first() {
            Some(((_, Some(code)), rest))
                if rest.iter().all(|(_, other)| other == &Some(*code)) =>
            {
                Some(*code)
            }
            _ => None,
        };
        for (number, code) in &dial_ins {
            match code {
                Some(code) if shared.is_none() => lines.push(format!(
                    "{}: {number} ({}: {code})",
                    words.dial_in, words.dial_in_code
                )),
                _ => lines.push(format!("{}: {number}", words.dial_in)),
            }
        }
        if let Some(code) = shared {
            lines.push(format!("{}: {code}", words.dial_in_code));
        }
        lines.push(RULE.to_owned());
        Some(lines.join("\n"))
    }
}

/// The code a caller enters after dialling `entry`: its access code, or its PIN.
fn dial_in_code(entry: &EntryPoint) -> Option<&str> {
    let access = &entry.access;
    access.access_code.as_deref().or(access.pin.as_deref())
}

/// `notes` with `block` after them, a blank line between.
#[must_use]
pub fn with_text_block(notes: &str, block: &str) -> String {
    let notes = notes.trim_end();
    if notes.is_empty() {
        block.to_owned()
    } else {
        format!("{notes}\n\n{block}")
    }
}

/// `text` without each meeting block, in any of the shapes [`crate::calendar::Event::notes`]
/// recognises, that holds a link joining `conference` ([`Conference::joined_at`]). What a
/// removed meeting leaves in the notes.
#[must_use]
pub fn without_text_block<'a>(text: &'a str, conference: &Conference) -> Cow<'a, str> {
    without_blocks(text, |block| {
        links(block).any(|uri| conference.joined_at(&uri))
    })
}

#[cfg(test)]
#[path = "conference_text_tests.rs"]
mod tests;
