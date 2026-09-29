#!/usr/bin/env bash
# Fail if a tracked source or doc file names a real-world domain in an email address.
#
# AGENTS.md hard rule: "Identifiers in fixtures and docs use reserved names."
#
# This exists because of a near miss. A CalDAV reply-delivery bug was diagnosed against a
# live account, and the regression fixtures were built — correctly — from the *observed
# bytes* that server returned. Those bytes contained the developer's own address, and they
# reached a public repository's history. Anonymising after the fact does not undo it: a
# force-push moves a ref, but the old commit is still served by SHA.
#
# So the rule is positive, not a denylist: a denylist has to enumerate every private domain
# in the world, and would have to name the very addresses it is protecting. An allowlist of
# reserved names fails closed on the domain nobody thought of.
#
# Reserved names, and what reserves them:
#   example.com / .net / .org      RFC 2606 §3
#   .test .example .invalid        RFC 2606 §2   (.localhost too)
#   .local                         RFC 6762 §3   (the harness accounts: carol@test.local)
#
# Run from the repo root:
#
#     scripts/ci/check-fixture-identifiers.sh
#
# `git ls-files` sees only tracked files, so `target/` and untracked scratch work are out of
# scope by construction — but so is a new fixture you have not staged yet. Stage it, or run
# this after `git add`.
set -euo pipefail

# Match bytes, not characters, so this script's verdict is a property of the repository
# and not of whoever runs it.
#
# `[A-Za-z]` in an ERE is a *collating range*, and glibc resolves it through the locale:
# under `en_US.UTF-8` it matches `Ü`, under `C`/`C.UTF-8` it does not. That is not a
# nitpick — it made `Team@BÜCHER.example` (a reserved `.example` name, so legitimate)
# fail locally on a developer machine while passing in CI, which reads exactly like the
# branch under test broke something it never touched. Pinning the locale makes both
# answers the same answer.
export LC_ALL=C

# Non-ASCII bytes. Under `LC_ALL=C` a UTF-8 character is just its bytes, all of them
# above 0x7F, so one range covers every accented letter and every IDN label without the
# script needing to know any of them. Written as a variable because the ranges below
# would otherwise be unreadable, and spliced in **last** in each bracket expression so a
# literal `-` cannot be read as the start of a range.
HIGH=$'\200-\377'

# An email-shaped token. The trailing TLD must *begin* with a letter, which is what keeps
# version strings (`pkg@1.2.3`) from matching; the rest of it admits digits and hyphens so
# a punycode TLD (`xn--zckzah`) is matched whole rather than truncated at the first `-`.
#
# Non-ASCII bytes are admitted everywhere an ASCII letter is, so the check **fails
# closed** on an internationalized address at a real IDN domain. Excluding them would
# make the script silently blind to exactly the addresses hardest to notice by eye — the
# opposite of the fail-closed design the reserved-name allowlist exists for.
ADDR_RE="[-A-Za-z0-9._%+${HIGH}]+@[-A-Za-z0-9.${HIGH}]+\.[A-Za-z${HIGH}][-A-Za-z0-9${HIGH}]+"

# A domain that is reserved for documentation and testing, and can therefore never be
# someone's real mailbox. Subdomains of each are equally reserved — including non-ASCII
# ones, since a `.example` name is reserved whatever script its labels are written in.
# The reserved suffixes are ASCII apart from the IDN pair below, so the case-folding
# further down reaches them.
#
# `テスト` (punycode `xn--zckzah`) is IANA's Japanese IDN **test** TLD — the
# internationalized counterpart of `.test`, delegated for IDN evaluation and never sold
# to registrants. It is listed here rather than in `exempt()` because it is genuinely a
# reserved name: anything under it is as safe as anything under `.test`, so the rule
# generalizes instead of naming one fixture.
RESERVED_RE="^([-a-z0-9${HIGH}]+\\.)*(example\\.(com|net|org)|test|invalid|example|localhost|local|テスト|xn--zckzah)$"

# The narrow set of real domains that legitimately appear, each for a stated reason. Keep
# this list short and keep the reasons — an entry with no reason is one nobody can retire.
exempt() {
  case "$1" in
    # Microsoft Graph OData annotations (`start@odata.type`). Not an address at all; it only
    # matches because the syntax reuses `@`.
    *@odata.*) return 0 ;;
    # Google's own identifier formats: an event's iCalUID is `<opaque-id>@google.com`, a
    # calendar id may be `<name>@group.v.calendar.google.com`, and Gmail rewrites a sent
    # message's `Message-ID` to `<opaque-id>@mail.gmail.com` (which is why the submit path
    # cannot reconcile by the id it generated). Rewriting these would make the fixtures and
    # the prose describe a wire format Google does not emit.
    *@google.com | *@group.v.calendar.google.com | *@mail.gmail.com) return 0 ;;
    # Company-owned mailboxes used by the live provider suites. Real, but ours, and named in
    # the live-test setup docs rather than belonging to a person.
    allodia.e2e@gmail.com | allodia-e2e@outlook.com) return 0 ;;
    # The project's published contact address (REUSE.toml, and the search fixtures that
    # tokenize it). Already public by intent.
    info@allodia.eu) return 0 ;;
  esac
  return 1
}

fail=0

# Vendored upstream (docker/sabredav) is excluded: it is third-party PHP carrying its own
# authors' addresses, and rewriting it would fork the dependency.
while IFS= read -r file; do
  [ -f "$file" ] || continue
  while IFS= read -r addr; do
    [ -n "$addr" ] || continue
    # ASCII-only case folding, which is all that is needed: every reserved suffix is
    # ASCII, and a non-ASCII label matches the byte range either way.
    lower=$(printf '%s' "$addr" | tr 'A-Z' 'a-z')
    domain=${lower#*@}
    # A fixture filename (`evt-1@test.local.ics`) is a file, not a mailbox.
    domain=${domain%.ics}
    domain=${domain%.eml}
    if printf '%s' "$domain" | grep -qE "$RESERVED_RE"; then
      continue
    fi
    if exempt "$lower"; then
      continue
    fi
    printf '  %s: %s\n' "$file" "$addr"
    fail=1
  done < <(grep -ohE "$ADDR_RE" "$file" 2>/dev/null || true)
done < <(git ls-files -- 'crates/*' 'docs/*' 'scripts/*' 'docker/*' '*.md' '*.toml' \
  ':(exclude)Cargo.lock' ':(exclude)docker/sabredav/*')

if [ "$fail" -ne 0 ]; then
  cat >&2 <<'MSG'
ERROR: the address(es) above use a real domain.

Fixtures and docs must use names reserved for the purpose, so that nothing in this public
repository can be someone's mailbox:

  example.com / example.net / example.org      RFC 2606 §3
  anything.test / .example / .invalid          RFC 2606 §2
  carol@test.local                             the harness accounts

If a real domain is structurally required (a provider's own identifier format, say), add it
to `exempt()` in this script WITH the reason.
MSG
  exit 1
fi

echo "OK: every email-shaped identifier in tracked sources uses a reserved name."
