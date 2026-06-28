//! Pure parsers for the CONDSTORE/QRESYNC negotiation responses (RFC 7162).
//!
//! Split out of [`crate::parse`] (which owns the metadata-response grammar:
//! `SELECT`/`FETCH`/`ENVELOPE`/`LIST`/`SEARCH`) to keep both files focused: this one
//! reads the two responses the QRESYNC path adds — a `CAPABILITY` list (to detect
//! whether the server offers QRESYNC) and a `* VANISHED` set (the UIDs an incremental
//! delta expunges). Both are pure and panic-resistant on hostile input, like the rest
//! of the parse layer. (`HIGHESTMODSEQ` itself is just a `SELECT` response code, so it
//! stays in [`crate::parse`] alongside the other `SELECT` fields.)

/// Reads a `CAPABILITY` response's advertised capability atoms (RFC 9051 §7.2.1),
/// from either an untagged `* CAPABILITY …` line or a tagged-completion
/// `[CAPABILITY …]` response code line the transport forwarded. Used to detect
/// QRESYNC (RFC 7162) before enabling it. Non-capability lines are skipped.
pub(crate) fn parse_capabilities(lines: &[Vec<u8>]) -> Vec<String> {
    for line in lines {
        let text = String::from_utf8_lossy(line);
        // The untagged form leads with `CAPABILITY` (the transport already stripped
        // the `* `); accept it anywhere on the line so a `[CAPABILITY …]` code is
        // also read.
        if let Some(after) = capability_atoms(&text) {
            return after;
        }
    }
    Vec::new()
}

/// The capability atoms in a line, if it carries a `CAPABILITY` list (untagged
/// `CAPABILITY a b c` or a bracketed `[CAPABILITY a b c]` code). The atoms run to the
/// end of the line, or to the `]` that closes a response code.
fn capability_atoms(text: &str) -> Option<Vec<String>> {
    let after = text.strip_prefix("CAPABILITY ").or_else(|| {
        text.find("[CAPABILITY ")
            .map(|at| &text[at + "[CAPABILITY ".len()..])
    })?;
    let list = after.split(']').next().unwrap_or(after);
    Some(list.split_whitespace().map(str::to_owned).collect())
}

/// Whether an `ENABLE` response's `* ENABLED <caps>` line actually lists QRESYNC
/// (RFC 5161 §3.2, RFC 7162 §3.1). A server may answer a tagged `OK` to `ENABLE
/// QRESYNC` while listing nothing (or not QRESYNC) in `* ENABLED`, meaning the
/// extension was **not** enabled — issuing `CHANGEDSINCE`/`VANISHED` would then be
/// illegal, so the caller must stay on the non-QRESYNC baseline.
pub(crate) fn enabled_lists_qresync(lines: &[Vec<u8>]) -> bool {
    lines.iter().any(|line| {
        let text = String::from_utf8_lossy(line);
        let mut tokens = text.split_whitespace();
        tokens
            .next()
            .is_some_and(|head| head.eq_ignore_ascii_case("ENABLED"))
            && tokens.any(|cap| cap.eq_ignore_ascii_case("QRESYNC"))
    })
}

/// Reads `* VANISHED [(EARLIER)] <uid-set>` responses (RFC 7162 §3.2.10) into the
/// expanded list of expunged UIDs. Both forms (the synchronous `VANISHED` and the
/// `CHANGEDSINCE`/QRESYNC `VANISHED (EARLIER)`) remove the named UIDs locally, so the
/// optional `(EARLIER)` marker is ignored. The sequence set is expanded
/// (`1:3,5` → 1,2,3,5); expansion is bounded by [`MAX_VANISHED`] so an adversarial
/// range cannot drive an unbounded allocation (`north-star.md` security).
pub(crate) fn parse_vanished(lines: &[Vec<u8>]) -> Vec<u32> {
    let mut out = Vec::new();
    for line in lines {
        let text = String::from_utf8_lossy(line);
        let mut tokens = text.split_whitespace();
        if !tokens
            .next()
            .is_some_and(|head| head.eq_ignore_ascii_case("VANISHED"))
        {
            continue;
        }
        // The set is the first token that is not a `(…)` modifier list (`(EARLIER)`).
        if let Some(set) = tokens.find(|token| !token.starts_with('(')) {
            expand_uid_set(set, &mut out);
        }
    }
    out
}

/// The most UIDs a single `VANISHED` response is allowed to expand to — generous for
/// any real delta (an entire renumbered folder is a snapshot, not a VANISHED list),
/// but a hard ceiling so a hostile `1:4294967295` cannot exhaust memory.
const MAX_VANISHED: usize = 1 << 20;

/// Expands one IMAP sequence-set (`5,7,10:12`) into `out`, capped at
/// [`MAX_VANISHED`] total. A bare UID is a one-element range; an unparseable part
/// (a non-numeric endpoint, an empty token) is skipped. The single cap check inside
/// the push loop bounds both a hostile range and a hostile comma-list.
fn expand_uid_set(set: &str, out: &mut Vec<u32>) {
    for part in set.split(',') {
        let (lo, hi) = match part.split_once(':') {
            Some((lo, hi)) => match (lo.parse::<u32>(), hi.parse::<u32>()) {
                (Ok(lo), Ok(hi)) => (lo.min(hi), lo.max(hi)),
                _ => continue,
            },
            None => match part.parse::<u32>() {
                Ok(uid) => (uid, uid),
                Err(_) => continue,
            },
        };
        for uid in lo..=hi {
            if out.len() >= MAX_VANISHED {
                return;
            }
            out.push(uid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the `Vec<Vec<u8>>` shape the transport hands the parsers (each entry is
    /// one untagged response body, `* ` already stripped).
    fn lines(strs: &[&str]) -> Vec<Vec<u8>> {
        strs.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn capabilities_are_read_from_untagged_or_a_response_code() {
        // The untagged `* CAPABILITY …` form (the transport stripped the `* `).
        let untagged = parse_capabilities(&lines(&[
            "CAPABILITY IMAP4rev2 ENABLE CONDSTORE QRESYNC UIDPLUS",
        ]));
        assert!(untagged.iter().any(|c| c == "QRESYNC"));
        assert!(untagged.iter().any(|c| c == "CONDSTORE"));

        // The `[CAPABILITY …]` response-code form a tagged completion carries; atoms
        // stop at the closing `]`.
        let coded = parse_capabilities(&lines(&[
            "OK [CAPABILITY IMAP4rev2 QRESYNC] Authentication successful",
        ]));
        assert_eq!(coded, ["IMAP4rev2", "QRESYNC"]);

        // A server without QRESYNC.
        let plain = parse_capabilities(&lines(&["CAPABILITY IMAP4rev2 IDLE UIDPLUS"]));
        assert!(!plain.iter().any(|c| c.eq_ignore_ascii_case("QRESYNC")));

        // No CAPABILITY line at all → no atoms (treated as no QRESYNC, the fallback).
        assert!(parse_capabilities(&lines(&["OK nothing to see here"])).is_empty());
    }

    #[test]
    fn enabled_is_recognized_only_when_it_actually_lists_qresync() {
        assert!(enabled_lists_qresync(&lines(&["ENABLED QRESYNC"])));
        assert!(enabled_lists_qresync(&lines(&[
            "ENABLED CONDSTORE QRESYNC"
        ])));
        // A bare `* ENABLED` (server enabled nothing) must NOT be treated as success.
        assert!(!enabled_lists_qresync(&lines(&["ENABLED"])));
        assert!(!enabled_lists_qresync(&lines(&["ENABLED CONDSTORE"])));
        // A non-ENABLED line is ignored.
        assert!(!enabled_lists_qresync(&lines(&["OK done"])));
    }

    #[test]
    fn vanished_parses_both_forms_and_expands_ranges() {
        // The `CHANGEDSINCE` form carries `(EARLIER)`; the synchronous form does not.
        assert_eq!(parse_vanished(&lines(&["VANISHED (EARLIER) 7"])), [7]);
        assert_eq!(parse_vanished(&lines(&["VANISHED 41"])), [41]);
        // A sequence set expands every UID (each is its own removal key).
        assert_eq!(
            parse_vanished(&lines(&["VANISHED (EARLIER) 3:5,9,12:13"])),
            [3, 4, 5, 9, 12, 13]
        );
        // Non-VANISHED lines are ignored.
        assert!(parse_vanished(&lines(&["2 FETCH (UID 2 FLAGS ())"])).is_empty());
    }

    #[test]
    fn vanished_skips_unparseable_parts() {
        // A non-numeric range endpoint or bare token is dropped; the valid UIDs survive.
        assert_eq!(
            parse_vanished(&lines(&["VANISHED (EARLIER) 3:x,bad,5,7:8"])),
            [5, 7, 8]
        );
        // An empty set (just the marker, no UIDs) yields nothing, never panics.
        assert!(parse_vanished(&lines(&["VANISHED (EARLIER)"])).is_empty());
    }

    #[test]
    fn a_hostile_vanished_range_cannot_exhaust_memory() {
        // An adversarial `1:4294967295` must be bounded, not expanded to 4 billion UIDs.
        let huge = parse_vanished(&lines(&["VANISHED (EARLIER) 1:4294967295"]));
        assert_eq!(
            huge.len(),
            MAX_VANISHED,
            "expansion is capped at MAX_VANISHED"
        );
    }
}
