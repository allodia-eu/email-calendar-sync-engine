# Filters and out-of-office: design note

Status: **design settled, nothing implemented.** Written 2026-11-23, when the owner moved message
rules and automatic replies into scope; every question it raised was decided the same day
(section 7). `north-star.md` already records the scope and the accessor pattern (#378). The
change that lands the first slice updates `modeling.md`, and `providers.md` for the new traits, in
the same commit, per "Documentation currency".

Marks used below: **live** = observed against a harness server on 2026-11-23; **docs** = read from
the RFC or vendor reference cited; **unverified** = believed, not yet checked, and must be proven
before code depends on it.

## 1. Problem

A user's mail rules ("move mail from the board to Board", "forward invoices to the bookkeeper")
and their out-of-office reply live on the server, in a different language on every provider. Three
kinds of host need them:

| Need | Sync host (app, local store, outbox) | Transfer host (migrates a mailbox) | Direct host (store-less webmail, JMAP, many users per process) |
|---|---|---|---|
| Read | Show the rules and the reply in settings | Read the source's whole set, once per migration | Per request, no cache across users |
| Edit | Create, reorder, enable, delete; set the reply and its window | — | Same as the sync host, per request |
| Import | — | Write the source's rules and reply to the target, and say per rule what the target kept | — |
| Preflight | — | Tell the operator *before* writing what will not carry | — |
| Concurrency | Refuse an edit built on a stale read where the transport can | Write once into a target nobody else edits yet | Must refuse a stale write: two tabs, one user |
| Offline | Not needed: settings are written directly, not through the outbox (decision 1) | — | — |

What the three share is one neutral model of a rule set and a reply, a capability that says what a
given account's rule language can express, and a receipt that reports what was kept. What differs
is only the call pattern, which the traits below already cover.

## 2. Provider survey

### Rules

| | JMAP (RFC 9661 Sieve) | IMAP + ManageSieve (RFC 5804) | Graph `messageRule` | Gmail `users.settings.filters` | CalDAV/CardDAV |
|---|---|---|---|---|---|
| Unit | One **script** (a blob), many per account, **at most one active** (RFC 9661 §2.1) | Same model: scripts, one active (RFC 5804 §2.8 `SETACTIVE`) | One resource per rule under `mailFolders/inbox/messageRules` | One resource per filter | n/a |
| Read | `SieveScript/get` + blob download (§2.3, §2.2) | `LISTSCRIPTS`, `GETSCRIPT` | `GET …/inbox/messageRules` | `filters.list` | n/a |
| Write | Upload blob, `SieveScript/set` with `onSuccessActivateScript` (§2.4); `SieveScript/validate` checks without storing (§2.6) | `PUTSCRIPT`, `SETACTIVE`, `CHECKSCRIPT`, `HAVESPACE` | `POST` / `PATCH` / `DELETE` per rule | `create` / `delete` only: **no update** (docs) | n/a |
| Atomic over the set | Yes: the set is one script | Yes: one `PUTSCRIPT` | No: one request per rule | No: one request per filter | n/a |
| Version / guard | `state` + `ifInState` (RFC 8620 §5.3) on the `SieveScript` type. Here the type state is the right grain, because the rule set *is* a script (contrast `WriteGuard::Absent` for events) | None. A client can compare `GETSCRIPT` bytes before `PUTSCRIPT`, which narrows the race and does not close it | None documented on `messageRule` (**unverified**: whether `@odata.etag` + `If-Match` is honoured) | None; filters are immutable | n/a |
| Order | Script order | Script order | `sequence` (Int32) | **None**: every matching filter applies | n/a |
| Enable / name | Dialect convention only (Sieve has neither) | Same | `isEnabled`, `displayName`; read-only `hasError`, `isReadOnly` | **Neither** | n/a |
| Stop | `stop` (RFC 5228 §3.3) | Same | `stopProcessingRules` | **None** | n/a |
| Language | Full Sieve: `anyof`/`allof`/`not`, header/address/envelope/size/body/date tests, many extensions | Same, per server | Fixed predicate set (30 fields) ANDed, plus `exceptions` of the same shape; 11 actions (docs) | `from`, `to`, `subject`, `hasAttachment`, `size`, and free Gmail-search `query`/`negatedQuery`; add/remove label ids, `forward` | n/a |
| Limits | Per account: `maxNumberScripts`, `maxSizeScript`, `maxNumberRedirects` | Server config | Exchange rules quota (**unverified**: 64 KB default per mailbox) | 1,000 filters (docs); one user label per filter (docs) | n/a |
| Known gaps | VacationResponse may be a script of its own (RFC 9661 §4, below) | Port 4190 is a separate service; discovery is SRV (RFC 5804 §1.8) or host config | Inbox only; `/users/{shared}/mailboxSettings` is `403` on a delegated credential (`graph.md`), rules on a shared mailbox untested; a personal account answered `403` on `messageRules` (`graph.md`, likely the missing scope) | `forward` needs an address already verified through `forwardingAddresses` (docs, **unverified** live) | n/a |

An IMAP server without ManageSieve, and Gmail or Outlook.com over IMAP, offer no rules surface. A
JMAP account shared read-only lists neither `sieve` nor `vacationresponse` (captured,
`provider-jmap/tests/fixtures/session_shared_accounts.json`), so the answer is per account.

### Out-of-office

| | JMAP `VacationResponse` (RFC 8621 §8) | Sieve `vacation` (RFC 5230, RFC 6131) | Graph `automaticRepliesSetting` | Gmail `VacationSettings` |
|---|---|---|---|---|
| Read / write | `VacationResponse/get` / `/set` on the singleton | Part of the active script | `GET` / `PATCH /me/mailboxSettings` | `getVacation` / `updateVacation` (`PUT`) |
| Enabled | `isEnabled` | The action runs or not | `status`: `disabled`, `alwaysEnabled`, `scheduled` | `enableAutoReply` |
| Window | `fromDate`, `toDate` (UTCDate, null = open) | Needs `date` (RFC 5260 §5, `currentdate`) | `scheduledStart/EndDateTime` (`dateTimeTimeZone`, zoned), only with `scheduled` | `startTime`, `endTime` (epoch ms) |
| Subject | `subject` (null = server picks) | `:subject` | **None** | `responseSubject` (a prefix) |
| Body | `textBody`, `htmlBody` | Reason text, or MIME with `:mime` | `internalReplyMessage`, `externalReplyMessage` (HTML string) | `responseBodyPlainText`, `responseBodyHtml` (HTML wins, sanitised by Gmail) |
| Audience | Everyone | Everyone, or what a test can express (sender domain) | Internal always; external `none` / `contactsOnly` / `all`, with its own message | `restrictToContacts`, `restrictToDomain` (Workspace only) |
| Interval | Server policy (RFC 3834 applies) | `:days` (RFC 5230 §4.1), `:seconds` (RFC 6131) | Server policy | Server policy |
| Guard | `ifInState` on `VacationResponse` | Shares the script's guard | None documented | None |

### Observed on the harness (live, read-only)

- **Stalwart v1.0.2, JMAP.** The session advertises `urn:ietf:params:jmap:sieve`
  (`implementation: "Stalwart v1.0.0"`) and `urn:ietf:params:jmap:vacationresponse`. Alice's
  account states `maxSizeScriptName: 512`, `maxSizeScript: 102400`, `maxNumberScripts: 100`,
  **`maxNumberRedirects: 1`**, `notificationMethods: ["mailto"]`, `externalLists: null`, and 46
  `sieveExtensions`, among them `body`, `copy`, `date`, `editheader`, `ereject`, `fileinto`,
  `imap4flags`, `include`, `mailbox`, `mailboxid`, `regex`, `reject`, `relational`,
  `special-use`, `vacation`, `vacation-seconds`, `variables`. The group share `support` advertises the same; bob's and dave's
  read-only shares advertise neither capability. `SieveScript/get` answers an empty list;
  `VacationResponse/get` answers the singleton `{isEnabled: false, fromDate: null, toDate: null,
  subject: null, textBody: null, htmlBody: null}`. Both answer `state: "n"`: whether the two types
  share one state counter is **unverified**; slice 1 settles it, and section 4 says what a host
  sees either way.
- **Stalwart v1.0.2, ManageSieve.** Listening on 4190 inside the container, **not published** by
  `docker/stalwart/docker-compose.yml`. Greeting: `VERSION "1.0"`, `STARTTLS`, `SASL ""` before
  TLS (mechanisms come after `STARTTLS`, RFC 5804 §1.7), `MAXREDIRECTS "1"`, the same extension
  list as JMAP.
- **Dovecot 2.4.4 + Pigeonhole** (the IMAP harness). ManageSieve on 34190, not published.
  Greeting: `SASL "PLAIN"`, `STARTTLS`, `VERSION "1.0"`, and a **smaller** set: `fileinto reject
  envelope encoded-character vacation subaddress comparator-i;ascii-numeric relational regex
  imap4flags copy include body variables enotify environment mailbox date index ihave duplicate
  mime foreverypart extracttext`. No `vacation-seconds`, `editheader`, `special-use`,
  `mailboxid` or `ereject`. Config: `sieve_max_redirects = 4`, `sieve_max_actions = 32`,
  `sieve_max_script_size = 1M`, vacation period 1 day to 60 days, default 1 week.

The two Sieve servers differ in exactly the extensions a rule renderer would lean on (`special-use`
to name Trash, `mailboxid` to survive a rename, `editheader` for importance) and in how many
redirects one message may cause. The vocabulary must therefore be read per account, never
assumed per protocol.

Graph and Gmail are surveyed from their references only (learn.microsoft.com `messageRule`,
`messageRulePredicates`, `messageRuleActions`, `automaticRepliesSetting`; developers.google.com
`users.settings.filters`, `VacationSettings`). Nothing about them here is live.

## 3. The neutral model

### Sieve as the canonical form, or a typed rule list?

Sieve is tempting as the canonical form: two of the four transports speak it, it is standardised,
and it can say everything Graph and Gmail can say and more. It is the wrong spine, for four reasons:

1. **A host cannot edit it.** A settings screen needs "rule 3 has two conditions and moves to
   Board". Sieve is a programming language (variables, nested `if`, `include`); turning arbitrary
   scripts back into rows is undecidable in general, so every host would carry its own partial
   Sieve parser, which is the engine pushing its job outward.
2. **Graph and Gmail are not Sieve.** Their rules carry facts Sieve has no home for (`isEnabled`,
   `displayName`, `hasError`, Exchange message classes, Gmail's search query). Converting them to
   Sieve on read would lose those facts before anyone asked for Sieve.
3. **The receipt is per rule.** "Rule 3 lost its importance action" needs rules to be units. A
   script is one blob.
4. **Capability checks need structure.** "Can this account express this rule?" is a question
   about conditions and actions against a vocabulary, not about text.

**Decision: a typed, ordered rule list is the model; Sieve is one rendering of it; the raw form
is always kept.** For a Sieve source the engine segments the script at top-level commands, merges
`require`, and reads each top-level `if` it recognises as a typed rule. Everything else, a rule it
does not recognise, a `set`, a bare `keep`, becomes an **opaque rule** holding its exact bytes, in
place. A write re-renders only the rules the host changed and copies every other segment verbatim,
so a hand-written script survives an edit made in the app. This is the engine's existing rule
("a write re-derives every modelled property from the normalized value", `modeling.md`) applied to
a script.

The segmenter lives in a new pure crate, `engine-sieve`, and is small and hand-written: a lexer
that knows strings, `text:` blocks, comments and brackets, splits the script at its top-level
commands, and understands nothing else. Stalwart's `sieve-rs` is `AGPL-3.0-only OR
LicenseRef-SEL` (its source headers on docs.rs), which the MPL engine cannot take, and a full
parser would buy semantics the model does not use. The segmenter is fuzzed beside the existing
MIME and iCalendar targets.

The recognised dialect is the one the engine writes, and the engine writes Roundcube's: its
managesieve plugin marks each rule with `# rule:[name]` and a disabled one with `if false # …`
(the byte detail is **unverified**; slice 2 pins it, section 7). Roundcube is the most widely used
webmail on Sieve servers, so a user who also uses it reads and edits the same named rules there,
and rules it wrote read back typed here. When the account has no active script, the engine creates
one named `rules` and activates it; otherwise it edits the active script in place.

### Types (sketch, `engine-core::rules`)

```rust
/// The account's rules, in evaluation order, as read.
#[non_exhaustive]
pub struct RuleSet {
    pub rules: Vec<MailRule>,
    /// What a write names to be refused if the set moved since (`RuleSetVersion` is opaque:
    /// a JMAP state, a hash of the ManageSieve script). `None`: the transport has none.
    pub version: Option<RuleSetVersion>,
    /// The provider's own form, kept for round trips: the Sieve script, or the rules' JSON.
    pub raw: RawRules,
}

#[non_exhaustive]
pub struct MailRule {
    pub name: Option<RuleName>,   // Graph displayName; a dialect comment on Sieve; none on Gmail
    pub enabled: bool,
    pub body: RuleBody,
    pub origin: Option<RuleOrigin>, // the provider key (Graph rule id, Gmail filter id) or Sieve span
}

pub enum RuleBody {
    Typed { when: Match, actions: Vec<Action>, stop: bool },
    /// A rule the engine carries but cannot show: kept byte for byte, writable only back to
    /// the same kind of transport.
    Opaque(RawRule),
}

pub struct Match {
    pub mode: MatchMode,           // All | Any
    pub conditions: Vec<Condition>, // empty: every message
    pub except: Vec<Condition>,    // any one of these vetoes the rule (Graph `exceptions`)
}

pub enum Condition {
    Header { field: HeaderName, test: TextTest },            // subject, or a named header
    Address { field: AddressField, part: AddressPart, test: TextTest }, // from/to/cc/to-or-cc; all/local/domain
    SentToMe(MeField),           // to, cc, to-or-cc, only-to, not-to (needs the account's addresses)
    Body(TextTest),
    Size(SizeBound),             // Over(bytes) | Under(bytes)
    HasAttachment,
    Importance(Importance),      // engine_core's existing type
    Native(NativeCondition),     // provider-only: an Exchange message class, an unparsed Gmail query
}

pub struct TextTest { pub op: TextOp, pub values: Vec<String> } // values are any-of
pub enum TextOp { Contains, Is, Matches /* glob */, Regex }

pub enum Action {
    FileInto { mailbox: RuleMailbox, copy: bool }, // copy: also keep it where it was
    Discard,                                       // gone, not to Trash
    Redirect { to: Vec<EmailAddress>, copy: bool }, // original sender kept
    Forward { to: Vec<EmailAddress> },             // resent as the account (Exchange forward)
    AddKeywords(BTreeSet<Keyword>),                // $seen, $flagged, a named keyword (category)
    Native(NativeAction),                          // forward-as-attachment, mark importance, …
}

pub enum RuleMailbox { Role(MailboxRole), Id(MailboxId) }
```

`Trash` and `Junk` are `FileInto { Role(..) }`, which is how Graph's `delete` and Gmail's `TRASH`
label read. Mark-read and flag are keywords, the axis the engine already uses for both. A Graph
category is a named keyword, as `KeywordName` already treats it. `Native` holds the provider's own
JSON fragment and its kind, so a host can show "a condition this app cannot display" and the
engine can carry it to the same kind of provider; it never crosses kinds.

A Gmail filter's `query` and `negatedQuery` are parsed when every term is one of a small subset:
`from:`, `to:`, `subject:`, `has:attachment` and `larger:`, read as `Address`, `Header`,
`HasAttachment` and `Size(Over)` conditions (a `negatedQuery` lands in `except`). A query with any
other term or operator stays whole, as one `Native` condition: it carries to Gmail unchanged, and on
a non-Gmail target the rule is `NotKept`, because a lost condition drops the rule (section 4).

No rule-level `Reject` (RFC 5429) in the first slices: no non-Sieve provider has it, so it reads as
an opaque rule until a host asks.

### Out-of-office

```rust
#[non_exhaustive]
pub struct VacationResponse {
    pub enabled: bool,
    pub window: VacationWindow,        // start, end: Option<UtcDateTime>; both None = no window
    pub reply: AutoReply,              // subject: Option<String>, text: Option<String>, html: Option<String>
    pub audience: ReplyAudience,       // Everyone | Contacts | Organisation | OrganisationAndContacts
    pub outside_reply: Option<AutoReply>, // a different reply for senders outside the organisation
    pub raw: RawVacation,
}
```

`audience` and `outside_reply` exist because Graph and Gmail both have them and a migration from
either has to say when they are lost. Graph's `externalAudience` maps `all`→`Everyone`,
`none`→`Organisation`, `contactsOnly`→`OrganisationAndContacts`; Gmail's `restrictToContacts`→
`Contacts`, `restrictToDomain`→`Organisation` (both set is lossy: contacts inside the domain).
Graph's zoned schedule is read as instants (a cut-off, not a wall-clock event) and its zone stays in
`raw`. The Sieve reply interval (`:days`, `:seconds`) is not modelled and is left to the server, as
three of the four transports leave it; a Sieve source's value is preserved in the raw script.

### Content and logs

Rule values, names, addresses and reply text are content. Every type here gets the redacted
`Debug` the raw types have, and receipts and errors carry classes and positions, never values
(`north-star.md`, security).

## 4. Provider traits (`engine-provider`)

### Capability as a type

A capability is its own trait, reached through an accessor on `Provider` that defaults to `None`,
as `mail_importer` does (#375; `north-star.md`, "Key Decisions", #378). `Box<dyn Provider>`
delegates the accessor like every other method, so a host holding one reaches the domain without
knowing the adapter and without the upcast cost `providers.md` measured for sub-traits. An adapter
that implements the trait answers `Some(self)`; there is no capability flag beside it.

```rust
trait Provider {
    // …
    fn mail_rules(&self) -> Option<&dyn MailRules> { None }
    fn vacation(&self) -> Option<&dyn Vacation> { None }
    #[cfg(feature = "import")]
    fn rules_importer(&self) -> Option<&dyn RulesImporter> { None }
}

#[async_trait]
pub trait MailRules: Send + Sync {
    /// What this account's rule language can express, read at connect (JMAP `sieveExtensions`
    /// and limits, the ManageSieve greeting) or a constant (Graph, Gmail).
    fn vocabulary(&self) -> &RuleVocabulary;
    fn guard(&self) -> WriteGuard;                       // Enforced on JMAP; Absent elsewhere
    async fn rules(&self, account: &AccountId) -> ProviderResult<RuleSet>;
    /// Replaces the whole set. `base` is the set as the caller read it: its version guards the
    /// write, its raw form supplies every rule the caller did not change.
    async fn replace_rules(&self, account: &AccountId, base: &RuleSet, rules: &[MailRule])
        -> ProviderResult<RulesReceipt>;
}

#[async_trait]
pub trait Vacation: Send + Sync {
    fn vocabulary(&self) -> &VacationVocabulary;       // window? subject? audiences? outside reply?
    async fn vacation(&self, account: &AccountId) -> ProviderResult<VacationResponse>;
    async fn set_vacation(&self, account: &AccountId, base: &VacationResponse,
        new: &VacationResponse) -> ProviderResult<VacationReceipt>;
}
```

`Some(..)` is the capability; `vocabulary()` is what it can express. JMAP answers `Some` only for
an account listing the capability (so not on a read-only share). ManageSieve is a module of
`provider-imap`, sharing the adapter's TLS, SASL and the account's connection budget; its endpoint
comes from host configuration, with no SRV discovery (RFC 5804 §1.8) for now. An IMAP provider
answers `Some` only when the host gave it that endpoint and the greeting succeeded. On ManageSieve,
rules and vacation are one script, so both traits are one object and share its version.

Rules and the reply are account settings, edited online in every client surveyed, so every host
kind writes them directly, guarded by the version it read; they never go through the outbox and
have no store tables. A sync host reads them on demand, as a direct host does.

On JMAP the guard is `Enforced` whether or not Stalwart moves the `SieveScript` state when the
vacation reply changes (slice 1 answers it). If it does, a rules write built on a read taken
before a reply change is refused once with `stateMismatch`, though no rule moved; the host re-reads
and writes again, as after any stale read. Nothing is lost, and the refusal is the guard working
at the grain the server keeps.

`replace_rules` is whole-set because order is a property of the set and two transports store the
set as one script. Graph and Gmail adapters diff `base` against `rules` and issue per-rule calls;
Gmail's "update" is create-then-delete, new first, as `put_draft` orders its two calls. Each
per-rule failure lands in the receipt rather than failing the call.

### The pure core: plan, then render

What a target keeps is decided before anything is sent, by one pure function in `engine-core`:

```rust
pub fn plan_rules(rules: &[MailRule], target: &RuleVocabulary, mode: PlanMode) -> RulesPlan;
// PlanMode: Edit | Import { forwarding: ForwardingRules }   // ForwardingRules: Disabled (default) | Enabled
// RulesPlan { rules: Vec<MailRule> /* what will be written */, outcomes: Vec<RuleOutcome> }
```

A sync host calls it to grey out what the account cannot hold. A transfer host calls it for the
preflight report without touching the target. Every adapter's `replace_rules` and importer run it
first, so the receipt's classes come from one place and adapters only add what the server alone
can refuse (a Gmail forwarding address not verified, a script over `maxSizeScript`).

### The receipt

```rust
pub struct RuleOutcome {
    pub source: usize,             // index in the rules handed in
    pub kept: Kept,                // Whole | Reduced | Disabled | NotKept
    pub losses: Vec<Loss>,
}
pub struct Loss { pub part: RulePart, pub reason: LossReason }
pub enum RulePart { Condition(usize), Exception(usize), Action(usize), Stop, Enabled, Name, Order }
pub enum LossReason {
    NotExpressible,      // the target's language has no equivalent
    Approximated,        // carried with a stated difference (Exchange forward → redirect)
    MissingMailbox,      // the folder it names has no counterpart on the target
    ForwardNotAllowed,   // the target refuses the address (Gmail unverified) or the count (maxNumberRedirects)
    LimitExceeded,       // rule count, script size
    SourceInError,       // Graph `hasError`
    OpaqueSource,        // an unparsed Sieve rule going to a non-Sieve target
}
```

Two safety rules are part of the plan, not host policy:

- **A lost condition or exception drops the whole rule (`NotKept`).** Dropping either *widens*
  what the rule matches. A "delete newsletters except from my bank" rule that lost its exception
  would delete the bank's mail. A lost *action* narrows what the rule does, so the rule is carried
  `Reduced` as long as one action remains.
- **A rule that sends mail out of the account (`Redirect`, `Forward`) is imported `Disabled`.**
  Planted forwarding rules are a known way an attacker keeps reading a compromised Microsoft 365
  mailbox, and a migration should not quietly arm one on a new server. Each such rule is listed
  in the transfer host's preflight report, and the operator re-enables after review, or turns on a
  per-project switch that imports them enabled; the host passes that switch to the import call.
  On a target that cannot store a disabled rule (Gmail), the rule is `NotKept` and the receipt
  says so. An edit through `replace_rules` is the user's own and is written as given.

### Import (the `import` feature)

The import seam (`engine_provider::import`, behind the `import` feature: `MailImporter`, #375, with
IMAP import, #376) gains a sibling, reached the same way, through `Provider::rules_importer`:

```rust
#[async_trait]
pub trait RulesImporter: Send + Sync {
    /// Appends `rules` after the target's own, never replacing them, and answers per rule.
    async fn import_rules(&self, account: &AccountId, rules: &[MailRule],
        forwarding: ForwardingRules) -> ProviderResult<Vec<RuleOutcome>>;
    async fn import_vacation(&self, account: &AccountId, reply: &VacationResponse)
        -> ProviderResult<VacationReceipt>;
}
```

It differs from `replace_rules` in what it may touch: it appends, keeps the target's existing
rules, and applies the import-only defaults above. The host rewrites each `RuleMailbox::Id` from
source to target mailbox ids first, with the folder map the mail import already built; an id it
cannot map stays, and the plan reports `MissingMailbox`. Errors follow `MailImporter`:
`requires_confirmation` means the script or a rule may have been stored; `item_unavailable`
means it never will be. Unlike `import_messages`, the call answers one result for the set,
because a Sieve target stores the set as one script; what each rule kept is in its
`RuleOutcome`, as an imported message reports the `MessageMetadata` the target kept (#383).

A transfer host reads the source and writes the target with administrative access where the
provider offers it: Graph application permissions (shared-mailbox settings need them; delegated
`mailboxSettings` is self-only, `graph.md`), Google domain-wide delegation, a Sieve or IMAP master
user. Per-user credentials are the fallback. The adapter is the same either way; only the
credential the host supplies differs (the `CredentialSource` design note, #381).

### Example: Graph rules to a Dovecot target over ManageSieve

Source, two Graph rules (abridged):

```json
[{"displayName": "Invoices", "sequence": 1, "isEnabled": true,
  "conditions": {"subjectContains": ["Invoice"]},
  "actions": {"moveToFolder": "AAMk-finance", "markImportance": "high",
              "stopProcessingRules": true}},
 {"displayName": "Board", "sequence": 2, "isEnabled": true,
  "conditions": {"fromAddresses": [{"emailAddress": {"address": "board@example.com"}}]},
  "exceptions": {"isAutomaticReply": true},
  "actions": {"moveToFolder": "AAMk-board"}}]
```

Plan against Dovecot's vocabulary (no `editheader`; `isAutomaticReply` is an Exchange message
class, `Native`):

```text
rule 0  Reduced  Action(1) NotExpressible        # markImportance: no editheader
rule 1  NotKept  Exception(0) NotExpressible     # dropping it would also move auto-replies
```

Written script, in the Roundcube dialect (section 3; layout as slice 2 pins it):

```sieve
require ["fileinto", "mailbox"];
# rule:[Invoices]
if header :contains "subject" "Invoice" {
    fileinto "Finance";
    stop;
}
```

On Stalwart the same plan keeps rule 0 `Whole` (it has `editheader`, rendered as
`addheader "Importance" "high"`, `Approximated`), and rule 1 stays `NotKept` for the same reason.

## 5. Mapping

✓ carries; ≈ carries with a stated difference (`Approximated`); ✗ cannot (`NotExpressible`);
`ext` names the Sieve extension the account must list.

### Conditions

| Neutral | Sieve | Graph | Gmail |
|---|---|---|---|
| `Match` All / Any | ✓ `allof` / `anyof` | All ✓ (predicates ANDed; values in one predicate ORed, **unverified**); Any ✗ | All ✓; Any ≈ (`query` with `OR`) |
| `except` | ✓ `not anyof(…)` | ✓ `exceptions` (same predicates only) | ≈ `negatedQuery` |
| Subject contains | ✓ `header :contains "subject"` | ✓ `subjectContains` | ≈ `subject` (word match, not substring) |
| Subject is / glob / regex | ✓ `:is` / `:matches` / `:regex` (ext `regex`) | ✗ | ✗ |
| Named header contains | ✓ `header :contains "X-…"` | ≈ `headerContains` (any header) | ✗ |
| From is / contains | ✓ `address :is` / `:contains "from"` | ✓ `fromAddresses` / `senderContains` | ≈ `from` (search semantics) |
| From domain | ✓ `address :domain` | ≈ `senderContains "@example.com"` | ≈ `from:(@example.com)` |
| To / Cc / to-or-cc | ✓ `address ["to","cc"]` | ✓ `recipientContains`, `sentToAddresses` | ≈ `to` (also matches Bcc) |
| Sent to me (to, cc, only, not) | ≈ address tests over the account's addresses | ✓ `sentToMe`, `sentCcMe`, `sentOnlyToMe`, `notSentToMe`, `sentToOrCcMe` | ≈ `to:me` in `query` (only `to`) |
| Body contains | ✓ `body :text :contains` (ext `body`) | ✓ `bodyContains` | ≈ `query` |
| Subject or body | ✓ `anyof(header…, body…)` | ✓ `bodyOrSubjectContains` | ≈ `query` |
| Size over / under | ✓ `size :over` / `:under` | ≈ `withinSizeRange` (KB, a range) | ✓ `size` + `sizeComparison` |
| Has attachment | ≈ `exists :mime :anychild` heuristics (ext `mime`) | ✓ `hasAttachments` | ✓ `hasAttachment` |
| Importance | ≈ header tests over `Importance::HEADERS` | ✓ `importance` | ✗ (Gmail's `important` is its own classifier) |
| `Native`: Exchange class (`isMeetingRequest`, `isAutomaticReply`, `sensitivity`, `categories`, `messageActionFlag`, …) | ✗ (a few ≈ through headers, not attempted) | ✓ | ✗ |
| `Native`: unparsed Gmail `query` | ✗ | ✗ | ✓ |

### Actions and rule properties

| Neutral | Sieve | Graph | Gmail |
|---|---|---|---|
| `FileInto` (move) | ✓ `fileinto`; by name, or `:mailboxid` (RFC 9042, survives a rename) where listed | ✓ `moveToFolder` | ✓ add label + remove `INBOX` (one user label per filter) |
| `FileInto { copy }` | ✓ `fileinto :copy` (ext `copy`, RFC 3894) | ✓ `copyToFolder` | ✓ add label, keep `INBOX` |
| `FileInto { Role(Trash / Junk) }` | ✓ `:specialuse` (RFC 8579) where listed, else by the folder's name | ✓ `delete` / move to Junk | ✓ `TRASH` / `SPAM` |
| `Discard` | ✓ `discard` | ✓ `permanentDelete` | ≈ `TRASH` (recoverable) |
| `Redirect` | ✓ `redirect` (`:copy` keeps it), bounded by `maxNumberRedirects` | ✓ `redirectTo` (keeps a copy) | ≈ `forward`: one verified address, original sender kept |
| `Forward` (as the account) | ≈ `redirect :copy` (From differs) | ✓ `forwardTo` | ≈ `forward` |
| `AddKeywords` `$seen` / `$flagged` | ✓ `addflag "\\Seen"` / `"\\Flagged"` (ext `imap4flags`, RFC 5232) | ✓ `markAsRead` / ✗ (no flag action) | ✓ remove `UNREAD` / add `STARRED` |
| `AddKeywords` named | ✓ `addflag "name"` | ≈ `assignCategories` | ≈ hidden named label (counts as the one user label) |
| `Native`: mark importance | ≈ `addheader` (ext `editheader`) | ✓ `markImportance` | ≈ `IMPORTANT` |
| `Native`: forward as attachment | ✗ | ✓ `forwardAsAttachmentTo` | ✗ |
| `stop` | ✓ `stop` | ✓ `stopProcessingRules` | ✗ (every filter applies) |
| `enabled: false` | ≈ dialect: `if allof(false, …)` | ✓ `isEnabled` | ✗ |
| `name` | ≈ dialect comment | ✓ `displayName` | ✗ |
| Order | ✓ script order | ✓ `sequence` | ✗ |

### Out-of-office

| Neutral | JMAP | Sieve | Graph | Gmail |
|---|---|---|---|---|
| `enabled` | ✓ | ✓ | ✓ `status` | ✓ |
| `window` | ✓ | ✓ `currentdate` (ext `date`) | ✓ `scheduled` (zoned) | ✓ |
| `reply.subject` | ✓ | ✓ `:subject` | ✗ | ≈ prefix |
| `reply.text` / `html` | ✓ / ✓ | ✓ / ≈ `:mime` | ≈ / ✓ (one HTML string) | ✓ / ✓ |
| `audience` Everyone | ✓ | ✓ | ✓ external `all` | ✓ |
| `audience` Organisation | ✗ | ≈ sender-domain test (needs the domains) | ✓ external `none` | ✓ `restrictToDomain` (Workspace) |
| `audience` Contacts | ✗ | ✗ | ≈ `contactsOnly` (external only) | ✓ `restrictToContacts` |
| `outside_reply` | ✗ | ≈ sender-domain branch | ✓ `externalReplyMessage` | ✗ |

## 6. Phasing and tests

### Phasing

Each slice is proven live before the next starts (`AGENTS.md`).

1. **Vacation over JMAP.** `VacationResponse` in `engine-core`, the `Vacation` trait and accessor,
   `provider-jmap` get/set with `ifInState`. Live on Stalwart, which also settles the RFC 9661 §4
   question: does setting a reply create a `SieveScript`, does it move the `SieveScript` state, and
   does activating it deactivate a user's script? The answer shapes slice 2.
2. **Rules over JMAP Sieve.** The pure `engine-sieve` crate (segmenter, Roundcube dialect reader,
   renderer, fuzzed beside the existing MIME/iCalendar targets), `plan_rules`, `MailRules` on
   `provider-jmap` (upload, `SieveScript/set` with `ifInState` and `onSuccessActivateScript`,
   `SieveScript/validate` before storing; a new script is named `rules`). Live on Stalwart.
3. **ManageSieve.** The harness publishes 4190 (Stalwart) and 34190 (Dovecot); a `managesieve`
   module of `provider-imap` over the adapter's TLS, SASL and connection budget, at a
   host-configured endpoint; vacation and rules in one script. Live on both, which exercises two
   vocabularies.
4. **Import from Graph.** Read `messageRules` and `automaticRepliesSetting` into the model;
   `RulesImporter` on the JMAP and ManageSieve targets; per-rule receipts; the preflight list of
   forwarding rules. Live against a work/school test tenant with application permissions (the
   personal account answered `403`).
5. **Gmail read**, then Graph and Gmail writes for the sync and direct hosts.

### Tests

- **Live, both directions, per AGENTS.md.** `ifInState` with a stale state is refused
  (`stateMismatch`) and with the current one is accepted. A rule written is read back typed. An
  extension a server lacks is refused by `plan_rules` *and*, sent anyway in a test-only request,
  by the server's `invalidSieve` / `CHECKSCRIPT`, proving the vocabulary is the server's and not a
  guess. An absence (no `editheader` on Dovecot) is recorded only after asking for it.
- **Fixtures, scrubbed as written.** The JMAP session already captured; `SieveScript/get`,
  `VacationResponse/get` and both ManageSieve greetings, captured as observed here; Graph and Gmail
  responses once a tenant run exists.
- **Round trip.** For any script, segment then render with no edits reproduces the bytes (property
  test and fuzz target). An edit to one rule leaves every other segment byte-identical.
- **Plan.** Table tests over the mapping in section 5, one row per cell, including both safety
  rules (a lost exception drops the rule; a redirect imports disabled).
- **Redaction.** `Debug` of every rule, reply and receipt prints no value.

## 7. Decisions (2026-11-23)

The owner approved each of these; the body of the note is written to match.

1. **Rules and the vacation reply are written directly, with a version guard,** for every host
   kind. They are settings, not messages, so they do not go through the outbox, and the store
   keeps no tables for them (section 4).
2. **Forwarding and redirect rules import disabled by default.** Each one is listed in the
   transfer host's preflight report, and a per-project switch imports them enabled. Where a
   target cannot store a disabled rule, the rule is `NotKept` and the receipt says so
   (section 4).
3. **The engine writes Roundcube's Sieve dialect,** `# rule:[name]` per rule, so the most widely
   used webmail reads and edits what the engine writes. A new script, when the account has no
   active one, is named `rules` (section 3).
4. **A Gmail filter's `query` is parsed for a small subset** (`from:`, `to:`, `subject:`,
   `has:attachment`, `larger:`) into typed conditions. Anything else stays `Native` and is
   reported as not kept on a non-Gmail target (section 3).
5. **A new crate, `engine-sieve`, holds a small hand-written segmenter** that splits a script at
   its top-level commands and understands nothing else. Stalwart's `sieve-rs` is AGPL, which the
   MPL engine cannot take. It is fuzzed beside the existing parsers (section 3, slice 2).
6. **The Sieve reply interval (`:days`) is left to the server.** The source's value is preserved
   in the raw script (section 3).
7. **ManageSieve is a module of `provider-imap`,** sharing TLS, SASL and the account's connection
   budget. Its endpoint comes from host configuration; there is no SRV discovery for now
   (section 4, slice 3).
8. **Transfer hosts use administrative access where the provider offers it** (Graph application
   permissions, which shared-mailbox settings need; Google domain-wide delegation; a Sieve or IMAP
   master user), with per-user credentials as the fallback (section 4).
9. **Capabilities are reached through accessors on `Provider` that default to `None`,** as
   `mail_importer` does (#375; `north-star.md`, #378): `mail_rules()`, `vacation()`, and
   `rules_importer()` behind the `import` feature. The `providers.md` rule against sub-traits is
   superseded; that doc is rewritten to match (#386).

### Settled by live tests

These are facts to observe, not choices; the design holds whichever way each falls.

- **Whether Stalwart moves the `SieveScript` state when the vacation reply changes** (slice 1).
  The guard is `Enforced` either way; if it does, a host sees one `stateMismatch` on the next rules
  write and re-reads (section 4).
- **RFC 9661 §4 on Stalwart** (slice 1): whether setting a reply creates a `SieveScript`, and
  whether activating it deactivates the user's script. The answer decides how slice 2 places the
  vacation segment.
- **The byte layout of Roundcube's dialect** (slice 2), including `if false # …` for a disabled
  rule, pinned from scripts the plugin wrote, scrubbed as fixtures.
- **The Graph and Gmail items marked unverified** (slices 4 and 5): `@odata.etag` with `If-Match`
  on `messageRule`, the Exchange rules quota, OR within one predicate, rules on a shared mailbox,
  and Gmail's verified-forwarding-address requirement.
