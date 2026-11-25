# Filters and out-of-office: design note

Status: **design settled; slice 1 (the reply over JMAP), slice 2 (rules over JMAP Sieve),
slice 3 (rules and the reply over ManageSieve) and slice 4 (importing rules and the reply into
JMAP and ManageSieve, and reading them from Graph) implemented, with slice 5's Graph and Gmail
reads and writes.** Written
2026-11-23, when the owner moved message rules and automatic replies into scope; every question it
raised was decided the same day (section 7). `north-star.md` records the scope and the accessor
pattern (#378). Slice 1 landed `engine_core::vacation`, the `Vacation` trait and
`Provider::vacation`, and `provider-jmap`'s `VacationResponse` get and set, and answered the
questions it was to settle live ("Settled by live tests"). Slice 2 has landed `engine-sieve` (the
segmenter, and the dialect reader, writer and vocabulary), `engine_core::rules` with
`plan_rules`, the `MailRules` trait and `Provider::mail_rules`, and `provider-jmap`'s rules over
JMAP Sieve, with the reply kept running beside them, and settled live how the two coexist.
Slice 3 has landed the reply as a rule of the script (`engine_sieve::dialect::reply`) and a
`managesieve` module of `provider-imap` with both traits, proven live on Stalwart and Dovecot
("Settled in slice 3"). Slice 4's target half has landed `RulesImporter` and
`Provider::rules_importer`, one import over every target's own `MailRules` and `Vacation`
(`engine_provider::import::append_rules`, `import_reply`), the preflight list
(`engine_core::rules::held_forwarding`), and both Sieve targets, proven live ("Settled in slice
4"). Slice 4's source half, Graph's rules and reply, is read by `provider-graph`, which writes
them too (slice 5's Graph writes), reads and writes proven live, the writes on rules and a reply
of the test's own ("Settled for Graph"). Slice 5's Gmail half, `provider-google`'s filters and
vacation reply, read and written, is proven live on filters and a reply of the test's own
("Settled for Gmail"). Where the code differs from a sketch below, the sketch has been brought up
to date and says why.

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
| Version / guard | `state` + `ifInState` (RFC 8620 §5.3) on the `SieveScript` type. Here the type state is the right grain, because the rule set *is* a script (contrast `WriteGuard::Absent` for events) | None. A client can compare `GETSCRIPT` bytes before `PUTSCRIPT`, which narrows the race and does not close it | None: a `messageRule` carries no `@odata.etag`, listed or read alone, and a stale `If-Match` is ignored (**live**). A compare before writing narrows the race; Graph's listing can lag a write it has answered (**live**) | None; filters are immutable | n/a |
| Order | Script order | Script order | `sequence` (Int32) | **None**: every matching filter applies | n/a |
| Enable / name | Dialect convention only (Sieve has neither) | Same | `isEnabled`, `displayName`; read-only `hasError`, `isReadOnly` | **Neither** | n/a |
| Stop | `stop` (RFC 5228 §3.3) | Same | `stopProcessingRules` | **None** | n/a |
| Language | Full Sieve: `anyof`/`allof`/`not`, header/address/envelope/size/body/date tests, many extensions | Same, per server | Fixed predicate set (30 fields) ANDed, plus `exceptions` of the same shape; 11 actions (docs) | `from`, `to`, `subject`, `hasAttachment`, `size`, and free Gmail-search `query`/`negatedQuery`; add/remove label ids, `forward` | n/a |
| Limits | Per account: `maxNumberScripts`, `maxSizeScript`, `maxNumberRedirects` | Server config | Exchange rules quota (**unverified**: 64 KB default per mailbox) | 1,000 filters (docs); one user label per filter (**live**: two are `400 invalidArgument`); at least one criterion (**live**); no two identical filters (**live**: `400 failedPrecondition`) | n/a |
| Known gaps | VacationResponse may be a script of its own (RFC 9661 §4, below) | Port 4190 is a separate service; discovery is SRV (RFC 5804 §1.8) or host config | Inbox only; `/users/{shared}/mailboxSettings` is `403` on a delegated credential (`graph.md`), an application token with `MailboxSettings.Read` reads a shared mailbox's rules and reply (**live**); a personal account answered `403` on `messageRules`, its token lacking `MailboxSettings.*` (**live**) | `forward` needs an address already verified through `forwardingAddresses` (**live**: `400 failedPrecondition` otherwise) | n/a |

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
| Body | `textBody`, `htmlBody` | Reason text, or MIME with `:mime` | `internalReplyMessage`, `externalReplyMessage` (HTML string) | `responseBodyPlainText` or `responseBodyHtml`: where both are sent the text is dropped (**live**) |
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
  subject: null, textBody: null, htmlBody: null}`. Both answer `state: "n"`, and they are one
  counter (**live**, slice 1): a write of either moves both ("Settled by live tests").
- **Stalwart v1.0.2, ManageSieve.** Listening on 4190 inside the container, published on
  `127.0.0.1:14190` since slice 3. Greeting: `VERSION "1.0"`, `STARTTLS`, `SASL ""` before
  TLS (mechanisms come after `STARTTLS`, RFC 5804 §1.7), `MAXREDIRECTS "1"`, the same extension
  list as JMAP.
- **Dovecot 2.4.4 + Pigeonhole** (the IMAP harness). ManageSieve on 34190, published from the rev1
  service on `127.0.0.1:14191` since slice 3; `MAXREDIRECTS "4"` is listed only once signed in.
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

Graph and Gmail were surveyed from their references (learn.microsoft.com `messageRule`,
`messageRulePredicates`, `messageRuleActions`, `automaticRepliesSetting`; developers.google.com
`users.settings.filters`, `VacationSettings`). Graph's reads have since been observed live on a work
account ("Settled for Graph"), and Gmail's reads and writes on a throwaway personal account
("Settled for Gmail").

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
managesieve plugin marks each rule with `# rule:[name]` and a disabled one with `if false # …`.
The bytes are pinned from scripts the plugin's own writer produced, run offline, which its own
parser read back identically (not **live**: no server was involved; `engine-sieve`'s
`tests/fixtures/roundcube/README.md` says how they were made and what they show). What they
show: one `require ["a","b"];` first, names sorted; per rule a `# rule:[name]` line (none for an
unnamed rule), `if <test>`, then `{`, one tab-indented action per line, and `}`, every line ending
in CRLF; several tests as `allof (a, b)` or `anyof (a, b)`; a rule for every message as `if true`;
a disabled rule as `if false # <its tests as they would be written>`. The segmenter splits each
into one segment per rule, the name comment travelling with its `if`. Roundcube is the most
widely used webmail on Sieve servers, so a user who also uses it reads and edits the same named
rules there, and rules it wrote read back typed here. The rules live in the script that runs; where
none of the user's runs, in the engine's own script named `rules`, or the one script besides the
reply's, and a first write creates `rules` and activates it. On a JMAP server that runs the reply
as a script of its own, the rules script includes the reply's while the reply is on, so both run
("Settled by live tests", slice 2).

Where the server keeps no reply of its own (ManageSieve), the reply is a rule of that same script,
as Roundcube's vacation engine writes it: a new one written first and named `Out-of-Office`, its
window `currentdate :zone … :value "ge"` / `"le" "iso8601"` tests (`allof` for two, `true` for
none), switched off as any rule is. The plugin takes as the reply the first rule whose first action
is `vacation`; the engine narrows that to one whose tests are only those the plugin writes for a
window, so a user's own `if header :contains "from" "boss" { vacation …; }` stays an ordinary rule
rather than being read, and rewritten, as a reply to everyone. `engine_sieve::dialect::reply`
reads it into the neutral `VacationResponse` and writes one over a script, in place, keeping the
`vacation` arguments the model leaves to the script (`:days`, `:seconds`, `:addresses`, `:handle`,
`:from`) and any action after it, every other rule copied byte for byte. The reply rule is the
reply's alone: `read_rules` leaves it out of the user's rules, and `write_rules` copies it where it
was, so a host that hides it and writes the rules back keeps the reply, and a migration does not
carry the `vacation` twice; `write_rules` puts it after the last written rule that ran before it
(else before the first that ran after it), so every rule that ran ahead of it, a `discard; stop`
among them, still does however a host inserts or moves rules. A reply rule
holding more than the model says (another action, an `elsif` or `else`, a `:mime` body, a date in
the server's own zone, a `${` the script would expand) is read as a reply the engine cannot show,
never as a narrower one; it belongs to neither the reply nor the rules, so a host shows it, and
`UnreadReply::sends_mail` says whether it may send mail, for a preflight. It is written over only
when the caller names it (`write_reply_over`): that keeps its name, every command after `vacation`
and any `elsif` or `else`, and `vacation`'s unmodelled arguments where `vacation` could be read,
and discards the rest. A reply that is on is refused where any enabled rule of the user's runs
`vacation` in any branch (it may match the same message, RFC 5230 §4.7 makes two an error, and
Dovecot then sends neither), and beside an `include`, which the dialect does not follow. The
window is written to the second and never wider than asked; one that holds no whole second is
refused. Text only for now: a `:mime` body holding HTML is a follow-up. The bytes are pinned from
the plugin's own output, one script saved through its settings page
(`tests/fixtures/roundcube/README.md`).

### Types (`engine-core::rules`, implemented in slice 2)

Implemented as sketched, with these differences, each for a reason the code met:

- `RuleMailbox` also has `Path(MailboxPath)`: a Sieve `fileinto` names a folder by path, and a
  rule read from a script must keep that name until something resolves it. `RuleMailbox::Role
  (Inbox)` needs no role extension anywhere: every transport has the inbox by name.
- Marking an importance is a typed `Action::MarkImportance(Importance)`, not `Native`: it crosses
  kinds (Graph's `markImportance`, Sieve's `addheader "Importance"`, ≈), which `Native` never does.
- A condition's header is `HeaderField::Subject` or `Named(String)`; addresses are
  `RuleAddress`, names `RuleName`, folder paths `MailboxPath`, each with a `Debug` that shows only
  a length. `RuleOrigin::ScriptSegment(index)` is the Sieve span.
- `RulePart` adds `Rule` (an opaque rule as a whole) and `Mode` (how conditions combine), and
  `RuleVocabulary` is a map from a `RuleFeature` (the grain where transports differ) to `Support`
  (`Exact` or `Approximated`), plus the redirect limit and the kinds of opaque and native parts it
  stores back.
- The receipt is `RulesReceipt { version, outcomes, stored, reply, activation, already_held,
  failures }`: where the rules went (`RulesStored`), what the write did to a running reply
  (`ReplyEffect`), whether the rules written run (`RulesActivation`, slice 4), the rules handed
  in that an import found already held (slice 4), and each rule a rule-by-rule write did not store
  or remove (`RuleWriteFailure`, Graph); `RulesPlan.rules` pairs each written rule with its source
  index (`PlannedRule`).
- `RuleVocabulary::with_folders` (a `FolderMap`, source folder id to target folder id) is how an
  import files by id: a mapped id is written as the target's, and one the map does not hold loses
  its action as `MissingMailbox` (slice 4). `RuleVocabulary::without` takes a feature out, for a
  transport that cannot use what its language could say (ManageSieve, filing by id).
- A `RawRule` carries what the reader learned of its text:
  - the extensions it requires, from one table of command, test and tag, a tag read in the
    context of its command (`:seconds` is `vacation-seconds` on `vacation` only), and where a
    token is not in the table, everything the source script required (unknown, for a command a
    host wrote);
  - whether it may send mail: unless every command and test in it is known to send none, so
    `redirect`, `notify`, `include` and anything unknown (`pipe`, `execute`) may;
  - the name it holds;
  - whether its strings hold `${`, and whether its source expanded them (`Expansion`).

  The plan carries it only to a target that lists every extension it needs, never renames or
  disables it (its text holds both), does not carry one that may send mail on an import that arms
  no forwarding, and does not move one with `${` to another script (it means what its own script
  made of it, and may use a variable another command there sets). The writer refuses one whose
  `${` the script it writes over would read the other way, and a typed rule with `${` anywhere
  over a script that expands it (`DialectErrorKind::Variable`). A `NativeAction` says whether it
  sends mail too.
- `RuleOrigin::ScriptSegment { script, index }`: `script` is a fingerprint of the text read, so a
  rule read from one script is never copied as a segment of another.
- A `TextTest` always has a value, and `RuleName`, `MailboxPath` and `RuleAddress` are never empty
  and never hold a CR, LF or NUL: what no transport writes cannot be built. The plan answers
  `NotExpressible` for what can still be built and not written (a role no transport names, a
  redirect to nobody, no keyword to add).
- A rule a provider lists as an object of its own (Graph) has `RuleOrigin::Listed { set, index }`:
  its index in the listing kept as the set's raw form, `set` a fingerprint of that listing. The
  provider's id stays in the raw form, so the origin stays `Copy` and names a position.
- `MailRule::in_error` and `read_only` carry Graph's `hasError` and `isReadOnly`. The plan records
  `SourceInError` for a rule in error; an import stores it disabled for review (`Kept::Disabled`),
  or does not keep it where the target cannot store a disabled rule, and an edit writes it as given.
  A write never sends a read-only rule.
- `RulesReceipt::failures` lists each rule a rule-by-rule write did not store or remove
  (`RuleWriteFailure`: `FailedRule::Written` or `Removed`, its class, whether it may have landed,
  and whether no credential could be had for it);
  the rest of the write goes ahead, unless the failure reaches every later rule (a throttle, a
  refused credential, no credential at all: `ProviderError::ends_batch`, the import's rule too),
  when nothing more is sent and each later rule is listed with that class
  (`engine_provider::RuleWriteEnding`). A transport that stores the set at once fails whole
  instead.
- `RuleLimit` says what a target holds part by part and not together (`RuleVocabulary::with_limit`):
  one test per kind among the conditions, and among the exceptions (where tests with values merge
  into one any-of) one only of a kind with a single value; one move, copy and importance per rule;
  `To` and `Cc` only together; no system keyword but `$seen` and `$flagged`; filing only into
  folders, not virtual collections (`RealFoldersOnly`). The plan judges a second test of a kind as
  a lost condition or exception and a second action of a kind as a lost action, so a preflight sees
  Graph's grain before anything is written. Gmail's adds two (slice 5): one of the user's folders
  per rule, a named keyword counting as one (`OneFolderPerRule`, a second is a lost action), and a
  rule that tests something (`SomeTest`: one with neither conditions nor exceptions is not kept).
  A vocabulary without limits plans as before.
- `RuleOutcome::not_written`, `written_short` and `left_beside` are what a rule-by-rule writer
  records on a planned outcome when the server did not do as asked, one answer for Graph and
  Gmail; `RulesReceipt::left_as_read` names the rules whose edit did not apply (section 4).
- `TextOp::Words` is a search's match (whole words and phrases, ignoring case: Gmail's), and
  `AddressPart::WithName` the address with the display name beside it (`RuleFeature::
  AddressWithName`): what Gmail's `subject:` and `from:` say, read exactly. Neither is wider or
  narrower than another test in every case, so no other stands in for it, and no Sieve or Graph
  vocabulary holds it: a Gmail rule with one is not kept there. Gmail's limits are `RuleLimit`s too
  (slice 5): `ExactExceptions` (an approximated exception is lost: narrowing an exception widens
  the rule), `ImportanceHighOrLow`, `NativeConditionsAllOf`, `PhraseValues` (`rules::is_phrase`:
  no quote, no control character, not blank, not `me`), `TrashJunkArchiveOnly`,
  `WordsOnSubjectAndSender` (a word match on an address only on the sender with its name); a
  vocabulary may
  hold only some named keywords (`RuleVocabulary::with_keywords`); and a provider's own action that
  sends mail counts as one address against the redirect limit (Gmail's reader marks every action
  but `forward` as sending none, an action key Gmail may add later included).
- `LossReason::NotWritten`: a rule-by-rule write asked for a part and did not get it, and the
  receipt's `failures` say why; on the whole rule it is not kept, on its order it is reduced, so no
  outcome reads whole for a rule listed as failed.
- A redirect that keeps the message needs only `RuleFeature::RedirectCopy`: Graph's `redirectTo`
  always keeps it, so Graph lists that and not `Redirect`.
- The action order is not semantic. The writer adds keywords and an importance before it files,
  redirects or discards, because a flag set after a `fileinto` does not reach the filed copy (RFC
  5232 §3); a script in another order reads as carried whole.

The sketch as written before the code:

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

A Gmail filter's `from` and `subject` criteria, and its `query` and `negatedQuery`, are parsed when
every term is one of a small subset: `from:`, `subject:`, `has:attachment`, `larger:` and
`smaller:` (a plain number of bytes), values a word, a quoted phrase or an any-of (`a OR b`,
`(a OR b)`, `{a b}`). They read as `Address { From, WithName, Words }`, `Header { Subject, Words }`,
`HasAttachment` and `Size` conditions: exactly what Gmail matches, a search's word match over the
sender's name and address and over the subject. A `query` of terms joined by spaces adds them to the
conditions; one of terms joined by ` OR ` beside no other criterion is an any-of rule. A
`negatedQuery` of terms joined by ` OR ` is the exceptions; one whose terms must all hold says "not
both", which no list of exceptions says, and stays whole. `to:` matches every recipient, `Bcc`
too, as no typed condition says, so it stays whole too, as does `me`, bare or quoted. A query with
any other term or operator stays whole, as one `Native` condition: it carries to Gmail unchanged,
and on a non-Gmail target the rule is `NotKept`, because a lost condition drops the rule (section
4); so does a word match, which no other target holds.

No rule-level `Reject` (RFC 5429) in the first slices: no non-Sieve provider has it, so it reads as
an opaque rule until a host asks.

### Out-of-office

Implemented in slice 1 as `engine_core::vacation`:

```rust
#[non_exhaustive]
pub struct VacationResponse {
    pub enabled: bool,
    pub window: VacationWindow,        // start, end: Option<UtcDateTime>; both None = no window
    pub reply: AutoReply,              // subject: Option<String>, text: Option<String>, html: Option<String>
    pub audience: ReplyAudience,       // Everyone | Contacts | Organization | OrganizationAndContacts
                                       // | ContactsInOrganization
    pub outside_reply: Option<AutoReply>, // a different reply for senders outside the organization
    pub version: Option<VacationVersion>, // as read; what a write built on it names
    pub raw: Option<RawVacation>,      // the provider's own form, as read
}
// replies_at(at): on, and at in [start, end) — RFC 8621 §8's "on or after" / "before".

#[non_exhaustive]                      // private fields, read through accessors
pub struct VacationVocabulary { window: WindowSupport, subject: bool, bodies: ReplyBodies,
    audiences: BTreeSet<ReplyAudience>, outside_reply: bool }
pub enum WindowSupport { None, EitherEnd, BothEnds }   // Graph's `scheduled` needs both ends
pub enum ReplyBodies { One, TextAndHtml }              // Graph keeps one HTML string per message
// lost_parts(&reply) -> Vec<VacationPart>  // Window | Subject | Bodies | Audience | OutsideReply

#[must_use]
pub struct VacationReceipt { pub version: Option<VacationVersion>, pub rules: RulesEffect }
pub enum RulesEffect { Untouched, Deactivated, StillDeactivated, Unknown }
```

The version sits on the reply, so a guarded write names the reply it read
(`VacationPrecondition::IfUnchanged(&read)`) as `replace_rules` will name its base; `raw` is `None`
on a reply a host built rather than read. The audiences are spelled `Organization`, as every other
identifier in the engine is. `lost_parts` is the one rule for what a transport would drop, used by
the edit (which refuses) and later by import (which reports). The vocabulary's fields are private
and its window and bodies are enums, so Graph's shape (both ends, one body) and whatever a later
transport needs are additions, not breaking changes. `RulesEffect` exists because of what slice 1
found live: turning the reply on can switch the user's own filter script off, and turning it off
leaves it off ("Settled by live tests"), and a host must be able to say so, and offer to switch the
rules back on. The engine never does that itself.

`audience` and `outside_reply` exist because Graph and Gmail both have them and a migration from
either has to say when they are lost. Graph's `externalAudience` maps `all`→`Everyone`,
`none`→`Organization`, `contactsOnly`→`OrganizationAndContacts`; Gmail's `restrictToContacts`→
`Contacts`, `restrictToDomain`→`Organization`, both→`ContactsInOrganization`. A read never maps
an audience to a wider one; where a provider's setting has no exact audience, the adapter answers
the narrowest one that contains it.

Graph is canonicalised on read, so a reply that says the same thing reads the same way: an
`externalReplyMessage` equal to the `internalReplyMessage`, or `externalAudience: none` (which sends
no external reply at all), reads as `outside_reply: None`. Only a genuinely different external
message is an outside reply.
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
pub trait Vacation: Send + Sync {                       // implemented, slice 1
    async fn vocabulary(&self, account: &AccountId) -> ProviderResult<VacationVocabulary>;
    fn vacation_write_guard(&self) -> WriteGuard;      // Enforced on JMAP
    async fn read_vacation(&self, account: &AccountId) -> ProviderResult<VacationResponse>;
    async fn set_vacation(&self, account: &AccountId, precondition: VacationPrecondition<'_>,
        new: &VacationResponse) -> ProviderResult<VacationReceipt>;
}

pub enum VacationPrecondition<'a> { IfUnchanged(&'a VacationResponse), Unconditional }
```

The read is `read_vacation`, not `vacation`: an adapter implements both `Provider` and `Vacation`,
and two methods named `vacation` would make every call on a concrete adapter ambiguous. The
vocabulary is asked per account, async and fallible, as `mail_import_limits` is: a Gmail account's
audiences depend on whether it is a Workspace account, a ManageSieve account's window on the
extensions its server lists. The guard is `vacation_write_guard`, beside `calendar_write_guard` and
`contact_write_guard`, and `WriteGuard` now lives in a module of its own.

The write's guard is spelled out at the call site and has one source. `IfUnchanged(&read)` guards
by the read reply's version, and on a transport that enforces one, a reply with no version (built,
not read) is refused rather than written unguarded. `Unconditional` is the deliberate overwrite (an
administrator turning a reply off). `new`'s own version and raw form are never read. Refusals made
before sending are never `InvalidState`, which is also how a write that may have landed reads: a
part the account cannot hold, or a window that ends at or before its start, is `item_unavailable`
(`refuse_unheld_vacation`), and a guarded write with no version is `Permanent`.

`Some(..)` is the capability; `vocabulary()` is what it can express. JMAP answers `Some` only for
an account listing the capability (so not on a read-only share). Graph answers `Some` on every
mailbox: its permission is a token scope, known only by asking, and a credential without
`MailboxSettings.Read` shows as a `Permanent` `403` on the first read (observed on a personal
account), which a host answers by asking for the scope. ManageSieve is a module of
`provider-imap`, sharing the adapter's TLS, SASL and the account's connection budget; its endpoint
comes from host configuration, with no SRV discovery (RFC 5804 §1.8) for now. An IMAP provider
answers `Some` only when the host gave it that endpoint and the greeting succeeded. On ManageSieve,
rules and vacation are one script, so both traits are one object and share its version.

As built in slice 3 (`provider-imap/src/managesieve/`): `ImapConfig::with_managesieve(addr,
server_name)` names the endpoint; `STARTTLS`, the protocol's one transport, is required. When the
account connects it reads the greeting and what the server offers over TLS, with no credential,
alongside the mail connect (joined, so the outcome never depends on which finishes first), under
its own 10-second deadline, and leaves without waiting for `LOGOUT`'s answer.
Where that succeeds the connect observer sees `ConnectStep::Negotiated("ManageSieve", extensions)`
and `mail_rules()` answers `Some` (`vacation()` too where `vacation` is listed). Where it fails the
observer sees `ConnectStep::Unavailable { service: ConnectService::ManageSieve, class }`, the class
alone, and both answer `None`; mail works without them. The accessors answer what the last probe
found: a failure that may pass (`Retryable`, a timeout) is probed again beside a later dial, and
`invalidate()` asks for a probe whatever the last one found, so a blip at connect does not hide the
rules for the account's lifetime. One that will not pass (no `STARTTLS`, not ManageSieve) waits for
`invalidate()`. After any failure no dial probes for five minutes, so a blackholed endpoint costs a
dial at most one probe deadline in that time, and a probe whose dial is dropped is wanted again.
`Some` says the service answers, not that signing in works: nothing is signed in at connect. Each call opens a session of its own in a
slot of the account's connection budget (closing a parked IMAP socket when the shelf is full),
with two minutes for the dial and the work, and then sends `LOGOUT` without waiting for its
answer, so a slow `LOGOUT` never puts a finished write in doubt. The credential is asked as `CredentialProtocol::ManageSieve` for the origin
`sieve://<server name>:<port>` (RFC 5804 §3); a password goes as `AUTHENTICATE "PLAIN"`, a token as
`OAUTHBEARER` or `XOAUTH2`. The version is a SHA-256 of the script list (names and the active flag)
and the rules script's text; ManageSieve has no conditional `PUTSCRIPT`, so the guard is
`WriteGuard::Absent`, but a write lists and reads again in its own session and refuses a stale base
as `Conflict`, which narrows the race to one round trip and does not close it. The script is chosen
as JMAP chooses it, by one function (`engine_sieve::dialect::scripts::rules_script`: the active
one, else `rules`, else the only one, never a server's own reply script); a new one is `rules`,
stored with `PUTSCRIPT` and switched on with `SETACTIVE`. JMAP knows the server's reply script by
its name (`vacation`), ManageSieve by its text, in exactly the shape Stalwart writes
(`scripts::plainly_server_reply`, pinned by live captures of every window shape in
`engine-sieve/tests/fixtures/stalwart/`): a `require` of exactly `vacation`, `relational` and
`date`, then up to two nested `if currentdate :value "ge"|"le" "iso8601"` around one unnamed
`vacation :mime`, and nothing else. `:mime` and that `require` are what a user's own script does
not write. Any other script with an unnamed `vacation` (SOGo's, or a user's plain
`vacation :days 7 "Away";`) is someone's rules, refused rather than rewritten or routed around.
ManageSieve reads only the scripts the choice asks about: the active one, `rules`, every
candidate left where several are (so a server's inactive reply beside one user script leaves
that script, as JMAP chooses), and one the chosen script includes. One removed in between is a
`Conflict`, and a script whose name RFC 5804 forbids still counts but is never read. So the two
can choose differently only for a user's script named `vacation` or a server's reply script under
another name; on Stalwart they agree, which the live suite pins (the reply's script active, and
inactive beside a user's `myrules`). Where a
server's reply script runs, as the active one or through the rules' `include`, ManageSieve writes
the rules as JMAP does: the include is kept first, and a new `rules` script includes the running
reply's and is made the active one, so the reply runs on; a server that lists no `include` is
refused before anything is sent. The include is no rule of the user's on either protocol
(`engine_sieve::dialect::include`). `PUTSCRIPT` checks the script itself, so neither
`CHECKSCRIPT` nor `HAVESPACE` is sent first. The vocabulary is what the server lists once signed in.
The reply is on where its rule is on and the script runs; turning it on where the script is stored
but off switches the script on only where that switches on no enabled rule of the user's, and is
refused otherwise. Text replies only: an HTML body is refused before sending.

Rules and the reply are account settings, edited online in every client surveyed, so every host
kind writes them directly, guarded by the version it read; they never go through the outbox and
have no store tables. A sync host reads them on demand, as a direct host does.

On JMAP the guard is `Enforced`. Stalwart moves the `SieveScript` state when the vacation reply
changes, and the reply's state when a script changes (slice 1, live), so a rules write built on a
read taken before a reply change is refused once with `stateMismatch`, though no rule moved, and
the other way round; the host re-reads and writes again, as after any stale read. Nothing is lost,
and the refusal is the guard working at the grain the server keeps.

`replace_rules` is whole-set because order is a property of the set and two transports store the
set as one script. Graph and Gmail adapters diff `base` against `rules` and issue per-rule calls;
Gmail's "update" is create-then-delete, new first, as `put_draft` orders its two calls. Each
per-rule failure lands in the receipt rather than failing the call.

As built for Graph (`provider-graph`, `graph.md` → "Mail rules and the out-of-office reply"), on
what a live run showed of how Graph keeps rules: `sequence` is a position in a list, renumbered on
every change and clamped to its end; a `PATCH` clears nothing it is sent as `null`; a stale
`If-Match` is ignored; the listing and `mailboxSettings` can lag a write already answered. So both
guards are `Absent`, with a SHA-256 of what was read as the version, read again before writing and
refused as `Conflict` where it moved, which narrows the race and does not close it, and neither
receipt carries a version. A write plans (the vocabulary's `RuleLimit`s carrying Graph's grain),
removes the rules no rule handed in names first (so a replacement a host built, with no origin,
deletes the old rule before its new copy is `POST`ed, and a failed `POST` leaves neither; editing
the rule read instead writes it anew, new first), then walks the planned rules keeping the server's list as it
goes: a
new rule `POST`ed at its position, one out of place given its position by a `sequence`-only
`PATCH`, one in place not sent; a changed rule `PATCH`ed whole where it drops nothing, and written
anew in its place (new first, then the old deleted) where it drops a part or shortens a list,
since a `PATCH` cannot clear one. Graph names every rule, so an edit that removes a name leaves the
old one and says so on the outcome. A read-only rule is never sent, nor a rule whose id holds a `/`
(unproven live). A rule the writer still cannot place keeps the server's
rule as it was and is a failure; every failure is on the rule's outcome too
(`LossReason::NotWritten`).

**An edit that is not kept does not apply** (decided 2026-11-24, for every transport that stores
rules one by one). A rule the plan does not keep, or whose write failed, that names a rule the
write read, leaves that rule where it is, as it was, and the receipt says so:
`RulesReceipt::left_as_read` lists it, by its index in the rules handed in. Nothing is removed for
it. Graph and Gmail do this (Graph's proven offline with its fake, no live Graph write run for it;
Gmail's live); the question "is the rule as read still there" has one answer,
`engine_provider::RuleListing::left_as_read`, which both call. A rule whose write may have landed
(an outcome-unknown failure: a timeout, a gateway's `502` or `504`) is not named, since its edit
may have applied; on Gmail that is conservative, since a failure before the filter was sent (a
forwarding or label read) leaves the filter read certainly untouched. The base check is one answer too: a base is the listing read when its version is
that of its raw form as the adapter parses it (`RuleListing::of_base`), so a host that re-encoded
the raw form is still understood, and the listing read now is compared with it by version
(`RuleListing::unchanged_since`). A rule written anew whose old copy
could not be removed is `LossReason::LeftBeside`, not `NotWritten`: both run until it goes, so the
rule is `Reduced` and the account does more than asked meanwhile. The Sieve targets store the set
as one script and still leave a not-kept rule out of it; giving them the same treatment is a
follow-up.

As built for Gmail (`provider-google`, `google.md` → "Filters and the vacation reply"): filters
have no update and no ETag, so the guard is `Absent` with the listing's SHA-256 read again before
writing (the same `RuleListing` as Graph's). A changed rule is created first and the filter it was
read as deleted after; a rule Gmail already holds as a filter (the one it was read as, one no rule
claims, one this write created) is not sent, since Gmail refuses a duplicate; a forward to an
address not verified for forwarding, typed or Gmail's own, is lost as `ForwardNotAllowed` before
sending; a label made for a keyword of a filter then not stored is deleted again. Gmail showed
every write on the next read (no lag observed).

### The pure core: plan, then render

What a target keeps is decided before anything is sent, by one pure function in `engine-core`:

```rust
pub fn plan_rules(rules: &[MailRule], target: &RuleVocabulary, mode: PlanMode) -> RulesPlan;
// PlanMode: Edit | Import { forwarding: ForwardingRules }   // ForwardingRules: Disabled (default) | Enabled
// RulesPlan { rules: Vec<MailRule> /* what will be written */, outcomes: Vec<RuleOutcome> }
```

A sync host calls it to grey out what the account cannot hold. A transfer host calls it for the
preflight report without touching the target, and lists the forwarding rules the import will
hold back with `held_forwarding`, which is decided by `plan_rules` alone ("Import" below). Every adapter's `replace_rules` and importer run it
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
        forwarding: ForwardingRules, folders: &FolderMap) -> ProviderResult<RulesReceipt>;
    /// Writes `reply` where the target holds none of its own.
    async fn import_vacation(&self, account: &AccountId, reply: &VacationResponse)
        -> ProviderResult<VacationReceipt>;
}
```

It differs from `replace_rules` in what it may touch: it appends, keeps the target's existing
rules, and applies the import-only defaults above. Errors follow `MailImporter`:
`requires_confirmation` means the script or a rule may have been stored; `item_unavailable`
means it never will be. Unlike `import_messages`, the call answers one result for the set,
because a Sieve target stores the set as one script; what each rule kept is in its
`RuleOutcome`, as an imported message reports the `MessageMetadata` the target kept (#383).

**As built (slice 4, the target half).** The contract is: **never change what the target
already had, and be safe to run again.** Where the code differs from the sketch above, it says
why.

- **A receipt, not a list.** The sketch answered `Vec<RuleOutcome>`; the import answers a whole
  `RulesReceipt`, its outcomes indexed by the rules handed in, because an import can do what an
  edit can: create a `rules` script and switch it on (`RulesStored::CreatedRunning`), or write into
  a script that does not run (`RulesReceipt::activation`). It also says which rules the target
  already held (`already_held`), and that nothing was written (`RulesStored::Unwritten`). A bare
  list would lose all of it, and the receipt is the transfer host's record of the pair.
- **The importer takes the folder map; the host does not rewrite ids.** The sketch had the host
  rewrite each `RuleMailbox::Id` and leave an unmapped one as it was. Folder ids are a server's
  own and repeat across accounts (Stalwart numbers each account's from `a`), so an unmapped source
  id can be a folder the target has, and an existence check cannot tell the two apart: the rule
  would file into the wrong folder and report `Whole`. So the host hands in its `FolderMap`
  (source id to target id), the plan writes each mapped id as the target's
  (`RuleVocabulary::with_folders`), and an id the map does not hold loses its action as
  `MissingMailbox`. JMAP also asks the server for the mapped target ids the rules need
  (`Mailbox/get` for exactly those, in chunks of `maxObjectsInGet`) and drops a pair whose target
  folder is gone, so it is `MissingMailbox` too. ManageSieve files by no id at all (below).
- **One import for every target**, in `engine_provider::import`: `append_rules` and
  `import_reply` over a target's `&dyn MailRules` and `&dyn Vacation`. An adapter's
  `RulesImporter` calls them, and adds only what it alone knows. So the import writes through
  `replace_rules` and `set_vacation`, and which script holds the rules, the guard, the reply's
  include and the rule that a write never switches stored rules on or off are each decided in one
  place, for an edit and an import alike.
- **The rules.** `append_rules` reads the target's rules and plans the imported ones in `Import`
  mode. Before writing, it plans the target's own rules as an edit would: where that would write
  any of them differently or not at all (a rule needing an extension the server takes in `require`
  without listing it, STALWART-8), the import is refused (`item_unavailable`). It then writes the
  target's own rules, unchanged, followed by the planned ones, through `replace_rules` built on
  that read; a race with another client in between is the edit's `Conflict`, nothing written. The
  imported rules are planned twice, once as an import and once inside `replace_rules` as an edit;
  the second keeps every planned rule as the first wrote it (pinned by a test), so the outcomes
  are the import's. Each is written as a rule of no script (`MailRule::origin` cleared): the
  segment it names is the source's, and a target script with the same text must not be taken for
  it. The target's own rules are copied byte for byte, and `require` grows. One more line changes
  in one case: where the target's rules script is stored but switched off and includes a server
  reply that does not run, the write leaves the include out, as an edit does, because an included
  script runs even where it is not the active one: kept, it would switch a reply that is off on
  with the rules.
- **Safe to run again.** A planned rule the target already holds an equal typed rule for is not
  written again (equal leaving out where each was read and the source's marks, `in_error` and
  `read_only`, which a target reads its rules back without; whether it runs, `enabled`, still
  counts), each held rule answering for one handed in (two equal rules need two held), and
  the receipt lists it (`already_held`); its outcome is still what the plan decided. A rule equal
  to one the user wrote themselves counts as imported: written again it would do nothing more,
  since the held copy runs first on the same messages. A rule carried whole is never taken as held
  (its text is compared by no one), so running an import twice writes such a rule twice. A
  planned rule is compared in the form the target will read it back once written: the target's
  own writer renders it and its own reader reads it (`ImportDialect::read_back`;
  `engine_sieve::dialect::read_back` for both Sieve targets). So whatever the dialect makes of a
  rule on the way is made of the rule handed in too, with no case-by-case normalisation: a
  forward written as `redirect :copy`, two redirects or two keyword actions alike read as one,
  a single condition of either kind read as `All`, `INBOX` by path read as the inbox's role.
- **Facts from the text, failing closed.** A rule carried whole is planned with what its own text
  requires and does, read by the target's dialect (`ImportDialect::facts`;
  `engine_sieve::dialect::facts_from_text`), not with what the host that built it said. A
  `RawRule` a host builds is assumed to send mail, name a folder by id and run another script
  until a reader says otherwise (`sending_no_mail`, `naming_no_folder_by_id`,
  `including_no_script`), so a target that read nothing from the text still carries none of it.
  The import is stricter than an edit here, on purpose: where a carried rule uses a command or a
  tag the dialect's tables do not know (a vendor extension's, such as Dovecot's `pipe`),
  the reader of a script falls back to everything the source script's `require` named, but
  `facts_from_text` reads the rule alone, with no script to fall back to, so what it needs is
  unknown and no target stores it (`OpaqueSource`). Needs that cannot be verified fail closed,
  since the source's `require` names what the source server took, not what the rule needs on
  another. An edit is unaffected: it plans the account's own rules, read with their script. A
  forwarding rule an operator has switched on since reads differently from the disabled one the
  import writes, so a second run adds it again, disabled.
- **Never stop what runs.** On JMAP, where the target runs the server's reply script and the new
  rules script cannot include it (the condition fails), an import that would create and switch on
  `rules` is refused (`item_unavailable`) rather than stop the reply, as ManageSieve refuses where
  its server lists no `include`. The other way round, a reply import that would switch the running
  rules off (the server's flag, with no include path) is refused too. Both are the edit's own
  write paths, asked to refuse where an edit writes and says so (`ReplyEffect::Stopped`,
  `RulesEffect::Deactivated`): the JMAP importer hands the engine's import a view of the adapter
  whose writes refuse (`ImportTarget`), so the question "would this stop what runs" keeps one
  answer, computed where the write is.
- **Where the target's rules script is stored but switched off**, the imported rules are written
  into it and do not run until the user switches it on: activating it would switch on rules the
  user left off, or switch off the server's own reply script, and a write never does either. The
  receipt says so (`RulesReceipt::activation: Inactive`), as every rules write's receipt now does.
- **What a rule carried whole cannot take across.** On import, a rule carried whole that holds
  `${`, names a folder by id (`fileinto :mailboxid`), or runs another script (`include`) is
  `NotKept`: the first means what its own script made of it, and the other two name the source
  account's folders and scripts. The reader marks the last two
  (`RawRule::names_folders_by_id`, `includes_scripts`). A *typed* imported rule whose value holds
  `${`, going into a target script that requires `variables`, still refuses the whole import
  (the writer's `Variable` refusal): the plan cannot see the target's script, and judging it by
  the account's extensions instead would drop every such rule on both servers surveyed, which
  list `variables` whether or not the script requires it.
- **The reply.** `import_reply` refuses a reply with a part the target cannot hold, whole
  (`refuse_unheld_vacation`, as an edit is refused: a lost window, audience or outside reply would
  widen who is answered). It then reads the target's reply and compares the two as the target
  holds them (`as_held`): a CRLF is an LF, one line end closing a text is dropped (a Sieve `text:`
  block adds it), and the window is taken to whole seconds inward, as the Sieve writer stores it.
  Where the target's reply is empty (off, nothing to say, no window, the default audience) it
  writes the imported one, guarded by that read. Where it already says the same, it answers
  without writing, so an import run again after an interruption answers as the first did. One
  race is left open: on a JMAP server whose `SieveScript` and `VacationResponse` states are
  separate counters, a user script switched on between the write's listing and its
  `VacationResponse/set` is switched off by the reply going on by the server's flag, which the
  import would have refused had it seen it. The receipt says so (`RulesEffect::Deactivated`),
  which is how the host learns of it and can offer to switch the rules back on. Any
  other reply, on or off, is the target's own and is refused (`item_unavailable`): an import never
  replaces it, as it never replaces a rule. A host that means to replace it writes through
  `Vacation::set_vacation` itself, `Unconditional` if it must.
- **A disabled rule filing by id reads as enabled over ManageSieve.** A rule another client wrote
  as `if false # …` with `fileinto :mailboxid` is carried whole there, and a rule carried whole is
  enabled: its text decides whether it runs, and it does not. A host shows it as a rule it cannot
  display rather than as a disabled one. Keeping `enabled: false` on it would make every edit drop
  it (the plan and the writer take a rule carried whole as running), so this stays a known limit.
- **Folders by id, per target.** JMAP files by the target's `Mailbox` id, which its Sieve
  `:mailboxid` names. ManageSieve files by no id: Sieve's `:mailboxid` names an RFC 8474
  `MAILBOXID`, which the IMAP adapter does not read (its folder ids are names), so its vocabulary
  leaves filing by id out, a rule filing by id loses that action there, and a host names an IMAP
  target's folders by path. Written unchecked, `fileinto :mailboxid` with an id the server lacks
  files into the inbox (RFC 9042's fallback), which is why neither target writes one it has not
  seen.

The preflight list of decision 2 is `engine_core::rules::held_forwarding(rules, target)`: each
rule an import holds back because it sends mail out of the account, with its outcome under the
default, `Disabled` (stored for review) or `NotKept` (the target cannot store it disabled). It is
the set of rules `plan_rules` keeps differently with `ForwardingRules::Enabled`, so `plan_rules`
alone decides it; it agrees with the receipt of an import planned against the same vocabulary and
folders, and a target that changed in between can answer differently. A rule already disabled at
the source, or one the target would not keep either way, is not listed: the switch changes
nothing for it.

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

Written script, in the Roundcube dialect, as `engine_sieve::dialect::write_rules` writes it (every
line ends in CRLF; the action is indented by a tab):

```sieve
require ["fileinto"];
# rule:[Invoices]
if header :contains "subject" "Invoice"
{
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
| `Match` All / Any | ✓ `allof` / `anyof` | All ✓ (predicates ANDed; values in one predicate ORed, **unverified**; a second test of one kind ✗, `RuleLimit::OneTestPerKind`, since Graph would read the two as either); Any ✗ | All ✓; Any ✓ (`query` of terms joined by `OR`, alone); a native condition only among all-of ones (`NativeConditionsAllOf`) |
| `except` | ✓ `not e`, or `allof (c1, …, not e1, …)`; with any-of conditions `allof (anyof (c1, …), not e1, …)`; an `anyof` holding a `not`, or nested lists, read as carried whole | ✓ `exceptions` (same predicates only) | ✓ `negatedQuery` of terms joined by `OR`; an approximated exception is lost (`ExactExceptions`), a native one is not held |
| Subject contains | ✓ `header :contains "subject"` | ✓ `subjectContains` | ≈ `subject` as a word search, which matches less: as a condition only, lost as an exception (`ExactExceptions`) |
| Subject, sender with name: words (`TextOp::Words`) | ✗ (no word match) | ✗ | ✓ `subject`, `from` (Gmail's own; read so) |
| Subject is / glob / regex | ✓ `:is` / `:matches` / `:regex` (ext `regex`) | ✗ | ✗ |
| Named header contains | ✓ `header :contains "X-…"` | ✗ (`headerContains` searches every header, which would widen the rule; read as `Native`) | ✗ |
| From is / contains | ✓ `address :is` / `:contains "from"` | ✓ `fromAddresses` / `senderContains` | ✗: `from:` is a word search over the name too, which would widen it (changed from ≈, 2026-11-24) |
| From domain | ✓ `address :domain` | ✗: `senderContains "@example.com"` also matches `example.com.evil.test` and `example.community`, which widens the condition (changed from ≈ in review, 2026-11-24) | ✗: `from:(@example.com)` is a word search, which matches `example.com.evil.test` too (changed from ≈ for the same reason as Graph, 2026-11-24) |
| To / Cc / to-or-cc | ✓ `address ["to","cc"]` | to-or-cc ✓ `recipientContains`, `sentToAddresses`; `To` or `Cc` alone ✗ (`RuleLimit::RecipientsTogether`) | ✗: `to:` matches `Bcc` too, so it is read native and nothing typed is written as it (changed from ≈, 2026-11-24) |
| Sent to me (to, cc, only, not) | ✗ for now (≈ address tests over the account's addresses, which the writer does not have) | ✓ `sentToMe`, `sentCcMe`, `sentOnlyToMe`, `notSentToMe`, `sentToOrCcMe` | ✗ for now: `to:me` matches every recipient field, so it is read as a native condition and not written |
| Body contains | ✓ `body :text :contains` (ext `body`) | ✓ `bodyContains` | ✗: a search word in `query` also matches the subject and the senders, which widens the rule (changed from ≈, 2026-11-24) |
| Subject or body | ✓ `anyof(header…, body…)` | ≈ `bodyOrSubjectContains`, read as `Native` (the model has no subject-or-body condition yet) and written back as it came | ✗ for now: a bare search word is read as a native condition |
| Size over / under | ✓ `size :over` / `:under` | ✗ for now: `withinSizeRange` (KB, a range) is read as `Native` until its bounds are proven live | ✓ `size` + `sizeComparison`; a second bound as `larger:` / `smaller:` in `query` (bytes only: what `10M` means is not pinned) |
| Has attachment | ≈ `header :mime :anychild :contains "Content-Disposition" "attachment"` (ext `mime`) | ✓ `hasAttachments` | ✓ `hasAttachment` |
| Importance | ✗ for now (≈ header tests over `Importance::HEADERS`, not yet written) | ✓ `importance` | ✗ (Gmail's `important` is its own classifier) |
| `Native`: Exchange class (`isMeetingRequest`, `isAutomaticReply`, `sensitivity`, `categories`, `messageActionFlag`, …) | ✗ (a few ≈ through headers, not attempted) | ✓ | ✗ |
| `Native`: unparsed Gmail `query` | ✗ | ✗ | ✓ |

### Actions and rule properties

| Neutral | Sieve | Graph | Gmail |
|---|---|---|---|
| `FileInto` (move) | ✓ `fileinto`; by name, or `:mailboxid` (RFC 9042, survives a rename) where listed | ✓ `moveToFolder`, by id only: a Sieve rule names its folder by path, so a migration from Sieve to Graph maps each path to a folder id first, as it maps ids between accounts, or the rule's move is ✗; one move per rule (`RuleLimit::OneActionPerKind`) | ✓ add label + remove `INBOX` (one user label per filter) |
| `FileInto { copy }` | ✓ `fileinto :copy` (ext `copy`, RFC 3894) | ✓ `copyToFolder` | ✓ add label, keep `INBOX` |
| `FileInto { Role(Trash / Junk) }` | ✓ `:specialuse "\\Trash" "Trash"` (RFC 8579), the attribute's name as the fallback folder, for every role but the inbox (`"INBOX"`); ✗ where `special-use` is not listed (no fallback to a name alone) | ✓ `delete` for Trash; the well-known folder's id for Junk, Archive, Sent, Drafts | ✓ `TRASH` / `SPAM`; Archive is `INBOX` removed; Sent, Drafts and a copy into a role ✗ (`TrashJunkArchiveOnly`) |
| `Discard` | ✓ `discard` | ✓ `permanentDelete` | ≈ `TRASH` (recoverable) |
| `Redirect` | ✓ `redirect` (`:copy` keeps it), one per address; a rule's addresses together bounded by `maxNumberRedirects` (two rules that each fit can exceed it for one message; the server enforces that on delivery) | ✓ `redirectTo`, which keeps a copy (`RedirectCopy`); without the copy ✗ | ≈ `forward`: one verified address, original sender kept |
| `Forward` (as the account) | ≈ `redirect :copy` (From differs) | ✓ `forwardTo` | ≈ `forward` |
| `AddKeywords` `$seen` / `$flagged` | ✓ `addflag "\\Seen"` / `"\\Flagged"` (ext `imap4flags`, RFC 5232) | ✓ `markAsRead` / ✗ (no flag action) | ✓ remove `UNREAD` / add `STARRED` |
| `AddKeywords` named | ✓ `addflag "name"` | ≈ `assignCategories`; a category reads typed only where it is a keyword as written (lowercase, no space), else `Native` | ≈ hidden named label, per keyword where the host named one for it (`KeywordName`, `RuleVocabulary::with_keywords`); else ✗; one user label per filter, a folder included (`OneFolderPerRule`) |
| Mark importance (`Action::MarkImportance`) | ≈ `addheader "Importance"` (ext `editheader`) | ✓ `markImportance` | ≈ high: add `IMPORTANT`; low: remove it ("never mark as important"); normal ✗ (`ImportanceHighOrLow`) |
| `Native`: forward as attachment | ✗ | ✓ `forwardAsAttachmentTo` | ✗ |
| `stop` | ✓ `stop` | ✓ `stopProcessingRules` | ✗ (every filter applies) |
| `enabled: false` | ✓ in the dialect, `if false # <test>` (a client that does not know it sees a rule that never runs); ✗ where a value of the test spans lines (`RuleFeature::DisabledMultiLine`): no comment holds it, and Roundcube reads `allof (false, …)` as a rule that runs (observed, the fixture README) | ✓ `isEnabled` | ✗ |
| `name` | ✓ in the dialect, `# rule:[name]` (a comment to any other client) | ✓ `displayName` | ✗ |
| Order | ✓ script order | ✓ `sequence` | ≈ every matching filter applies, and Gmail has no stop, so the label changes and forwards come out the same in any order (a stop is lost on its own) |

### Out-of-office

| Neutral | JMAP | Sieve | Graph | Gmail |
|---|---|---|---|---|
| `enabled` | ✓ | ✓ | ✓ `status` | ✓ |
| `window` | ✓ either end | ✓ `currentdate` (ext `date`) | ✓ `scheduled`, both ends (zoned) | ✓ |
| `reply.subject` | ✓ | ✓ `:subject` | ✗ | ≈ prefix |
| `reply.text` / `html` | ✓ / ✓ | ✓ / ≈ `:mime` | ≈ / ✓ (one HTML string) | ✓ / ✓ |
| text **and** HTML kept apart | ✓ | ≈ `:mime` multipart | ✗ (`ReplyBodies::One`) | ✗: the text is dropped where HTML is set (**live**; changed from ✓, 2026-11-24) |
| `audience` Everyone | ✓ | ✓ | ✓ external `all` | ✓ |
| `audience` Organization | ✗ | ≈ sender-domain test (needs the domains) | ✓ external `none` | ✓ `restrictToDomain` (Workspace) |
| `audience` Contacts | ✗ | ✗ | ≈ `contactsOnly` (external only) | ✓ `restrictToContacts` |
| `audience` OrganizationAndContacts | ✗ | ✗ | ✓ external `contactsOnly` | ✗ |
| `audience` ContactsInOrganization | ✗ | ✗ | ✗ | ✓ `restrictToContacts` and `restrictToDomain` |
| `outside_reply` | ✗ | ≈ sender-domain branch | ✓ `externalReplyMessage` | ✗ |

## 6. Phasing and tests

### Phasing

Each slice is proven live before the next starts (`AGENTS.md`).

1. **Vacation over JMAP. Implemented.** `VacationResponse` in `engine-core`, the `Vacation` trait
   and accessor, `provider-jmap` get/set with `ifInState`. Live on Stalwart, which also settled the
   RFC 9661 §4 question: setting a reply creates a `SieveScript`, it moves the `SieveScript` state,
   and turning it on deactivates a user's script. The answer shapes slice 2 ("Settled by live
   tests").
2. **Rules over JMAP Sieve.** The pure `engine-sieve` crate (segmenter, Roundcube dialect reader,
   renderer, fuzzed beside the existing MIME/iCalendar targets), `plan_rules`, `MailRules` on
   `provider-jmap` (upload, `SieveScript/set` with `ifInState` and `onSuccessActivateScript`; a new
   script is named `rules`). **Implemented.** `SieveScript/validate` is not sent before storing:
   `SieveScript/set` refuses an invalid script whole (proven live both ways), so validating first
   adds a round trip and no safety, and Stalwart's validate cannot see a blob uploaded in the
   same request. Live on Stalwart, which also settled how the rules and the reply coexist.
3. **ManageSieve. Implemented.** The harness publishes 4190 (Stalwart) and 34190 (Dovecot); a
   `managesieve` module of `provider-imap` over the adapter's TLS, SASL and connection budget, at a
   host-configured endpoint; vacation and rules in one script. Live on both, which exercises two
   vocabularies.
4. **Import from Graph. Implemented.** Read `messageRules` and `automaticRepliesSetting` into the
   model; `RulesImporter` on the JMAP and ManageSieve targets; per-rule receipts; the preflight list
   of forwarding rules. The target half: `RulesImporter` and
   `Provider::rules_importer`, the receipts, `held_forwarding`, and both Sieve targets, proven
   live on Stalwart (JMAP and ManageSieve) and Dovecot with a rule set built in the model as Graph
   reads it ("Settled in slice 4"). The source half, the Graph read (`provider-graph`), proven live
   on a work/school tenant, delegated and with application permissions ("Settled for Graph"). A
   rule Graph marks in error imports disabled, and an import knows a rule it already holds
   whatever marks its source puts on it.
5. **Gmail read**, then Graph and Gmail writes for the sync and direct hosts. **Implemented**:
   Graph's writes with Graph's read (`provider-graph`), and Gmail's reads and writes
   (`provider-google`), each proven live on rules and a reply of the test's own ("Settled for
   Graph", "Settled for Gmail"). Gmail is a source only for an import: no `RulesImporter` targets
   it yet.

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
   reported as not kept on a non-Gmail target (section 3). As built (2026-11-24, with the owner):
   `smaller:` too; `to:` stays native (it matches `Bcc`); `from:` and `subject:` read as a word
   match of their own (`TextOp::Words`, the sender with its name), which no other target holds,
   so such a rule is not kept elsewhere rather than carried wider or narrower.
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

**Settled in slice 1**, live against Stalwart v1.0.2 as the scratch account `carol@`, by hand and
then by `provider-jmap/tests/live_vacation.rs` (2026-11-23). Captured, scrubbed, as
`provider-jmap/tests/fixtures/vacation_*.json`.

- **Setting a reply creates a `SieveScript`** (RFC 9661 §4). The first `VacationResponse/set`
  made a script named `vacation`, active, in an account that had none. Its bytes are Stalwart's
  own rendering: `require ["vacation", "relational", "date"];`, the window as two nested
  `currentdate :value "ge"` / `"le" "iso8601"` tests, and `vacation :mime :subject "…"` with a
  `multipart/alternative` body of the text and HTML parts. The `"le"` makes `toDate` inclusive to
  the second, where RFC 8621 §8 says "before" (`docs/spec-deviations.md`, STALWART-4); the neutral
  window keeps RFC 8621's exclusive end. Stalwart's `currentdate` runs in UTC: a probe script
  filing by `currentdate :matches "zone"` filed a delivered message under `+0000`, with an
  `iso8601` value ending in `Z`, so its comparisons with the `Z` dates it writes are sound. A reply
  with only a start, no subject and only a text body renders as one `currentdate "ge"` test and
  `vacation :mime` with a single `text/plain` part. The script stays, inactive, when the reply is
  turned off.
- **A part left out is cleared.** Writing a reply with no subject, only a start and no HTML over a
  fully populated one (every property sent, the absent ones `null`) reads back without the
  subject, the end and the HTML (`vacation_set_clears_response.json`,
  `vacation_get_cleared_response.json`).
- **It moves the `SieveScript` state, and the two types share one state.** Every
  `VacationResponse/set` moved the `SieveScript` state to the reply's new state, and a
  `SieveScript/set` moved the `VacationResponse` state; `get` on both answers the same string
  throughout. Pinned both ways: a reply write guarded by a state read before a script was created
  is refused with `stateMismatch`.
- **Turning the reply on deactivates the user's own script, and turning it off does not switch it
  back.** With a script of the user's active, `VacationResponse/set {isEnabled: true}` left the
  `vacation` script active and the user's inactive (RFC 9661 §2.1: at most one active script).
  Turning the reply off left both inactive: the user's filters stay off until something
  reactivates them. Activating the user's script with `onSuccessActivateScript` while the reply
  was on deactivated the `vacation` script, and `VacationResponse/get` then answered
  `isEnabled: false`: Stalwart reads `isEnabled` from whether that script is the active one.
  Slice 1 does not hide this: the receipt reports `RulesEffect::Deactivated` when the reply goes on
  and `RulesEffect::StillDeactivated` when it goes off with the user's script still off, observed
  from the script list read before and after the write in the same request, so a host can tell the
  user and offer to switch their rules back on. The engine never reactivates a script itself. An
  effect is claimed only where the two listings' states are exactly the write's `oldState` and
  `newState` (the one counter above), so nothing else changed between them; otherwise it is
  `Unknown`.
- **The reply's script is the reply's alone, mostly.** `SieveScript/set` updating its content is
  refused `forbidden` ("use VacationResponse/set instead"), as RFC 9661 §4 requires; destroying it
  while active is refused (`scriptIsActive`). Destroying it once inactive **succeeds**, which RFC
  9661 §4 forbids (`docs/spec-deviations.md`, STALWART-3). The next `VacationResponse/set` makes a
  new one.

**Settled in slice 2**, live against Stalwart v1.0.2 as `carol@`, by hand with delivered mail
(Bob sending, Bob's inbox checked for the auto-reply) and then by
`provider-jmap/tests/live_rules.rs` and `live_rules_reply.rs`. Captured, scrubbed, as
`provider-jmap/tests/fixtures/sieve_*.json` and `vacation_set_kept_off_response.json`.

- **Rules and the reply both run through `include`.** On a JMAP account whose server runs the
  reply as its own script, rules in a separate active script and the reply cannot both run. Of
  the two ways out, the engine takes the first: its rules script starts with
  `include :personal :optional "vacation";` while the reply is on. Delivery showed the included
  `vacation` action fires, and the rules run beside it. The include is first, so a matched rule's
  `stop`, or a `discard`, still leaves the message answered (delivered and seen). `:optional` keeps
  a reply script someone destroyed (STALWART-3) from failing delivery.
- **The server's flag is not the off switch under the include.** The reply's script keeps its
  `vacation` action when `VacationResponse/set` turns it off; only the script stops being active.
  A rules script that includes it runs the reply all the same: a delivery after
  `isEnabled: false` was answered. So the engine turns the reply on and off by adding and removing
  the include, and writes its words with `isEnabled: false`, which stores them and activates
  nothing. A first such write creates the reply's script too (observed).
- **What another JMAP client sees.** On such an account `VacationResponse/get` answers
  `isEnabled: false` while the reply runs, and switching it off there does nothing. Switching it on
  there switches the user's rules off, as before. Rendering the reply into the rules script would
  show another client exactly the same, so it did not decide between the two. The include keeps
  the reply's words in the one object every JMAP client reads, and gives the reply one home rather
  than two. The engine's own read answers on where the reply's script is active, or the running
  rules script includes it: one function (`sieve_scripts::reply_runs`) for the reply and the rules.
- **The reply's script is called `vacation`.** RFC 9661 names none; Stalwart's first
  `VacationResponse/set` creates it under that name. `live_rules_reply` pins it, so a server that
  renames it turns the suite red instead of silently losing replies.
- **The two writes cannot share one request.** Stalwart ignores a result reference for `ifInState`
  (STALWART-7), so the include toggle goes in a second request, guarded by the `newState` the
  reply's write answered. If the first lands and the second does not, the words are stored and
  whether the reply runs is as it was: `requires_confirmation`, read before writing again.
- **`SieveScript/set` is atomic, so nothing validates first.** An invalid create asking to be
  activated stored and activated nothing and left the state; an invalid update of the running
  script left its bytes and its flag (and Stalwart's validate cannot see a blob uploaded in the
  same request, STALWART-6). Stalwart answers `invalidScript` where RFC 9661 says
  `invalidSieve` (STALWART-5); both are `item_unavailable`. Stalwart accepts unknown names in
  `require` silently (STALWART-8), so the proof that it enforces its list uses `imapsieve`,
  which it knows and refuses.
- **A flag after `fileinto` misses the filed copy** (RFC 5232 §3, delivered): the writer puts marks
  first, and reads only that order typed.

**Settled in slice 3**, live against Stalwart v1.0.2 (as `carol@`) and Dovecot 2.4.4 with
Pigeonhole (as `alice@`), by hand and then by `provider-imap/src/managesieve/live_tests.rs` and
`live_provider_tests.rs`. Captured, scrubbed, as `provider-imap/tests/fixtures/managesieve/`.

- **Mechanisms come with TLS.** Both greet with `SASL ""` and offer mechanisms only once `STARTTLS`
  is done: Stalwart `PLAIN OAUTHBEARER XOAUTH2`, Dovecot `PLAIN`. Both take `AUTHENTICATE "PLAIN"`
  with the initial response inline.
- **Dovecot lists `MAXREDIRECTS` only once signed in**, so the vocabulary is read after signing in.
- **A rule written is read back typed, and a stale version is refused**, on both; a reply written is
  read back with its window, subject and text, and a write of the rules keeps it. Reverting the
  version check turned the live suite red.
- **Each refuses an extension it does not list**: Dovecot refuses `editheader` at `CHECKSCRIPT`,
  and the plan keeps no rule that needs it; Stalwart, which lists it, takes it. Stalwart refuses
  `imapsieve` and, as over JMAP, takes an unknown name in `require` (STALWART-8).
- **Refusals carry prose, sometimes a code.** Dovecot answers an invalid script with a literal
  naming its own temporary file and the script's lines; Stalwart with a quoted line and column.
  Neither gives a code there; `NONEXISTENT`, `ACTIVE` and `QUOTA/MAXSIZE` were seen elsewhere. The
  engine keeps the code alone.
- **Stalwart answers `HAVESPACE` yes for any size, and refuses the store**: a 175 KB `PUTSCRIPT`
  is `NO (QUOTA/MAXSIZE)` (STALWART-10). Dovecot answers `HAVESPACE` with the code.
- **A reply rule written over an unread one keeps its blocks and `else`** in a form both servers'
  `CHECKSCRIPT` accept; the bytes Roundcube's settings page stores were captured from the Dovecot
  harness (`engine-sieve/tests/fixtures/roundcube/reply_vacation_post.sieve`).
- **An endpoint that does not answer leaves mail working**: `127.0.0.1:1` as the endpoint connects
  the account, reports `Unavailable` with `Retryable`, and offers neither trait.
- **An account a JMAP client also manages may reply from elsewhere.** On Stalwart, a reply switched
  on over JMAP is Stalwart's own `vacation` script (a `currentdate` test around `vacation :mime`,
  nested for two ends), active; the JMAP adapter's rules script instead runs it through `include`.
  ManageSieve may not write that script: `PUTSCRIPT "vacation"` is refused with no code. Over
  ManageSieve the engine neither reads nor writes the reply where the script it would use runs
  `include`, or holds a `vacation` outside the reply rule
  (`managesieve::vacation::reply_elsewhere`): it cannot read that reply as the account's, and one
  written beside it would make two. Proven on
  Stalwart with the reply set over JMAP, in both shapes, nothing written; the guard reverted, the
  `include` shape read as off and took a write. The rules stay readable and writable: Stalwart's reply
  script is never the rules' script, so they go in a `rules` script that includes it first and
  runs, and JMAP reads them back from there (live). A script that is chosen and still holds a
  `vacation` no `# rule:[…]` name marks, one written by hand, is refused rather than rewritten
  (`engine_sieve::dialect::reply::unnamed_vacation`). Stalwart refuses `PUTSCRIPT "vacation"`
  itself.

**Settled in slice 4 (the target half)**, live against Stalwart v1.0.2 (JMAP and ManageSieve, as
`carol@`) and Dovecot 2.4.4 with Pigeonhole (ManageSieve, as `alice@`), by
`provider-jmap/tests/live_rules_import.rs` and `provider-imap/src/managesieve/live_import_tests.rs`.
Captured, scrubbed, as `provider-jmap/tests/fixtures/mailbox_get_rule_folders_response.json`.

- **`Mailbox/get` for named ids answers `notFound` for one the account lacks**, beside the one it
  has, in one call; the import loses that rule's action.
- **Imported rules land after the account's own, which the server then holds byte for byte**, on
  all three: `require` grows, and an include of a reply that does not run, in a rules script that
  is switched off, is left out (JMAP, live). A rule set built as Graph reads it imports as planned: an
  importance mark is `addheader` on Stalwart (`Approximated`) and lost on Dovecot, a rule with an
  Exchange-only exception is not kept, a redirect arrives `if false #`-disabled and reads back
  typed and disabled, and `held_forwarding` lists exactly it.
- **A reply imports where none is held, again for nothing, and is refused over another**, on all
  three; on JMAP through the include beside the running rules script, on ManageSieve as the reply
  rule of the same script.

**Settled for Graph**, live against a Microsoft 365 work account and one of its shared mailboxes
(2026-11-24): reads by hand and then by `provider-graph/tests/live_rules.rs`; one run of
`live_rules_writes.rs` on rules of the test's own, every other rule left byte for byte as it was.
Captured, scrubbed, as `provider-graph/tests/fixtures/rules/`.

- **No ETag, and `If-Match` is ignored.** Neither the rules' listing nor a rule read alone carries
  `@odata.etag`, the reply's setting has none, and a rule `PATCH` with a stale `If-Match` lands:
  both guards are `Absent`, with a version read again before writing.
- **Writes are applied after they are answered.** The listing did not yet show a rule `POST`ed a
  moment earlier, so a base read before it passed the version check; and of a reply written and
  restored a few seconds apart, the reads straight after the restore showed it while reads minutes
  later showed the write (restored by hand, then read back unchanged for fifteen minutes). Ordering
  between two quick `mailboxSettings` writes is not guaranteed, a read-back proves nothing, and
  neither receipt names a version.
- **`sequence` is a position.** Dense (the rules are 1 to n), renumbered on every change, clamped:
  a `PATCH` to 29 among 11 rules answered and listed 11. A `PATCH` of `sequence` alone works.
- **A `PATCH` clears nothing.** `markImportance` and `displayName` sent as `null` stayed, with a
  `200`. A change that drops a part is therefore written as a new rule in its place.
- **Every rule has a name.** A `POST` without `displayName` is accepted (`201`) and Graph names the
  rule itself.
- **Ids are base64 with `=`**, sent as `%3D`, which works; none of the 9 read or the test's own held
  `/` or `+`.
- **Who may read.** A delegated token needs `MailboxSettings.Read`: a personal account's token
  without it answered `403 ErrorAccessDenied` on both. An application token holding only
  `MailboxSettings.Read` reads a user's rules and reply and a shared mailbox's at
  `/users/{address}`, where a delegated token is refused the shared mailbox's settings
  (`graph.md`); its reading of the user's rules was the delegated one byte for byte.
- **What Graph-read rules are.** Nine rules of a real mailbox (subject, body, sender and
  attachment tests, an exception, moves, read marks, a forward, a redirect, categories, disabled
  rules) all plan `Whole` against Graph's own vocabulary, so a write of the rules as read sends
  nothing; with the old redirect rule (a copy needing `Redirect` too) the redirecting rule would
  not have.
- **`$top` is ignored** on the listing: asked for two, it answered nine and no next link. The
  adapter follows a next link all the same.
- **The schedule is in UTC** on both mailboxes read; another zone is resolved through the tzdb, a
  skipped or repeated wall time never widening the window.
- **Writes, settled, proven on the test's own rules**: a rotation of three by position, a list
  shortened by the engine (written anew in place), a rename in place, a stale base refused once the
  listing settled; and a reply scheduled for 2099, settled, restored and still matching five
  minutes later. A `PATCH` replaces a list sent shorter, where it keeps a part sent as `null`.
- **An unscheduled reply reports a rolling schedule**: from the top of the current hour to a day
  later, moving every hour. The version leaves the two ends out unless the reply is scheduled.

**Settled for Gmail**, live against a throwaway personal Google account (2026-11-24): reads by
`provider-google/tests/live_rules.rs`; writes by `live_rules_writes.rs` on filters of the test's own
and `live_reply_writes.rs` on a reply put back byte for byte, every other filter left as it was.
Each claim below is asserted by those suites, not only printed. Captured as
`provider-google/tests/fixtures/mail/settings_filters.json` and `settings_vacation_off.json`.

- **No write lag.** A filter created or deleted, and a reply written, showed on the very next read,
  with no read flapping back, in every run. Neither receipt carries a version all the same: the
  guard is `Absent` and the next write reads first.
- **An empty list is `204` with no body.** `filters.list` and `forwardingAddresses.list` on an
  account with none answer `204 No Content`, not `{}`; the transport reads it as `null`.
- **Gmail keeps a filter as sent**: criteria and actions read back exactly as created (any-of values
  as `a OR b`, the exceptions as one `negatedQuery`, the size a number), and a query and
  `negatedQuery` bracketed as the writer brackets a native one, `(…) term` and `(…) OR term`, are
  stored as sent. That the brackets group as a search does is Gmail's search documentation.
- **What Gmail refuses**, each sent by hand once the engine had held it back: a forward to an
  address not verified for forwarding (`400 failedPrecondition`; the writer's check,
  `ForwardNotAllowed`), a filter adding two of the user's labels (`400 invalidArgument`;
  `RuleLimit::OneFolderPerRule`), a filter with no criteria (`400 invalidArgument`;
  `RuleLimit::SomeTest`), and a second filter identical to one it holds (`400 failedPrecondition`;
  the writer keeps the one held and sends nothing, proven through the engine too). A filter whose
  only criterion is a `negatedQuery` is stored: a rule with exceptions alone tests something.
- **A plain size is bytes.** `larger:` and `smaller:` with a message's own size halved and doubled
  find it above the one and below the other, through the same search a filter's query is.
- **One body for the reply.** A reply sent by hand with both `responseBodyPlainText` and
  `responseBodyHtml` reads back with the HTML alone: Gmail's vocabulary is `ReplyBodies::One`, and
  a reply with both is refused before sending. A part left out of the `PUT` is cleared. The window
  is kept to the millisecond, as written (start rounded up, end down). A reply never set reads as
  `enableAutoReply: false`, an empty subject, and nothing else.
- **An edit that is not kept leaves the filter as read**, and the receipt names it (section 4).

**Follow-ups for Gmail.**

- **An importer into Gmail must revisit what sends mail.** The reader marks every Gmail action but
  `forward` as sending none, an action key Gmail adds later included. That holds only while no
  `RulesImporter` targets Gmail: with one, such a key that sends mail would be armed with
  forwarding off and missing from `held_forwarding`.

- **Sent to me** (`to:me`), the recipients (`to:`), and a typed subject-or-body condition stay
  native for now; `larger:` with a unit (`10M`) stays native until its bytes are pinned.
- **A quoted `me`** is refused both ways (read native, never written) until a live search shows it
  is a word rather than the account.
- **`restrictToDomain` on a personal account** was not sent: the vocabulary keeps it to Workspace
  accounts, read from `userinfo`; a token that may not ask (`401`, `403`) gets the personal one.
- **The word-match semantics** of a filter (what `from:` and `subject:` match, that brackets group)
  are from Gmail's search documentation and the stored form, not from delivered mail.
- **Two native parts with one key** (a host joining Gmail fragments by hand) are refused by the
  writer, which the plan cannot see; a set read from Gmail never holds them.

**Follow-ups for Graph.**

- **A shortened list as a `PATCH`**: Graph replaces a list a `PATCH` sends shorter (observed), so
  the rule need not be written anew for that; it is, for now, which is proven too.
- **A typed subject-or-body condition**, so `bodyOrSubjectContains` (a common Outlook condition)
  carries to Sieve as `anyof (header, body)` instead of staying `Native`.
- **Size ranges** typed, once a one-sided `withinSizeRange` and its kilobyte rounding are proven.
- **A rule id holding `/`**, never seen: Graph re-parses a decoded `%2F`, so such an id is unproven.

**Follow-ups from slice 3.**

- **An HTML reply over Sieve**, as `vacation :mime` with a `multipart/alternative` body (what
  Stalwart's own JMAP rendering writes); Roundcube reads such a reply as raw MIME, so it is not
  written yet, and a `:mime` reply reads as one the engine cannot show.
- **SRV discovery of the endpoint** (RFC 5804 §1.8).
- **An authorization identity over ManageSieve** (`Credential::ActingAs`, a Dovecot master user),
  proven live: refused before sending for now, as SMTP refuses it.

**Follow-ups from slice 2.**

- **Switching stored rules on.** A write keeps rules where they are, running or not
  (`RuleSet::activation`); the engine never activates a script the user left off, as it never
  reactivates one the reply switched off. A host that wants to offer "switch your rules on" needs
  a deliberate verb for it, not yet designed.
- **The reply's script name.** The include names `vacation`, Stalwart's; RFC 9661 names none.
  Another JMAP server with a reply script of another name leaves the include naming nothing
  (`:optional`) until its name is known; the condition the adapter checks is in `jmap.md`.

**Still to settle:**

- **The Graph items marked unverified** (slices 4 and 5): the Exchange rules quota, and OR within
  one predicate.
