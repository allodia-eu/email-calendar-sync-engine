//! The FTS5 tokenizer a database is created with, and the store-open options
//! carrying it. Chosen at creation, recorded in `meta`, immutable afterwards —
//! this engine never re-tokenizes in place (a database re-derives by re-sync).

/// The FTS5 `tokenize=` clause of the two FTS tables (`fts_index`, `message_body_fts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsTokenizer {
    /// Stemmed English tokenization. The default and the only shape every
    /// database created before this option exists has.
    PorterUnicode61,
    /// 3-character substring tokenization (FTS5 `trigram`): CJK-friendly —
    /// a mid-string query like `会议纪` matches `会议纪要`. Queries shorter
    /// than 3 characters cannot use the index (documented in `search.md`).
    Trigram,
}

impl FtsTokenizer {
    /// The FTS5 `tokenize=` clause text; also the `meta.fts_tokenizer` value.
    pub fn sql(&self) -> &'static str {
        match self {
            FtsTokenizer::PorterUnicode61 => "porter unicode61",
            FtsTokenizer::Trigram => "trigram",
        }
    }

    /// Inverse of [`Self::sql`] over values this build may find in `meta`.
    pub fn from_meta(value: &str) -> Option<Self> {
        match value {
            "porter unicode61" => Some(FtsTokenizer::PorterUnicode61),
            "trigram" => Some(FtsTokenizer::Trigram),
            _ => None,
        }
    }
}

/// Store-creation options. Defaults reproduce today's behavior exactly.
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    /// The tokenizer the database's FTS tables are created with — fixed at
    /// creation, read back from `meta` on every later open.
    pub fts_tokenizer: FtsTokenizer,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            fts_tokenizer: FtsTokenizer::PorterUnicode61,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Build-time-selectable FTS5 tokenizer (spec: P0 §4). The default must stay
    //! byte-identical to the historical `tokenize = 'porter unicode61'` clause.

    use super::*;

    #[test]
    fn sql_strings_match_fts5_clauses() {
        assert_eq!(FtsTokenizer::PorterUnicode61.sql(), "porter unicode61");
        assert_eq!(FtsTokenizer::Trigram.sql(), "trigram");
    }

    #[test]
    fn meta_roundtrip_is_total_over_stored_values() {
        for t in [FtsTokenizer::PorterUnicode61, FtsTokenizer::Trigram] {
            assert_eq!(FtsTokenizer::from_meta(t.sql()), Some(t));
        }
        assert_eq!(FtsTokenizer::from_meta("porter"), None);
    }

    #[test]
    fn default_options_keep_porter_unicode61() {
        assert!(matches!(
            OpenOptions::default().fts_tokenizer,
            FtsTokenizer::PorterUnicode61
        ));
    }
}
