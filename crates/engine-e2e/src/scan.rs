//! The payload's subtree: errant layers, inline protection, malformed layers.

use crate::{
    LayerRecogniser,
    envelope::{ErrantLayer, InlineFinding, MalformedLayer, PartRef, SourceId},
    recognise::{PartView, Recognition, recognise},
    walk::{Checked, check},
};

/// What scanning a payload found.
#[derive(Debug, Default)]
pub(crate) struct Findings {
    pub(crate) errant: Vec<ErrantLayer>,
    pub(crate) inline: Vec<InlineFinding>,
    pub(crate) malformed: Vec<MalformedLayer>,
}

/// Scans the payload rooted at `root`, depth first in document order.
///
/// The root is the payload, so it is never itself errant. An encapsulated message
/// is not entered: it is a message of its own, with its own envelope. An errant
/// layer is not entered either: its subtree is shown in isolation, and walking it
/// is a separate question.
pub(crate) fn scan(
    recognisers: &[&dyn LayerRecogniser],
    source: SourceId,
    root: PartView<'_>,
    root_path: &[usize],
) -> Findings {
    let mut findings = Findings::default();
    let mut stack = vec![(root, root_path.to_vec(), true)];
    while let Some((part, path, is_root)) = stack.pop() {
        if part.is_message() {
            continue;
        }
        let here = || PartRef::new(source, &path, part.range());
        if !is_root && let Some((mechanism, recognition)) = recognise(recognisers, &part) {
            match recognition {
                Recognition::Layer(shape) => match check(part, shape) {
                    Checked::Detached { .. } | Checked::Sealed { .. } => {
                        findings.errant.push(ErrantLayer {
                            mechanism,
                            kind: shape.kind(),
                            part: here(),
                        });
                        continue;
                    }
                    Checked::Malformed => findings.malformed.push(MalformedLayer {
                        mechanism,
                        part: here(),
                    }),
                },
                Recognition::Malformed => findings.malformed.push(MalformedLayer {
                    mechanism,
                    part: here(),
                }),
            }
        }
        if let Some(text) = part.plain_text() {
            let found = recognisers
                .iter()
                .find_map(|r| r.inline(text).map(|kind| (r.mechanism(), kind)));
            if let Some((mechanism, kind)) = found {
                findings.inline.push(InlineFinding {
                    mechanism,
                    kind,
                    part: here(),
                });
            }
        }
        let children: Vec<_> = part.children().collect();
        for (index, child) in children.into_iter().enumerate().rev() {
            let mut child_path = path.clone();
            child_path.push(index);
            stack.push((child, child_path, false));
        }
    }
    findings
}
