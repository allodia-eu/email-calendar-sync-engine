#!/usr/bin/env bash
# Make what you are about to build, and judge, this checkout's own.
#
# `.cargo/config.toml` points `build.build-dir` at one directory every checkout shares, so the
# dependency compile is paid for once rather than once per worktree. Cargo cannot tell those
# checkouts apart: it names an artifact, and the fingerprint guarding it, from the workspace-
# RELATIVE path, so they all land on the same `deps/<crate>-<hash>` and the same fingerprint entry,
# and freshness then comes down to mtimes against a relative file list. A checkout whose sources
# predate the build another checkout last ran is declared fresh and is handed that checkout's
# binary. A suite then reports `ok` for an assertion this tree cannot satisfy, and a worktree
# created before another worktree built is exactly the shape that collects it.
#
# So this leaves a marker naming the checkout whose artifacts are in there, and runs
# `cargo clean --workspace` when the marker names someone else. That drops the MEMBERS' artifacts
# and leaves every dependency compiled, which is the whole of what the sharing buys: two checkouts
# cannot both have their build in one entry, so sharing the members was never buying anything.
#
# It costs a rebuild of this repository's own crates after a switch, and only where the answer
# would otherwise have been wrong: the checkout that built here last claims it again for nothing.
#
# Run it before the gate (AGENTS.md → "Building & verifying"). Best effort throughout: every
# failure here leaves the build to go ahead, because a gate that refuses to start over its own
# bookkeeping is worse than the reuse it guards against.
#
#     scripts/dev/claim-build-dir.sh
set -uo pipefail

MARKER=.allodia-checkout

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

# Asked of cargo rather than read out of the config file, because the setting takes placeholders
# and an ancestor config or the environment can override it.
meta=$(cargo metadata --format-version 1 --no-deps 2>/dev/null) || exit 0
build_dir=$(printf '%s' "$meta" |
  sed -nE 's/.*"build_directory":"((\\.|[^"\\])*)".*/\1/p' |
  sed -E 's/\\(.)/\1/g')
[ -n "$build_dir" ] || exit 0

# Inside the checkout: nothing else can be holding it, which is also the shape CI runs in.
case "$build_dir" in
"$root" | "$root"/*) exit 0 ;;
esac

held=$(cat "$build_dir/$MARKER" 2>/dev/null)
if [ "$held" = "$root" ]; then
  exit 0
fi

echo "== the shared build directory last held another checkout's build, so this repository's own"
echo "   crates are rebuilt here before anything is judged ($build_dir)"
if ! cargo clean --workspace --quiet; then
  echo "!! could not clean the workspace: what follows may report on another checkout's build" >&2
  exit 0
fi

# Written only once the clean succeeded, so a failure is claimed by nobody and the next run tries
# again rather than trusting what this one could not replace.
printf '%s' "$root" >"$build_dir/$MARKER" 2>/dev/null ||
  echo "!! could not record this checkout in $build_dir" >&2
