//! What a mechanism tells the walk: which MIME parts are its layers.

use core::{fmt, ops::Range};

use engine_core::e2e::{LayerKind, Mechanism, SignatureForm};
use mail_parser::{Message, MessagePart, MimeHeaders, PartType};

/// Recognises one mechanism's cryptographic layers in a MIME tree.
///
/// The walk asks every registered recogniser about a part, outermost first, and
/// takes the first answer. A recogniser looks only at structure (content types,
/// parameters, children); it never decrypts or verifies anything.
pub trait LayerRecogniser {
    /// The mechanism this recogniser speaks for.
    fn mechanism(&self) -> Mechanism;

    /// Says whether `part` is one of this mechanism's layers.
    ///
    /// Returns `None` for a part that is not, which is almost every part.
    fn recognise(&self, part: &PartView<'_>) -> Option<Recognition>;

    /// Says whether a text part carries this mechanism's protection inline, outside
    /// the MIME structure (RFC 9787 §6.2.3).
    fn inline(&self, text: &str) -> Option<InlineKind>;
}

/// A recogniser's answer about one part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recognition {
    /// The part is a layer of this shape.
    Layer(LayerShape),
    /// The part announces itself as a layer and breaks that layer's structure. It
    /// is treated as an ordinary part, and recorded.
    Malformed,
}

/// How a layer holds its protected part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerShape {
    /// `multipart/signed` (RFC 1847): exactly two children, the protected part and
    /// then the signature. The walk checks the count.
    DetachedSignature,
    /// A signature object that wraps its protected part, read by unwrapping it
    /// (S/MIME `signed-data`).
    EmbeddedSignature(Object),
    /// An encrypted object that decrypts to the protected part (PGP/MIME
    /// `multipart/encrypted`, S/MIME `enveloped-data` and `authEnveloped-data`).
    Encryption(Object),
}

impl LayerShape {
    /// Returns what a layer of this shape does, as far as its structure says. An
    /// encryption layer may turn out to be signed inside once opened.
    pub fn kind(self) -> LayerKind {
        match self {
            Self::DetachedSignature => LayerKind::Signed(SignatureForm::Detached),
            Self::EmbeddedSignature(_) => LayerKind::Signed(SignatureForm::Embedded),
            Self::Encryption(_) => LayerKind::Encrypted,
        }
    }
}

/// Where a sealed layer keeps the object its mechanism opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// The layer's own body (S/MIME `application/pkcs7-mime`).
    Body,
    /// The body of the child at this index (PGP/MIME: the `application/octet-stream`
    /// part, child 1).
    Child(usize),
}

/// Protection found inline in a text part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InlineKind {
    /// Inline encrypted content.
    Encrypted,
    /// An inline signature over text.
    Signed,
}

/// A read-only view of one MIME part, as a recogniser sees it.
#[derive(Clone, Copy)]
pub struct PartView<'a> {
    message: &'a Message<'a>,
    part: &'a MessagePart<'a>,
}

impl<'a> PartView<'a> {
    pub(crate) fn root(message: &'a Message<'a>) -> Option<Self> {
        Some(Self {
            message,
            part: message.parts.first()?,
        })
    }

    /// Whether the part's media type is `media_type/subtype`, compared without regard
    /// to case. A part with no `Content-Type` is `text/plain` (RFC 2045 §5.2).
    pub fn is(&self, media_type: &str, subtype: &str) -> bool {
        match self.part.content_type() {
            Some(content_type) => {
                content_type.ctype().eq_ignore_ascii_case(media_type)
                    && content_type
                        .subtype()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(subtype))
            }
            None => {
                media_type.eq_ignore_ascii_case("text") && subtype.eq_ignore_ascii_case("plain")
            }
        }
    }

    /// Returns a `Content-Type` parameter, its name compared without regard to case.
    pub fn param(&self, name: &str) -> Option<&'a str> {
        self.part
            .content_type()?
            .attributes()?
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .map(|attribute| attribute.value.as_ref())
    }

    /// Returns the part's children, in order; none for a leaf.
    pub fn children(&self) -> impl Iterator<Item = PartView<'a>> + use<'a> {
        let message = self.message;
        self.part
            .sub_parts()
            .unwrap_or_default()
            .iter()
            .filter_map(move |id| {
                Some(Self {
                    message,
                    part: message.part(*id)?,
                })
            })
    }

    /// Returns the child at `index`.
    pub fn child(&self, index: usize) -> Option<PartView<'a>> {
        self.children().nth(index)
    }

    /// The part's bytes, headers included, within its source. The root is the whole
    /// source.
    pub(crate) fn range(&self) -> Range<usize> {
        let source = self.message.raw_message.len();
        if self
            .message
            .parts
            .first()
            .is_some_and(|root| core::ptr::eq(root, self.part))
        {
            return 0..source;
        }
        let end = (self.part.offset_end as usize).min(source);
        (self.part.offset_header as usize).min(end)..end
    }

    /// The part's body, content-transfer-decoded.
    pub(crate) fn contents(&self) -> &'a [u8] {
        self.part.contents()
    }

    /// Whether the part is an encapsulated message (`message/rfc822`).
    pub(crate) fn is_message(&self) -> bool {
        matches!(self.part.body, PartType::Message(_))
    }

    /// The decoded text of a `text/plain` leaf.
    pub(crate) fn plain_text(&self) -> Option<&'a str> {
        match &self.part.body {
            PartType::Text(text) if self.is("text", "plain") => Some(text),
            _ => None,
        }
    }
}

impl fmt::Debug for PartView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let content_type = self.part.content_type();
        f.debug_struct("PartView")
            .field("type", &content_type.map(mail_parser::ContentType::ctype))
            .field("subtype", &content_type.and_then(|c| c.subtype()))
            .finish_non_exhaustive()
    }
}

/// Asks each recogniser in turn about `part`.
pub(crate) fn recognise(
    recognisers: &[&dyn LayerRecogniser],
    part: &PartView<'_>,
) -> Option<(Mechanism, Recognition)> {
    recognisers.iter().find_map(|recogniser| {
        recogniser
            .recognise(part)
            .map(|recognition| (recogniser.mechanism(), recognition))
    })
}
