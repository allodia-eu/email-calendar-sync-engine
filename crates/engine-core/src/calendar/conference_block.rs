//! Recognising the meeting blocks calendar services write into a description.
//!
//! An invitation's description carries its meeting as a block of text between two rule
//! lines, so that a client reading only the description still finds the link. The block
//! repeats the conference, so [`super::Event::notes`] leaves it out. Two shapes are
//! recognised, both from live invitations:
//!
//! - **Microsoft Teams**: a line of underscores, a first line naming "Microsoft Teams" (the brand
//!   is the same in every language the invitation is written in), and a closing line of
//!   underscores.
//! - **Google Calendar**: a line `-::~:~::~:~…::-` above the block and the same line below.
//!
//! A block whose closing line is missing runs to the end of the text, because a server's
//! preview of the description (Graph's `bodyPreview`) cuts it off.

use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Teams,
    Google,
}

impl Rule {
    fn of(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.len() >= 32 && line.bytes().all(|b| b == b'_') {
            Some(Self::Teams)
        } else if line.starts_with("-::~:~::~:~") && line.ends_with("::-") {
            Some(Self::Google)
        } else {
            None
        }
    }
}

/// `text` without the meeting blocks it holds, trimmed, with one blank line where a block
/// stood between two passages.
pub(super) fn without_blocks(text: &str) -> Cow<'_, str> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut passages = vec![String::new()];
    let mut found = false;
    let mut at = 0;
    while at < lines.len() {
        if let Some(len) = block_len(&lines[at..]) {
            found = true;
            passages.push(String::new());
            at += len;
        } else {
            if let Some(passage) = passages.last_mut() {
                passage.push_str(lines[at]);
            }
            at += 1;
        }
    }
    if !found {
        return Cow::Borrowed(text.trim());
    }
    let kept: Vec<&str> = passages
        .iter()
        .map(|passage| passage.trim_start_matches(['\r', '\n']).trim_end())
        .filter(|passage| !passage.is_empty())
        .collect();
    Cow::Owned(kept.join("\n\n"))
}

/// How many of `lines` the block they open with spans, or `None` when they open none.
fn block_len(lines: &[&str]) -> Option<usize> {
    let rule = Rule::of(lines.first()?)?;
    let rest = &lines[1..];
    if rule == Rule::Teams {
        let first = rest.iter().find(|line| !line.trim().is_empty())?;
        if !first.contains("Microsoft Teams") {
            return None;
        }
    }
    let close = rest.iter().position(|line| Rule::of(line) == Some(rule));
    Some(close.map_or(lines.len(), |end| end + 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAMS_RULE: &str =
        "________________________________________________________________________________";
    const GOOGLE_RULE: &str = "-::~:~::~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~::~:~::-";

    #[test]
    fn a_teams_block_is_left_out_and_the_notes_around_it_kept() {
        let text = format!(
            "Agenda: budget\r\n\r\n{TEAMS_RULE}\r\nMicrosoft Teams meeting\r\nJoin on your computer\r\n\
             Click here to join the meeting<https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0>\r\n\
             {TEAMS_RULE}\r\n\r\nBring the figures.\r\n"
        );
        assert_eq!(
            without_blocks(&text),
            "Agenda: budget\n\nBring the figures."
        );
    }

    #[test]
    fn a_google_block_is_left_out_in_any_language() {
        let text = format!(
            "{GOOGLE_RULE}\nDeelnemen met Google Meet: https://meet.google.com/abc-defg-hij\n\n\
             Wijzig dit gedeelte niet.\n{GOOGLE_RULE}\n"
        );
        assert_eq!(without_blocks(&text), "");
    }

    #[test]
    fn a_block_cut_off_by_a_preview_runs_to_the_end() {
        let preview =
            format!("{TEAMS_RULE}\r\nMicrosoft Teams-vergadering\r\nNeem deel vanaf uw computer");
        assert_eq!(without_blocks(&preview), "");
    }

    #[test]
    fn a_rule_that_opens_no_meeting_is_the_persons_own() {
        let text = format!("Minutes\n{TEAMS_RULE}\nAction points\n{TEAMS_RULE}\n");
        assert!(matches!(without_blocks(&text), Cow::Borrowed(_)));
        assert_eq!(without_blocks(&text), text.trim());
        assert_eq!(without_blocks("  Plain notes \n"), "Plain notes");
    }
}
