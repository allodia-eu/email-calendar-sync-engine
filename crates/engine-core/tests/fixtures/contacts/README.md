# Contact conformance fixtures

These inputs lock the provider-neutral contact contracts in
`docs/agent-guidance/contacts.md`. `complete-card.json` and
`complete-card.vcf` describe the same person and exercise multi-value fields,
international names/addresses, a photo reference, and unknown extensions.
`group.json` and `group.vcf` exercise the common group/member shape.
`legacy-malformed.vcf` is deliberately imperfect vCard 3 data: adapters keep
the raw document and salvage independently valid properties.

Provider crates add protocol envelopes around these logical cards for
snapshot/delta/deletion, permission, cursor-expiry, and write-request tests.
