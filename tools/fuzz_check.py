#!/usr/bin/env python3
"""Malformed input at everything that parses: does the node still answer?

Two reviews found ten ways to end the process or a request with one body: a
chain of `!` in a script, a `NOT` chain in SQL, a value that holds itself, a
character cut in half by a byte offset, a pattern that expands factorially. All
of them were reachable through an ordinary endpoint and none of them were in any
suite, because a suite asks for the right answer to a sensible question.

This asks nonsense. Every parser and analyser the server has -- Painless, SQL,
PPL, the query DSL, mustache templates, grok, date math, time zones, the
analysers, `filter_path`, the aggregations -- is given deep nesting, long
repetition, non-ASCII in the awkward places, numbers at the edges of their types,
and structures that point at themselves. What it must do is answer: any status
at all, within the timeout, with the node still alive afterwards.

    tools/fuzz_check.py                    # a few thousand probes
    tools/fuzz_check.py --rounds 20000 --seed 7

A probe that gets no answer (the connection dropped: a panic), or that takes
longer than the timeout (work with no ceiling), or that leaves the node dead, is
printed with the body that caused it.
"""

import argparse
import json
import os
import pathlib
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent

# characters that have caught something before: a letter with an accent, a
# character outside the basic plane, a space that is not a space, a combining
# mark, a right-to-left mark, and one that changes width when it is lowercased
AWKWARD = [
    "\u00e9",  # a letter with an accent
    "\u6771",  # a character three bytes wide
    "\U0001f600",  # one outside the basic plane, two UTF-16 units
    "\u00a0",  # a space that is not an ASCII space
    "\u0301",  # a combining mark, which belongs to the letter before it
    "\u200f",  # a mark with no width at all
    "\u212a",  # the Kelvin sign: one byte shorter once it is lowercased
    "\u00df",  # the sharp s, which uppercases into two letters
    "\ufb01",  # a ligature, which normalises into two
    "\u3000",  # an ideographic space
]
EDGES = [
    0,
    1,
    -1,
    127,
    128,
    32767,
    2147483647,
    -2147483648,
    9223372036854775807,
    -9223372036854775808,
    18446744073709551615,
    1e308,
    -1e308,
]


def deep(rng, opener, closer, most=2000):
    n = rng.randint(2, most)
    return opener * n + closer * n


def a_script(rng):
    """A Painless script, mostly nonsense."""
    which = rng.randrange(12)
    if which == 0:
        return "!" * rng.randint(2, 5000) + "true"
    if which == 1:
        return "1 " + "?: 1 " * rng.randint(2, 5000)
    if which == 2:
        return "1" + "?1:1" * rng.randint(2, 3000)
    if which == 3:
        return deep(rng, "(", ")").replace("()", "(1)")
    if which == 4:
        return f"def l = []; l.add(l); return l{rng.choice(['', '.size()'])}"
    if which == 5:
        return f"def a = new int[{rng.choice(EDGES)}]; return a"
    if which == 6:
        return f'return "{rng.choice(AWKWARD)}" @'
    if which == 7:
        return f'return "{rng.choice(AWKWARD) * rng.randint(1, 40)}".repeat({rng.choice(EDGES)})'
    if which == 8:
        return f"String s = 'a'; for (int i = 0; i < {rng.randint(1, 60)}; i++) {{ s = s + s; }} return s.length()"
    if which == 9:
        return f"def m = [:]; m.put('k', m); return m.toString()"
    if which == 10:
        return f"return {rng.choice(EDGES)} {rng.choice(['<<', '>>', '>>>', '/', '%'])} {rng.choice(EDGES)}"
    return "".join(rng.choice("abc(){}[]+-*/;=<>!?:'\"\\ .0123456789" + "".join(AWKWARD)) for _ in range(rng.randint(1, 200)))


def a_query(rng):
    """A SQL or PPL query."""
    which = rng.randrange(6)
    if which == 0:
        return "SELECT 1 FROM fuzz WHERE " + "NOT " * rng.randint(2, 20000) + "a = 1"
    if which == 1:
        return "SELECT " + deep(rng, "(", ")").replace("()", "(1)") + " FROM fuzz"
    if which == 2:
        return f"SELECT {rng.choice(AWKWARD)} FROM fuzz WHERE a = '{rng.choice(AWKWARD)}'"
    if which == 3:
        return f"SELECT * FROM fuzz LIMIT {rng.choice(EDGES)}"
    if which == 4:
        return "SELECT " + ", ".join("a" for _ in range(rng.randint(1, 300))) + " FROM fuzz"
    return "".join(rng.choice("SELECT * FROM WHERE ()'\",.0123456789abc" + "".join(AWKWARD)) for _ in range(rng.randint(1, 160)))


def a_template(rng):
    n = rng.randint(1, 40)
    which = rng.randrange(4)
    if which == 0:
        return "{{#a}}" * n + "x" + "{{/a}}" * n
    if which == 1:
        return "{{" * rng.randint(1, 100) + "a" + "}}" * rng.randint(1, 100)
    if which == 2:
        return f"{{{{#join}}}}{rng.choice(AWKWARD)}{{{{/join}}}}"
    return "".join(rng.choice("{}#/^&!.a" + "".join(AWKWARD)) for _ in range(rng.randint(1, 120)))


def a_body(rng):
    """One probe: a path, a body, and what it is for."""
    which = rng.randrange(14)
    if which == 0:
        return ("POST", "/_scripts/painless/_execute", {"script": {"source": a_script(rng)}})
    if which == 1:
        return (
            "POST",
            "/fuzz/_search",
            {"script_fields": {"x": {"script": {"source": a_script(rng)}}}},
        )
    if which == 2:
        return ("POST", "/_plugins/_sql", {"query": a_query(rng)})
    if which == 3:
        return ("POST", "/_plugins/_ppl", {"query": f"source=fuzz | stats {a_query(rng)[:60]}"})
    if which == 4:
        return ("POST", "/_render/template", {"source": a_template(rng), "params": {"a": [1, 2]}})
    if which == 5:
        return (
            "POST",
            "/_analyze",
            {
                "analyzer": rng.choice(["standard", "phone", "keyword", "simple", "whitespace"]),
                "text": rng.choice(AWKWARD) * rng.randint(1, 300) + "9" * rng.randint(0, 2000),
            },
        )
    if which == 6:
        return (
            "POST",
            "/_analyze",
            {
                "tokenizer": "standard",
                "char_filter": ["html_strip"],
                "filter": [rng.choice(["cjk_bigram", "kstem", "lowercase", "shingle"])],
                "text": "<b>" + rng.choice(AWKWARD) * rng.randint(1, 60) + "</p>",
            },
        )
    if which == 7:
        return (
            "POST",
            "/fuzz/_search",
            {
                "size": 0,
                "aggs": {
                    "a": {
                        rng.choice(["date_histogram", "histogram", "terms", "range"]): {
                            "field": rng.choice(["d", "n", "s"]),
                            "interval": rng.choice([0, 1, -1, 1e300]),
                            "calendar_interval": rng.choice(["day", "month", "x"]),
                            "time_zone": rng.choice(["+a" + rng.choice(AWKWARD), "+0730", "Zz"]),
                            "size": rng.choice(EDGES),
                            "ranges": rng.choice([[], [{"from": 1}], [{}]]),
                        }
                    }
                },
            },
        )
    if which == 8:
        return (
            "GET",
            "/fuzz/_search?filter_path=" + ".".join(["**"] * rng.randint(1, 30)) + ",-zzz",
            None,
        )
    if which == 9:
        return (
            "PUT",
            "/_ingest/pipeline/fuzz",
            {
                "processors": [
                    {
                        "grok": {
                            "field": "m",
                            "patterns": ["%{D" + str(rng.randint(0, 40)) + "}"],
                            "pattern_definitions": {
                                f"D{i}": ("a" * 8 if i == 0 else f"%{{D{i - 1}}}%{{D{i - 1}}}")
                                for i in range(0, 41)
                            },
                        }
                    }
                ]
            },
        )
    if which == 10:
        return (
            "POST",
            "/fuzz/_search",
            {"query": {"range": {"d": {"gte": "now/" + rng.choice(AWKWARD), "lt": "now+1x"}}}},
        )
    if which == 11:
        deep_query = {"match_all": {}}
        for _ in range(rng.randint(1, 200)):
            deep_query = {"bool": {"must": [deep_query]}}
        return ("POST", "/fuzz/_search", {"query": deep_query})
    if which == 12:
        return (
            "POST",
            "/fuzz/_search",
            {
                "query": {"match_all": {}},
                "from": rng.choice(EDGES),
                "size": rng.choice(EDGES),
                "sort": [{rng.choice(["n", "s", "_id"]): rng.choice(["asc", "x"])}],
            },
        )
    return (
        "POST",
        "/fuzz/_search",
        {
            "suggest": {
                "s": {
                    "text": rng.choice(AWKWARD) * rng.randint(1, 50),
                    rng.choice(["term", "phrase", "completion"]): {"field": "s"},
                }
            }
        },
    )


def call(url, method, path, body, timeout):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        url + path, data=data, method=method, headers={"content-type": "application/json"}
    )
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as answer:
            return answer.status, time.time() - started, ""
    except urllib.error.HTTPError as e:
        e.read()
        return e.code, time.time() - started, ""
    except Exception as e:
        return 0, time.time() - started, f"{type(e).__name__}: {str(e)[:120]}"


def start_node(binary, port, transport):
    data = tempfile.mkdtemp(prefix="boost-fuzz-")
    env = dict(os.environ)
    env.update(
        {
            "BOOSTSEARCH_ADDR": f"127.0.0.1:{port}",
            "BOOSTSEARCH_DATA": data,
            "BOOSTSEARCH_TRANSPORT_PORT": str(transport),
        }
    )
    log = open(pathlib.Path(data) / "node.log", "w")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    url = f"http://127.0.0.1:{port}"
    for _ in range(60):
        status, _, _ = call(url, "GET", "/", None, 5)
        if status:
            time.sleep(1)
            return node, data, url
        if node.poll() is not None:
            break
        time.sleep(1)
    node.kill()
    print(f"the node did not start; its log is in {data}/node.log")
    sys.exit(2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "boostsearch"))
    ap.add_argument("--rounds", type=int, default=3000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=20.0, help="a probe that takes longer fails")
    ap.add_argument("--port", type=int, default=9266)
    ap.add_argument("--transport", type=int, default=9366)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    node, data, url = start_node(args.binary, args.port, args.transport)
    try:
        call(
            url,
            "PUT",
            "/fuzz",
            {"mappings": {"properties": {"n": {"type": "integer"}, "s": {"type": "keyword"}, "d": {"type": "date"}}}},
            10,
        )
        call(url, "PUT", "/fuzz/_doc/1?refresh=true", {"n": 1, "s": "a", "d": "2026-01-01"}, 10)

        dead = []
        slow = []
        for round_number in range(args.rounds):
            method, path, body = a_body(rng)
            status, took, why = call(url, method, path, body, args.timeout)
            if status == 0:
                dead.append((why, took, method, path, json.dumps(body)[:300]))
                # the node may be gone: if it is, stop and say so
                alive, _, _ = call(url, "GET", "/_cat/health", None, 10)
                if not alive:
                    # a node that was killed from outside is not a finding:
                    # another session's `pkill` looks exactly like a crash
                    # from here, and the log is what tells them apart
                    node.poll()
                    log = (pathlib.Path(data) / "node.log").read_text(errors="replace")
                    crashed = any(
                        word in log
                        for word in ("panicked", "stack overflow", "memory allocation", "abort")
                    )
                    if not crashed and node.returncode is not None and node.returncode < 0:
                        print(
                            f"  the node was killed from outside (signal {-node.returncode}) "
                            f"after {round_number + 1} probes; nothing was found"
                        )
                        return 2
                    print(f"  the node stopped answering after {round_number + 1} probes")
                    for row in dead[-3:]:
                        print(f"    {row[2]} {row[3]}  {row[4]}")
                    print(f"\n  its log: {data}/node.log")
                    print("\nRESULT the node died")
                    return 1
            elif took >= args.timeout * 0.95:
                slow.append((took, method, path, json.dumps(body)[:300]))

        print(f"  {args.rounds} probes, seed {args.seed}")
        if dead:
            print(f"\n  {len(dead)} probes got no answer (a panic drops the connection):")
            for why, took, method, path, body in dead[:10]:
                print(f"    {method} {path}  {body}")
                print(f"      {why}")
        if slow:
            print(f"\n  {len(slow)} probes took longer than {args.timeout}s:")
            for took, method, path, body in slow[:10]:
                print(f"    {took:.1f}s  {method} {path}  {body}")
        if dead or slow:
            print("\nRESULT a request was answered with a dropped connection or not at all")
            return 1
        print("\nRESULT every probe was answered, and the node is still up")
        return 0
    finally:
        node.send_signal(signal.SIGTERM)
        try:
            node.wait(timeout=10)
        except subprocess.TimeoutExpired:
            node.kill()
        shutil.rmtree(data, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
