//! Internal helper macros.

/// Defines an **open** string enum: a fixed set of known variants plus an
/// `Other(String)` that preserves unrecognized values verbatim.
///
/// JSCalendar requires that unknown enum values "MUST be preserved" (RFC 8984),
/// so most of its enumerations are open. The generated type round-trips its
/// canonical wire strings through `as_str`/`from_wire`, `Display`, and `serde`
/// (as a string), matching exactly when known and falling back to `Other`.
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[derive(::serde::Serialize, ::serde::Deserialize)]
        #[serde(from = "String", into = "String")]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A value not defined by the spec, preserved verbatim.
            Other(String),
        }

        impl $name {
            /// Returns the canonical wire string for this value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Other(value) => value,
                }
            }

            /// Parses a wire string, preserving an unknown value in `Other`.
            #[must_use]
            pub fn from_wire(value: &str) -> Self {
                match value {
                    $( $wire => Self::$variant, )+
                    other => Self::Other(other.to_owned()),
                }
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::core::convert::From<String> for $name {
            fn from(value: String) -> Self {
                Self::from_wire(&value)
            }
        }

        impl ::core::convert::From<$name> for String {
            fn from(value: $name) -> Self {
                value.as_str().to_owned()
            }
        }
    };
}
