//! Addresses in User IDs, and how they compare (`openpgp.md` → Addresses).

/// Canonicalises an email address the way Autocrypt Level 1 does: the domain
/// converted to IDNA ASCII and lowercased, the local part lowercased.
///
/// Returns `None` for anything that is not one address: no `@`, an empty side, a
/// second `@`, whitespace or angle brackets in it, or a domain IDNA rejects.
///
/// This is looser than the engine's contact identity, which keeps the local part
/// exact: a binding check that disagreed with the sender's own client about case
/// would call a genuine signature someone else's.
pub fn canonical_address(address: &str) -> Option<String> {
    let address = address.trim();
    let (local, domain) = address.rsplit_once('@')?;
    let forbidden = |c: char| c == '@' || c == '<' || c == '>' || c.is_whitespace();
    if local.is_empty()
        || domain.is_empty()
        || local.contains(forbidden)
        || domain.contains(forbidden)
    {
        return None;
    }
    let domain = idna::domain_to_ascii(domain).ok()?.to_lowercase();
    if domain.is_empty() {
        return None;
    }
    Some(format!("{}@{domain}", local.to_lowercase()))
}

/// The address a User ID binds: the last `<…>` in a `Name <addr-spec>` form, or the
/// whole User ID when it is a bare address.
pub(crate) fn user_id_address(user_id: &str) -> Option<String> {
    match user_id.rfind('<') {
        Some(open) => {
            let inner = &user_id[open + 1..];
            canonical_address(&inner[..inner.find('>')?])
        }
        None => canonical_address(user_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unclosed_angle_bracket_binds_nothing() {
        assert_eq!(user_id_address("Alice <alice@example.org"), None);
    }

    #[test]
    fn a_domain_idna_rejects_binds_nothing() {
        assert_eq!(canonical_address("alice@xn--"), None);
    }
}
