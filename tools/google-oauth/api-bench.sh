#!/usr/bin/env bash
#
# api-bench.sh — probe Gmail's API directly to answer the questions that decide the
# adapter's fetch shape, none of which an offline fixture can answer:
#
#   1. What does one `messages.get` actually cost, and is that cost the network or Google?
#   2. How far does concurrency scale before Gmail starts answering 429?
#   3. Is the batch endpoint a way around the limits, or into them sooner?
#   4. What does each `format` weigh, and does gzip earn its header?
#
# This probes the API, not the adapter. For the adapter's own throughput through the real
# code path, use the gated `live_throughput` test in `crates/provider-google/tests/`.
#
# Reads the account already signed in through this tool. From the repo root:
#
#   tools/google-oauth/api-bench.sh
#
# Everything here is read-only: it lists message ids and fetches them. It never writes,
# labels, or deletes. Repeating a fetch of the same id is deliberate where a probe needs
# more calls than the mailbox has messages.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

TOK="$(cd "$ROOT" && cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)"
if [ -z "$TOK" ]; then
  echo "no access token — run 'cargo run --manifest-path tools/google-oauth/Cargo.toml -- login' first" >&2
  exit 1
fi

API="https://gmail.googleapis.com/gmail/v1/users/me"
BATCH="https://gmail.googleapis.com/batch/gmail/v1"
AUTH=(-H "Authorization: Bearer $TOK")
# The envelope headers the adapter asks for (`normalize::METADATA_HEADERS`).
MD="format=metadata"
for h in From To Cc Bcc Subject Date Message-ID In-Reply-To References; do
  MD="$MD&metadataHeaders=$h"
done

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
now() { python3 -c 'import time;print(time.time())'; }

# ---------------------------------------------------------------- ids
curl -s --http2 "${AUTH[@]}" -o "$WORK/list.json" \
  "$API/messages?maxResults=100&includeSpamTrash=true"
python3 - "$WORK/list.json" "$WORK/ids.txt" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
ids=[m["id"] for m in d.get("messages",[])]
open(sys.argv[2],"w").write("\n".join(ids))
print(f"mailbox sample: {len(ids)} id(s)"
      + ("  (more pages exist)" if d.get("nextPageToken") else ""))
PY
N=$(grep -c . "$WORK/ids.txt")
ID=$(head -1 "$WORK/ids.txt")
[ "$N" -gt 0 ] || { echo "no messages to probe" >&2; exit 1; }

# ------------------------------------------------- 1. where the time goes
say "1. one messages.get — is the cost the link or the service?"
for _ in 1 2 3 4 5; do
  curl -s --http2 "${AUTH[@]}" -o /dev/null \
    -w "   connect %{time_connect}s   ttfb %{time_starttransfer}s   total %{time_total}s   http/%{http_version}\n" \
    "$API/messages/$ID?$MD"
done
echo "   (a ttfb far above connect is Google's own latency: concurrency is the only lever)"

# ------------------------------------------------- 2. concurrency scaling
: > "$WORK/urls.txt"
while read -r id; do
  printf 'url = "%s/messages/%s?%s"\noutput = "/dev/null"\n' "$API" "$id" "$MD" >> "$WORK/urls.txt"
done < "$WORK/ids.txt"

say "2. concurrency scaling over one HTTP/2 connection ($N messages)"
for c in 1 5 10 20 40; do
  s=$(now)
  curl -s --http2 "${AUTH[@]}" --parallel --parallel-immediate --parallel-max "$c" \
    -K "$WORK/urls.txt" -w '%{http_code}\n' 2>/dev/null > "$WORK/codes.txt"
  e=$(now)
  python3 - "$WORK/codes.txt" "$c" "$N" "$s" "$e" <<'PY'
import sys,collections
t=collections.Counter(l.strip() for l in open(sys.argv[1]) if l.strip())
c,n,el=sys.argv[2],int(sys.argv[3]),float(sys.argv[5])-float(sys.argv[4])
print(f"   {c:>3} in flight: {el:6.2f}s   {el/n*1000:5.0f} ms/msg   "
      f"{n/el:5.1f} msg/s   {dict(t)}")
PY
done

# ------------------------------------------------- 3. the batch endpoint
mkbatch() { # mkbatch <n> <query> <outfile>
  python3 - "$WORK/ids.txt" "$1" "$2" "$3" <<'PY'
import sys
ids=[l for l in open(sys.argv[1]).read().split("\n") if l]
n=int(sys.argv[2]); q=sys.argv[3]
ids=(ids*((n//len(ids))+1))[:n]
b="batch_probe"
parts=[f"--{b}\r\nContent-Type: application/http\r\nContent-ID: <i{i}>\r\n\r\n"
       f"GET /gmail/v1/users/me/messages/{m}?{q}\r\n\r\n" for i,m in enumerate(ids)]
parts.append(f"--{b}--\r\n")
open(sys.argv[4],"w",newline="").write("".join(parts))
PY
}

say "3. the batch endpoint — one request, n sub-requests"
for n in 10 50 100; do
  [ "$n" -gt 0 ] && mkbatch "$n" "$MD" "$WORK/b.txt"
  curl -s --http2 "${AUTH[@]}" -X POST \
    -H "Content-Type: multipart/mixed; boundary=batch_probe" \
    --data-binary "@$WORK/b.txt" -o "$WORK/bo.txt" \
    -w "   n=$n: http %{http_code}  %{time_total}s  %{size_download}B" \
    "$BATCH"
  echo "   sub-statuses: $(grep '^HTTP/1.1' "$WORK/bo.txt" | awk '{print $2}' | sort | uniq -c | tr '\n' ' ')"
done
echo "   (a batch of n counts as n requests — the throttled sub-responses above are the"
echo "    proof. Whether batch or a fan-out throttles first depends on the cumulative rate"
echo "    already spent, not on the shape, so compare them at equal width and equal history:"
echo "    crates/provider-google/tests/live_batch_vs_concurrent.rs does exactly that)"

# ------------------------------------------------- 4. payload shape
say "4. payload weight per format, and what gzip saves (one sampled message)"
for f in "format=minimal" "$MD" "format=full" "format=raw"; do
  curl -s --http2 "${AUTH[@]}" -o /dev/null \
    -w "   ${f%%&*}:  %{time_total}s  %{size_download} bytes\n" "$API/messages/$ID?$f"
done
curl -s --http2 "${AUTH[@]}" -H "Accept-Encoding: gzip" -H "User-Agent: probe (gzip)" \
  -o /dev/null -w "   format=raw + gzip:  %{size_download} bytes on the wire\n" \
  "$API/messages/$ID?format=raw"
echo "   (Gmail wants 'gzip' in the User-Agent as well as the Accept-Encoding header.)"
echo "   (These weights are one message's; gzip saves proportionally more on a large body,"
echo "    so read the ratio rather than the absolute numbers.)"
