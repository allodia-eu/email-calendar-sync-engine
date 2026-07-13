//! Byte-preserving, fold-aware line surgery over a raw iCalendar document.
//!
//! This is the machinery both write paths that *edit an existing resource* are built
//! on — the [`patch`](super::patch) structural patcher and, through it, the
//! [`imip`](crate::imip) RSVP primitive. Its one job is the invariant those paths
//! depend on: **a line nobody edited is re-emitted byte-for-byte**, including its
//! original folding, its `\r\n` or bare `\n` terminator, and a missing terminator on
//! the final line.
//!
//! That rules out the obvious implementation. Parsing to a model and re-serializing
//! would silently rewrite every property the model cannot express — the `RRULE`, the
//! `VALARM`s, the `X-` properties, the embedded `VTIMEZONE` — which is data loss that
//! looks like a successful save (`calendar-semantics.md`: "provider writes round-trip
//! from raw plus targeted patches, never by re-serializing the lossy projection").
//! So a [`Document`] keeps borrowed slices of the original text and only the edited
//! *groups* are re-rendered.
//!
//! The unit of editing is the **logical** line (a *group* of physical lines): RFC 5545
//! §3.1 folds a content line longer than 75 octets across physical lines continued by a
//! leading space or tab, so a naive line-based find-and-replace corrupts any long
//! `DESCRIPTION` or `ATTENDEE`. A group is unfolded to be read and re-folded to be
//! written.

use core::ops::Range;
use std::collections::BTreeMap;

use super::format::fold_line;

/// What happens to one logical line when the document is rendered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum LineEdit {
    /// Re-emit the group's physical lines verbatim (the default for every group).
    #[default]
    Keep,
    /// Replace the logical line with this text, re-folded on write.
    Replace(String),
    /// Drop the logical line (and every physical line it folds across).
    Remove,
}

/// An edit applied to one logical line: text spliced in before it, and what becomes
/// of the line itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Edit {
    /// Raw text emitted immediately before the group — already folded and terminated
    /// (build it with [`Document::fold`], or splice a whole rendered component).
    pub before: String,
    /// What to do with the group's own line.
    pub line: LineEdit,
}

impl Edit {
    /// An edit that replaces the logical line with `text`.
    pub(crate) fn replace(text: impl Into<String>) -> Self {
        Self {
            before: String::new(),
            line: LineEdit::Replace(text.into()),
        }
    }

    /// An edit that splices `text` in before the (otherwise untouched) line.
    pub(crate) fn insert_before(text: impl Into<String>) -> Self {
        Self {
            before: text.into(),
            line: LineEdit::Keep,
        }
    }
}

/// The edits to apply to a document, keyed by logical-line index. `BTreeMap` so the
/// render walks them in document order.
pub(crate) type Edits = BTreeMap<usize, Edit>;

/// A raw iCalendar document split into physical lines and the logical groups they
/// fold into, holding borrowed slices of the original text so untouched lines
/// re-emit byte-for-byte.
#[derive(Debug)]
pub(crate) struct Document<'a> {
    /// Each physical line as `(content_without_terminator, terminator)`; the
    /// terminator is `""` only for an unterminated final line.
    physical: Vec<(&'a str, &'a str)>,
    /// The physical-line range each logical (unfolded) content line spans.
    groups: Vec<Range<usize>>,
}

impl<'a> Document<'a> {
    /// Splits `raw` into physical lines and folds them into logical groups.
    pub(crate) fn parse(raw: &'a str) -> Self {
        let physical = physical_lines(raw);
        let groups = logical_groups(&physical);
        Self { physical, groups }
    }

    /// The number of logical lines.
    pub(crate) fn len(&self) -> usize {
        self.groups.len()
    }

    /// The logical (unfolded) content line at `index`, with each continuation's one
    /// leading space or tab consumed.
    pub(crate) fn logical(&self, index: usize) -> String {
        let mut out = String::new();
        for (offset, &(content, _)) in self.physical[self.groups[index].clone()].iter().enumerate()
        {
            // The first physical line is whole; every continuation drops its single
            // leading fold character (RFC 5545 §3.1).
            if offset == 0 {
                out.push_str(content);
            } else {
                out.push_str(&content[1..]);
            }
        }
        out
    }

    /// The line terminator of the logical line at `index`, falling back to CRLF when
    /// the document's final line carries none (RFC 5545 mandates CRLF; a written line
    /// gets one even if the source was truncated).
    pub(crate) fn terminator(&self, index: usize) -> &'a str {
        let term = self.physical[self.groups[index].end - 1].1;
        if term.is_empty() { "\r\n" } else { term }
    }

    /// Folds `line` for insertion into this document, using the terminator in use at
    /// `index` (so a bare-LF document stays bare-LF). Includes the trailing
    /// terminator.
    pub(crate) fn fold(&self, index: usize, line: &str) -> String {
        fold_line(line, self.terminator(index))
    }

    /// Renders the whole document with `edits` applied.
    pub(crate) fn render(&self, edits: &Edits) -> String {
        self.render_range(0..self.len(), edits)
    }

    /// Renders the logical lines in `groups` with `edits` applied (keyed by absolute
    /// logical-line index). Every group not named in `edits` is copied from the source
    /// byte-for-byte — the invariant this whole module exists for.
    pub(crate) fn render_range(&self, groups: Range<usize>, edits: &Edits) -> String {
        let mut out = String::with_capacity(self.source_len());
        for index in groups {
            let edit = edits.get(&index);
            if let Some(edit) = edit {
                out.push_str(&edit.before);
            }
            match edit.map(|edit| &edit.line) {
                None | Some(LineEdit::Keep) => {
                    for &(content, term) in &self.physical[self.groups[index].clone()] {
                        out.push_str(content);
                        out.push_str(term);
                    }
                }
                Some(LineEdit::Replace(text)) => {
                    out.push_str(&self.fold(index, text));
                }
                Some(LineEdit::Remove) => {}
            }
        }
        out
    }

    /// A capacity hint: the total source length.
    fn source_len(&self) -> usize {
        self.physical
            .iter()
            .map(|(content, term)| content.len() + term.len())
            .sum()
    }
}

/// Splits `raw` into physical lines as `(content_without_terminator, terminator)`,
/// preserving each original `\r\n`/`\n` (or `""` for an unterminated final line) so
/// untouched lines re-emit byte-for-byte.
fn physical_lines(raw: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut start = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let content_end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            out.push((&raw[start..content_end], &raw[content_end..=i]));
            start = i + 1;
        }
    }
    if start < raw.len() {
        out.push((&raw[start..], ""));
    }
    out
}

/// Groups physical lines into logical content lines, attaching each folded
/// continuation (a line beginning with a space or tab, RFC 5545 §3.1) to its
/// predecessor.
fn logical_groups(physical: &[(&str, &str)]) -> Vec<Range<usize>> {
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i < physical.len() {
        let start = i;
        i += 1;
        while i < physical.len() && physical[i].0.starts_with([' ', '\t']) {
            i += 1;
        }
        groups.push(start..i);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str =
        "BEGIN:VCALENDAR\r\nSUMMARY:hello\r\nDESCRIPTION:one\r\n  two\r\nEND:VCALENDAR\r\n";

    #[test]
    fn an_empty_edit_set_reproduces_the_source_byte_for_byte() {
        // The load-bearing invariant. Folded lines, CRLF terminators, everything.
        let doc = Document::parse(DOC);
        assert_eq!(doc.render(&Edits::new()), DOC);
    }

    #[test]
    fn a_folded_line_is_one_logical_group() {
        let doc = Document::parse(DOC);
        assert_eq!(doc.len(), 4); // not 5 — the continuation folds into DESCRIPTION
        // The continuation's single leading space is consumed; the second is content.
        assert_eq!(doc.logical(2), "DESCRIPTION:one two");
    }

    #[test]
    fn replacing_a_folded_line_rewrites_only_that_group() {
        let doc = Document::parse(DOC);
        let mut edits = Edits::new();
        edits.insert(2, Edit::replace("DESCRIPTION:new"));
        assert_eq!(
            doc.render(&edits),
            "BEGIN:VCALENDAR\r\nSUMMARY:hello\r\nDESCRIPTION:new\r\nEND:VCALENDAR\r\n"
        );
    }

    #[test]
    fn a_replacement_over_seventy_five_octets_is_re_folded() {
        let doc = Document::parse(DOC);
        let mut edits = Edits::new();
        edits.insert(1, Edit::replace(format!("SUMMARY:{}", "x".repeat(120))));
        let rendered = doc.render(&edits);
        for line in rendered.split("\r\n") {
            assert!(line.len() <= 75, "unfolded line written: {line:?}");
        }
        // And it re-reads as one logical line with the full value.
        let reparsed = Document::parse(&rendered);
        assert_eq!(reparsed.logical(1), format!("SUMMARY:{}", "x".repeat(120)));
    }

    #[test]
    fn removing_and_inserting_leave_the_rest_untouched() {
        let doc = Document::parse(DOC);
        let mut edits = Edits::new();
        edits.entry(1).or_default().line = LineEdit::Remove;
        edits.insert(3, Edit::insert_before("X-NEW:v\r\n".to_owned()));
        assert_eq!(
            doc.render(&edits),
            "BEGIN:VCALENDAR\r\nDESCRIPTION:one\r\n  two\r\nX-NEW:v\r\nEND:VCALENDAR\r\n"
        );
    }

    #[test]
    fn a_bare_lf_document_keeps_bare_lf_and_an_unterminated_last_line() {
        // Terminators are per-line, not per-document: an edited line inherits its own.
        let raw = "BEGIN:VCALENDAR\nSUMMARY:hi\nEND:VCALENDAR";
        let doc = Document::parse(raw);
        assert_eq!(doc.render(&Edits::new()), raw);
        assert_eq!(doc.terminator(1), "\n");
        // The final line has no terminator; a *written* line still gets one (CRLF).
        assert_eq!(doc.terminator(2), "\r\n");

        let mut edits = Edits::new();
        edits.insert(1, Edit::replace("SUMMARY:bye"));
        assert_eq!(
            doc.render(&edits),
            "BEGIN:VCALENDAR\nSUMMARY:bye\nEND:VCALENDAR"
        );
    }

    #[test]
    fn rendering_a_range_with_no_edits_returns_it_verbatim() {
        let doc = Document::parse(DOC);
        assert_eq!(
            doc.render_range(1..3, &Edits::new()),
            "SUMMARY:hello\r\nDESCRIPTION:one\r\n  two\r\n"
        );
    }

    #[test]
    fn an_empty_document_renders_empty() {
        let doc = Document::parse("");
        assert_eq!(doc.len(), 0);
        assert_eq!(doc.render(&Edits::new()), "");
    }
}
