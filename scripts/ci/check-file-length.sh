#!/usr/bin/env bash
# Fail if any tracked Rust source file exceeds the line ceiling.
#
# AGENTS.md hard rule: "Files must stay under 500 lines. Split by responsibility."
# rustfmt and clippy have no per-file length lint, so this script is the machine
# enforcement of that rule — wired into CI and runnable locally from the repo root:
#
#     scripts/ci/check-file-length.sh
#
# `git ls-files` lists only tracked files and never descends into the gitignored
# `target/` dir, so the check sees exactly this repo's own Rust sources.
set -euo pipefail

MAX=500
fail=0

while IFS= read -r file; do
  lines=$(wc -l <"$file")
  if [ "$lines" -gt "$MAX" ]; then
    printf '  %s: %d lines\n' "$file" "$lines"
    fail=1
  fi
done < <(git ls-files '*.rs')

if [ "$fail" -ne 0 ]; then
  echo "ERROR: the file(s) above exceed the ${MAX}-line limit — split them by responsibility." >&2
  exit 1
fi

echo "OK: every tracked *.rs file is within the ${MAX}-line limit."
