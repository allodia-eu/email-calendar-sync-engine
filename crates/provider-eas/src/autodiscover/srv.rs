// SPDX-License-Identifier: MPL-2.0
// DNS SRV fallback ([MS-ASCMD] §4.2 step 7): record shape, selection, lookup.

use super::AutoDiscoverError;

/// Plain-data SRV record shape, decoupled from hickory so record selection is
/// unit-testable without a DNS resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvRecordData {
    /// RFC 2782 priority (lower wins).
    pub priority: u16,
    /// RFC 2782 weight (within a priority class).
    pub weight: u16,
    /// TCP port of the target (typically 443).
    pub port: u16,
    /// Target host, possibly with a trailing root dot (as DNS names arrive).
    pub target: String,
}

/// The SRV query name for [MS-ASCMD] §4.2 step 7. Fully qualified (trailing
/// dot) so the resolver treats it as absolute and skips search-suffix
/// expansion.
pub fn srv_query_name(domain: &str) -> String {
    format!("_autodiscover._tcp.{}.", domain.trim_end_matches('.'))
}

/// RFC 2782 record selection, made deterministic for reproducibility: lowest
/// priority wins; among equal priority the highest weight wins; full ties
/// resolve to the FIRST record in the list. (RFC 2782 §"Weight" specifies a
/// randomized load-balance for equal priorities; a mail client discovers once
/// per account setup, so a deterministic pick is preferable to RNG here.)
///
/// Builds the autodiscover URL for the winning record: port 443 is omitted,
/// any other port is explicit, and the trailing root dot on the target is
/// stripped. A root (".") target means "service decidedly not available at
/// this domain" (RFC 2782) and yields `None`, as does an empty record list.
pub fn srv_autodiscover_url(records: &[SrvRecordData]) -> Option<String> {
    let best = records
        .iter()
        .enumerate()
        .min_by(|(ia, a), (ib, b)| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| b.weight.cmp(&a.weight))
                .then_with(|| ia.cmp(ib))
        })
        .map(|(_, r)| r)?;
    let host = best.target.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    match best.port {
        443 => Some(format!("https://{host}/autodiscover/autodiscover.xml")),
        port => Some(format!(
            "https://{host}:{port}/autodiscover/autodiscover.xml"
        )),
    }
}

/// Thin async wrapper around hickory-resolver: system DNS config
/// (/etc/resolv.conf / Windows registry), SRV query for
/// `_autodiscover._tcp.<domain>`. Deliberately thin — the DNS exchange itself
/// cannot be unit-tested without a live resolver; ALL selection logic lives
/// in `srv_autodiscover_url` (unit-tested).
pub(super) async fn srv_lookup_records(
    domain: &str,
) -> Result<Vec<SrvRecordData>, AutoDiscoverError> {
    let resolver = hickory_resolver::Resolver::builder_tokio()
        .map_err(|e| AutoDiscoverError::Transport(format!("DNS resolver init: {e}")))?
        .build()
        .map_err(|e| AutoDiscoverError::Transport(format!("DNS resolver build: {e}")))?;
    let name = srv_query_name(domain);
    let lookup = resolver
        .srv_lookup(&name)
        .await
        .map_err(|e| AutoDiscoverError::Transport(format!("SRV {name}: {e}")))?;
    let records = lookup
        .answers()
        .iter()
        .filter_map(|record| match &record.data {
            hickory_resolver::proto::rr::RData::SRV(srv) => Some(SrvRecordData {
                priority: srv.priority,
                weight: srv.weight,
                port: srv.port,
                target: srv.target.to_string(),
            }),
            _ => None,
        })
        .collect();
    Ok(records)
}
