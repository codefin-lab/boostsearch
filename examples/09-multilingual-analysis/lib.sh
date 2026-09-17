# Shared helpers. Each example carries its own copy of this file, so an
# example directory can be taken away on its own and still run. Source it;
# do not run it.
#
#   VS    where the server listens (default http://127.0.0.1:9200)
#   AUTH  curl credentials, for the examples that turn security on
#
# Every example is written to be run twice: it deletes what it creates before
# it creates it, so a failed run leaves nothing in the way of the next one.

set -euo pipefail

# every path in an example is relative to the example's own directory
cd "$(dirname "${BASH_SOURCE[0]}")"

# .env is the example's own defaults; anything already in the environment wins,
# so `VS=... ./run.sh` is not silently overridden by a file
_vs_from_env="${VS-}"
_auth_from_env="${AUTH-}"
[ -f .env ] && . ./.env
[ -n "$_vs_from_env" ] && VS="$_vs_from_env"
[ -n "$_auth_from_env" ] && AUTH="$_auth_from_env"

VS="${VS:-http://127.0.0.1:9200}"
AUTH="${AUTH:-}"
CURL=(curl -sS --fail-with-body)
[ -n "$AUTH" ] && CURL+=(-u "$AUTH")

_n=0

step() { _n=$((_n + 1)); printf '\n\033[1m== %d. %s\033[0m\n' "$_n" "$*"; }
note() { printf '   %s\n' "$*"; }

# req METHOD PATH [BODY] -- a JSON request written inline, its answer printed
req() {
  local m=$1 p=$2 b=${3-}
  if [ -n "$b" ]; then
    "${CURL[@]}" -X "$m" "$VS$p" -H 'Content-Type: application/json' -d "$b"
  else
    "${CURL[@]}" -X "$m" "$VS$p"
  fi
  echo
}

# reqf METHOD PATH FILE -- the same, with the body read from requests/
reqf() {
  "${CURL[@]}" -X "$1" "$VS$2" -H 'Content-Type: application/json' --data-binary "@$3"
  echo
}

# quiet METHOD PATH [BODY] -- a request whose answer is thrown away
quiet() { req "$@" > /dev/null; }

# quietf METHOD PATH FILE
quietf() { reqf "$@" > /dev/null; }

# gone PATH -- delete something that may or may not be there
gone() { "${CURL[@]}" -X DELETE "$VS$1" > /dev/null 2>&1 || true; }

# ndjson PATH FILE -- a bulk-shaped body from a file
ndjson() {
  "${CURL[@]}" -X POST "$VS$1" -H 'Content-Type: application/x-ndjson' --data-binary "@$2"
  echo
}

# bulk PATH -- a bulk-shaped body from standard input
bulk() {
  "${CURL[@]}" -X POST "$VS$1" -H 'Content-Type: application/x-ndjson' --data-binary @-
  echo
}

# clip [N] / clipl [N] -- the first N characters, or lines, of an answer nobody
# needs all of. `head` closes the pipe as soon as it has enough, which sends
# SIGPIPE upstream, and under `set -o pipefail` that fails the whole script for
# a request that in fact succeeded. These read everything and then trim.
clip() {
  python3 -c 'import sys; n = int(sys.argv[1]); d = sys.stdin.read()
sys.stdout.write(d[:n] + (" ...\n" if len(d) > n else "\n"))' "${1:-400}"
}
clipl() {
  python3 -c 'import sys; n = int(sys.argv[1]); ls = sys.stdin.read().rstrip("\n").split("\n")
sys.stdout.write("\n".join(ls[:n]) + ("\n   ...\n" if len(ls) > n else "\n"))' "${1:-20}"
}

# --- the checks that keep an example from passing while doing nothing -------
#
# A request can succeed and still leave no data: a bulk answers 200 with every
# item failed, a reindex answers 200 with a `failures` array, a pipeline drops
# every document into a dead-letter index. An example that prints an empty
# result and exits 0 is the same kind of lie as a test that asserts nothing,
# so the examples say what they expect and stop when it is not there.

_fails=0

# docs INDEX -- how many documents it holds, 0 if it is not there
docs() {
  "${CURL[@]}" "$VS/$1/_count" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("count", 0))
except Exception: print(0)'
}

# expect_docs INDEX N [what] -- exactly N, or the example stops here
expect_docs() {
  local got; got=$(docs "$1")
  if [ "$got" = "$2" ]; then
    printf '   \033[32mok\033[0m  %s holds %s document%s%s\n' "$1" "$got" \
      "$([ "$got" = 1 ] || echo s)" "${3:+ -- $3}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s holds %s, expected %s%s\n' \
      "$1" "$got" "$2" "${3:+ -- $3}" >&2
    _fails=$((_fails + 1))
  fi
  return 0
}

# expect_at_least INDEX N [what]
expect_at_least() {
  local got; got=$(docs "$1")
  if [ "$got" -ge "$2" ] 2>/dev/null; then
    printf '   \033[32mok\033[0m  %s holds %s document%s%s\n' "$1" "$got" \
      "$([ "$got" = 1 ] || echo s)" "${3:+ -- $3}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s holds %s, expected at least %s%s\n' \
      "$1" "$got" "$2" "${3:+ -- $3}" >&2
    _fails=$((_fails + 1))
  fi
  return 0
}

# expect_hits N METHOD PATH BODY [what] -- a search that must find N documents
expect_hits() {
  local want=$1 m=$2 p=$3 b=$4 what=${5-}
  local got
  got=$("${CURL[@]}" -X "$m" "$VS$p" -H 'Content-Type: application/json' -d "$b" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["hits"]["total"]["value"])
except Exception: print(-1)')
  if [ "$got" = "$want" ]; then
    printf '   \033[32mok\033[0m  %s hit%s%s\n' "$got" "$([ "$got" = 1 ] || echo s)" "${what:+ -- $what}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s hits, expected %s%s\n' "$got" "$want" "${what:+ -- $what}" >&2
    _fails=$((_fails + 1))
  fi
  return 0
}

# done_ -- the last line of an example: say whether its own checks held
done_() {
  echo
  if [ "$_fails" = 0 ]; then
    printf '\033[1;32mRESULT\033[0m every check in this example held\n'
  else
    printf '\033[1;31mRESULT\033[0m %s check%s did not hold\n' "$_fails" "$([ "$_fails" = 1 ] || echo s)" >&2
    return 1
  fi
}

# green INDEX -- wait for the index to be ready to answer
green() {
  local i=0
  until "${CURL[@]}" "$VS/_cluster/health/$1?wait_for_status=yellow&timeout=5s" > /dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -gt 12 ] && { echo "the index never went yellow" >&2; return 1; }
    sleep 1
  done
}

alive() {
  "${CURL[@]}" "$VS" > /dev/null 2>&1 || {
    echo "no server at $VS -- see this example's README, or set VS" >&2
    exit 1
  }
}
alive
