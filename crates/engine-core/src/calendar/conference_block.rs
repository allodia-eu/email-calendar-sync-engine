//! Recognising the meeting blocks calendar services write into a description.
//!
//! An invitation's description carries its meeting as a block of text, so that a client
//! reading only the description still finds the link. The block repeats the conference, so
//! [`super::Event::notes`] leaves it out when that conference is shown. Three shapes are
//! recognised, all from live invitations. "Microsoft Teams" is the same in every language Teams
//! writes in.
//!
//! - **Microsoft Teams, between rules**: a line of underscores, a first line naming "Microsoft
//!   Teams", and a closing line of underscores at least as long as the opening one; the shorter
//!   rules between the block's own sections (its dial-in numbers) do not close it.
//! - **Microsoft Teams, appended**: a heading naming "Microsoft Teams" ("Microsoft
//!   Teams-vergadering") whose next line holds a Teams meeting link, with no rule above it. Teams
//!   appends this block last, so it runs to the end of the text.
//! - **Google Calendar**: a line `-::~:~::~:~…::-` above the block and the same line below.
//! - **Ours**: the rule [`super::conference_text`] writes, above the block and below it.
//!
//! A block whose closing line is missing runs to the end of the text, because a server's
//! preview of the description (Graph's `bodyPreview`) cuts it off.

use std::borrow::Cow;

use super::{ConferenceService, conference_detect::links};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Teams,
    Google,
    Ours,
}

impl Rule {
    fn of(line: &str) -> Option<Self> {
        let line = line.trim();
        if line == super::conference_text::RULE {
            Some(Self::Ours)
        } else if line.len() >= 32 && line.bytes().all(|b| b == b'_') {
            Some(Self::Teams)
        } else if line.starts_with("-::~:~::~:~") && line.ends_with("::-") {
            Some(Self::Google)
        } else {
            None
        }
    }
}

/// `text` without each meeting block it holds that `repeats`, given the block's text, says
/// repeats a meeting shown elsewhere; trimmed, with one blank line where a block stood between
/// two passages.
pub(super) fn without_blocks(text: &str, repeats: impl Fn(&str) -> bool) -> Cow<'_, str> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut passages = vec![String::new()];
    let mut found = false;
    let mut at = 0;
    while at < lines.len() {
        // A block that is kept is kept whole, so its closing rule is not read as the opening
        // of another.
        let len = block_len(&lines[at..]).unwrap_or(1);
        let block = lines[at..at + len].concat();
        if len > 1 && repeats(&block) {
            found = true;
            passages.push(String::new());
        } else if let Some(passage) = passages.last_mut() {
            passage.push_str(&block);
        }
        at += len;
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
    if opens_appended_teams_block(lines) {
        return Some(lines.len());
    }
    let rule = Rule::of(lines.first()?)?;
    let rest = &lines[1..];
    if rule == Rule::Teams {
        let first = rest.iter().find(|line| !line.trim().is_empty())?;
        if !first.contains("Microsoft Teams") {
            return None;
        }
    }
    // A Teams block separates its own sections with shorter rules, so only a rule as long as
    // the opening one closes it.
    let open = lines[0].trim().len();
    let close = rest
        .iter()
        .position(|line| Rule::of(line) == Some(rule) && line.trim().len() >= open);
    Some(close.map_or(lines.len(), |end| end + 2))
}

/// Whether `lines` open with the heading of an appended Teams block: a line naming
/// "Microsoft Teams" whose next non-blank line holds a Teams meeting link.
fn opens_appended_teams_block(lines: &[&str]) -> bool {
    let Some((heading, rest)) = lines.split_first() else {
        return false;
    };
    heading.contains("Microsoft Teams")
        && rest
            .iter()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|next| {
                links(next)
                    .any(|uri| ConferenceService::recognise(&uri) == Some(ConferenceService::Teams))
            })
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
            without_blocks(&text, |_| true),
            "Agenda: budget\n\nBring the figures."
        );
    }

    #[test]
    fn a_shorter_rule_inside_a_teams_block_does_not_close_it() {
        let section = "_".repeat(32);
        let text = format!(
            "Agenda\n{TEAMS_RULE}\nMicrosoft Teams Need help?\nJoin the meeting now\n{section}\n\
             Dial in by phone\n+31 20 000 0000\n{section}\nFor organizers: Meeting options\n\
             {TEAMS_RULE}\n"
        );
        assert_eq!(without_blocks(&text, |_| true), "Agenda");
    }

    #[test]
    fn an_appended_teams_block_runs_to_the_end_and_a_mention_of_teams_is_kept() {
        // The shape a work tenant writes today, as plain text (Graph's text body).
        let section = "_".repeat(32);
        let text = format!(
            "Bring the figures.\r\n\r\nMicrosoft Teams-vergadering\r\n\
             Deelnemen: https://teams.microsoft.com/meet/123456789?p=abc\r\n\
             Vergadering-id: 123 456 789\r\nWachtwoordcode: abc\r\n\
             Help: https://aka.ms/JoinTeamsMeeting?omkt=nl-NL\r\n{section}\r\n\r\n{section}\r\n\
             Voor organisatoren\r\nOpties voor vergadering: https://teams.microsoft.com/meetingOptions/\r\n"
        );
        assert_eq!(without_blocks(&text, |_| true), "Bring the figures.");
        let mention =
            "We use Microsoft Teams for this.\nSee https://www.microsoft.com/microsoft-teams\n";
        assert!(matches!(
            without_blocks(mention, |_| true),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_google_block_is_left_out_in_any_language() {
        let text = format!(
            "{GOOGLE_RULE}\nDeelnemen met Google Meet: https://meet.google.com/abc-defg-hij\n\n\
             Wijzig dit gedeelte niet.\n{GOOGLE_RULE}\n"
        );
        assert_eq!(without_blocks(&text, |_| true), "");
    }

    #[test]
    fn a_block_cut_off_by_a_preview_runs_to_the_end() {
        let preview =
            format!("{TEAMS_RULE}\r\nMicrosoft Teams-vergadering\r\nNeem deel vanaf uw computer");
        assert_eq!(without_blocks(&preview, |_| true), "");
    }

    #[test]
    fn a_block_that_is_kept_is_kept_whole() {
        let text = format!(
            "Agenda\n{GOOGLE_RULE}\nJoin: https://meet.example.com/a\n{GOOGLE_RULE}\nAfter\n"
        );
        assert_eq!(without_blocks(&text, |_| false), text.trim());
        let teams = format!("{TEAMS_RULE}\nMicrosoft Teams meeting\n{TEAMS_RULE}\n");
        assert_eq!(
            without_blocks(&format!("{text}{teams}"), |block| !block.contains("meet.")),
            text.trim()
        );
    }

    #[test]
    fn a_rule_that_opens_no_meeting_is_the_persons_own() {
        let text = format!("Minutes\n{TEAMS_RULE}\nAction points\n{TEAMS_RULE}\n");
        assert!(matches!(without_blocks(&text, |_| true), Cow::Borrowed(_)));
        assert_eq!(without_blocks(&text, |_| true), text.trim());
        assert_eq!(without_blocks("  Plain notes \n", |_| true), "Plain notes");
    }
}
