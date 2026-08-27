// SPDX-License-Identifier: MPL-2.0
//! Offline EAS transport harness — the real `EasClient` HTTP boundary driven
//! end-to-end against a local mock server, no network.
//!
//! This is the crate's answer to `AGENTS.md`'s coverage rule ("a provider's
//! thin HTTP/TLS transport boundary is the one place offline coverage is
//! hard; drive it with a mock HTTP server / fake executor"). The pattern
//! mirrors two existing engine precedents:
//!
//! * `provider-graph`'s fixture-replay server (`src/test_support.rs`) — a `std::net::TcpListener`
//!   on a background thread answering hand-rolled HTTP/1.1 — generalized from static fixture routes
//!   to a per-request handler closure so retry/provision/redirect *sequences* can be scripted
//!   (EAS's retry layers issue several requests per client call).
//! * `engine-tls`'s in-process TLS round-trip tests — `rcgen` mints a
//!   self-signed cert and a rustls `ServerConnection` serves TLS on
//!   127.0.0.1. The HTTP 451 `X-MS-Location` redirect and autodiscover
//!   scenarios need genuine `https://` because the client refuses `http://`
//!   redirect locations and builds autodiscover URLs as `https://` by spec.
//!
//! Canned WBXML bodies are built with the crate's own serializer
//! (`serialize_tree` over `WbxmlElement` trees using the public tag
//! constants), so fixtures stay self-consistent with the codec under test.
//!
//! Every scenario asserts BOTH the parsed result and some property of the
//! request the client actually put on the wire (a header, a decoded request
//! tree) — a test that asserts nothing is a defect, not coverage.

#[path = "transport_harness/fixtures.rs"]
mod fixtures;
#[path = "transport_harness/harness.rs"]
mod harness;
#[path = "transport_harness/server.rs"]
mod server;

#[path = "transport_harness/autodiscover_flow.rs"]
mod autodiscover_flow;
#[path = "transport_harness/codec_and_parse.rs"]
mod codec_and_parse;
#[path = "transport_harness/compose_flow.rs"]
mod compose_flow;
#[path = "transport_harness/folders_flow.rs"]
mod folders_flow;
#[path = "transport_harness/http_errors.rs"]
mod http_errors;
#[path = "transport_harness/items_flow.rs"]
mod items_flow;
#[path = "transport_harness/multipart_meeting_flow.rs"]
mod multipart_meeting_flow;
#[path = "transport_harness/ping_flow.rs"]
mod ping_flow;
#[path = "transport_harness/provision_flow.rs"]
mod provision_flow;
#[path = "transport_harness/session_options.rs"]
mod session_options;
#[path = "transport_harness/settings_flow.rs"]
mod settings_flow;
#[path = "transport_harness/sync_flow.rs"]
mod sync_flow;
