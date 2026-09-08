#!/usr/bin/env python3
"""Every route, every caller: what each identity may reach.

The gates measure whether this server answers the way OpenSearch answers. They
say nothing about what happens when a caller may *not* do what they asked, and
that is where two reviews found most of what was wrong: a path the action table
did not know ran unjudged, a path that merely ended with the token exchange ran
with no credentials at all, and a request that named its indices in the body was
judged on a cluster permission alone.

So this walks the router. Every route in `src/main.rs` is probed as five
identities -- nobody at all, a caller with no roles, a reader and a writer on
`public*`, and a caller with the composite-operations cluster permission -- and
the answer is compared with a baseline. A route that starts answering a caller
who should not reach it makes the file differ, and the run goes red.

    tools/auth_matrix.py                 # run against a node it starts itself
    tools/auth_matrix.py --write-baseline

The baseline is `tools/auth_matrix_baseline.json`: one line per route and
identity, holding "allowed" or the refusal. Reading the diff is the review.
"""

import argparse
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "tools" / "auth_matrix_baseline.json"
MAIN = ROOT / "src" / "main.rs"

# the identities, as (name, credentials or None, what they are for)
PEOPLE = [
    ("anonymous", None),
    # the passwords are checked against the user name, so they share none of it
    ("nobody", ("nobody", "Zq7-quiet-marsh-41")),
    ("reader", ("reader", "Zq7-quiet-marsh-42")),
    ("writer", ("writer", "Zq7-quiet-marsh-43")),
    ("ops", ("ops", "Zq7-quiet-marsh-44")),
]

# what the fixtures hold: one index the restricted callers may reach, one they
# may not, and a document in each
OPEN_INDEX = "public"
SHUT_INDEX = "secret"

# a body for the routes that need one; anything else is sent without a body.
# The composite endpoints carry a real body naming the forbidden index, since
# they judge each item rather than the request: with an empty body they answer
# 400 before judging anything, and the probe would say nothing.
NDJSON = {
    "_bulk": f'{{"index":{{"_index":"{SHUT_INDEX}","_id":"probe"}}}}\n{{"a":1}}\n',
    "_msearch": f'{{"index":"{SHUT_INDEX}"}}\n{{"query":{{"match_all":{{}}}}}}\n',
    "_msearch/template": f'{{"index":"{SHUT_INDEX}"}}\n'
    '{"source":"{\\"query\\":{\\"match_all\\":{}}}","params":{}}\n',
}
BODIES = {
    "_search": {"query": {"match_all": {}}},
    "_count": {"query": {"match_all": {}}},
    "_mget": {"docs": [{"_index": SHUT_INDEX, "_id": "1"}]},
    "_mtermvectors": {"docs": [{"_index": SHUT_INDEX, "_id": "1"}]},
    "_reindex": {"source": {"index": SHUT_INDEX}, "dest": {"index": OPEN_INDEX}},
    "_update_by_query": {"query": {"match_all": {}}},
    "_delete_by_query": {"query": {"match_all": {}}},
    "_render/template": {"source": "{{x}}", "params": {"x": 1}},
    "_analyze": {"text": "a"},
    "_aliases": {"actions": []},
    "_sql": {"query": f"SELECT * FROM {SHUT_INDEX}"},
    "_ppl": {"query": f"source={SHUT_INDEX}"},
}


def routes():
    """Every route the server declares, as (method, path).

    The file is read whole rather than a line at a time: a route written
    across several lines -- which is what a long path or a route with four
    methods on it looks like after rustfmt -- matched nothing, so 32 of the
    server's routes were never probed at all, and the ones written that way
    are the complicated ones.
    """
    text = MAIN.read_text()
    found = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', text):
        path = m.group(1)
        # the handlers of this route: from the comma to the `)` that closes
        # the `.route(` call
        rest, depth, i = [], 1, m.end()
        while i < len(text) and depth:
            c = text[i]
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if not depth:
                    break
            rest.append(c)
            i += 1
        rest = "".join(rest)
        methods = set(re.findall(r"\b(get|post|put|delete|head|patch|any)\s*\(", rest))
        if not methods:
            methods = {"get"}
        if "any" in methods:
            methods = {"get", "post"}
        for method in sorted(methods):
            found.append((method.upper(), path))
    return found


def filled(path):
    """A route with its parameters filled in, or None where it cannot be."""
    out = path
    out = out.replace("{index}", OPEN_INDEX)
    out = out.replace("{target}", "copy")
    out = out.replace("{id}", "1")
    out = out.replace("{name}", "one")
    out = out.replace("{repo}", "repo1")
    out = out.replace("{snapshot}", "snap1")
    out = out.replace("{policy}", "policy1")
    out = out.replace("{alias}", "alias1")
    out = out.replace("{field}", "a")
    out = out.replace("{lang}", "painless")
    out = out.replace("{context}", "score")
    out = out.replace("{node_id}", "_local")
    out = out.replace("{metric}", "_all")
    out = out.replace("{type}", "dashboard")
    out = out.replace("{*rest}", "x")
    out = re.sub(r"\{\*?[a-z_]+\}", "x", out)
    return out


def body_for(path):
    """The body a probe carries, and whether it is ndjson."""
    for key, raw in NDJSON.items():
        if path.endswith(key):
            return raw, True
    for key, body in BODIES.items():
        if key in path:
            return body, False
    return None, False


def call(url, method, path, who, body, ndjson=False):
    if body is None:
        data = None
    elif ndjson:
        data = body.encode()
    else:
        data = json.dumps(body).encode()
    kind = "application/x-ndjson" if ndjson else "application/json"
    req = urllib.request.Request(
        url + path, data=data, method=method, headers={"content-type": kind}
    )
    if who is not None:
        import base64

        token = base64.b64encode(f"{who[0]}:{who[1]}".encode()).decode()
        req.add_header("authorization", f"Basic {token}")
    try:
        with urllib.request.urlopen(req, timeout=20) as answer:
            return answer.status, answer.read()[:400]
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:400]
    except Exception as e:  # a connection reset is a panic, and is a failure
        return 0, str(e)[:200].encode()


def verdict(status, body):
    """What an answer says about permission, in one word."""
    if status in (401, 403):
        return "refused"
    if status == 0:
        return "no answer"
    text = body.decode("utf-8", "replace")
    if "security_exception" in text or "no permissions for" in text:
        return "refused"
    return "allowed"


def fixtures(url):
    """The indices, users and roles the probes are run against."""
    admin = ("admin", "admin")

    def put(path, body, who=admin):
        return call(url, "PUT", path, who, body)

    put(f"/{OPEN_INDEX}", {})
    put(f"/{SHUT_INDEX}", {})
    put(f"/{OPEN_INDEX}/_doc/1?refresh=true", {"a": 1, "hidden": "x"})
    put(f"/{SHUT_INDEX}/_doc/1?refresh=true", {"a": 1, "ssn": "000-11-2222"})
    roles = {
        "reader_role": {
            "index_permissions": [
                {"index_patterns": [f"{OPEN_INDEX}*"], "allowed_actions": ["read"]}
            ]
        },
        "writer_role": {
            "index_permissions": [
                {"index_patterns": [f"{OPEN_INDEX}*"], "allowed_actions": ["crud"]}
            ]
        },
        "ops_role": {"cluster_permissions": ["cluster_composite_ops"]},
        "nobody_role": {},
    }
    for name, body in roles.items():
        put(f"/_plugins/_security/api/roles/{name}", body)
    for who, role in [
        ("reader", "reader_role"),
        ("writer", "writer_role"),
        ("ops", "ops_role"),
        ("nobody", "nobody_role"),
    ]:
        password = dict(PEOPLE)[who][1]
        put(f"/_plugins/_security/api/internalusers/{who}", {"password": password})
        put(
            f"/_plugins/_security/api/rolesmapping/{role}",
            {"users": [who]},
        )


def start_node(binary, port, transport):
    """A node with security on and the default users, in a directory of its own."""
    data = tempfile.mkdtemp(prefix="boost-auth-")
    config = pathlib.Path(data) / "config"
    (config / "security").mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env.update(
        {
            "BOOSTSEARCH_ADDR": f"127.0.0.1:{port}",
            "BOOSTSEARCH_DATA": data,
            "BOOSTSEARCH_CONFIG": str(config),
            "BOOSTSEARCH_TRANSPORT_PORT": str(transport),
            "BOOSTSEARCH_DISABLED": "false",
            "BOOSTSEARCH_RESTAPI_ROLES_ENABLED": "all_access",
        }
    )
    log = open(pathlib.Path(data) / "node.log", "w")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    url = f"http://127.0.0.1:{port}"
    for _ in range(60):
        status, _ = call(url, "GET", "/", ("admin", "admin"), None)
        if status:
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
    ap.add_argument("--port", type=int, default=9269)
    ap.add_argument("--transport", type=int, default=9369)
    ap.add_argument("--write-baseline", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the node's directory behind")
    args = ap.parse_args()

    node, data, url = start_node(args.binary, args.port, args.transport)
    try:
        fixtures(url)
        found = {}
        for method, path in routes():
            probe = filled(path)
            if probe is None or probe.startswith("/_plugins/_security"):
                # the security API judges itself, and probing it would rewrite
                # the fixtures underneath the run
                continue
            body, ndjson = body_for(probe)
            for who, credentials in PEOPLE:
                status, answer = call(url, method, probe, credentials, body, ndjson)
                found[f"{method} {probe} :: {who}"] = verdict(status, answer)
        # the two shapes that a route table cannot show: a path that merely
        # ends with the token exchange, and one that names a forbidden index
        # in its body
        extra = {
            "PUT /_alias/_plugins/_security/api/authtoken :: anonymous": call(
                url,
                "PUT",
                "/_alias/_plugins/_security/api/authtoken",
                None,
                {"index": SHUT_INDEX, "alias": "sneaked"},
            ),
            "GET /_nodes/_plugins/_security/api/authtoken :: anonymous": call(
                url, "GET", "/_nodes/_plugins/_security/api/authtoken", None, None
            ),
            f"GET /{SHUT_INDEX}/_upgrade :: reader": call(
                url, "GET", f"/{SHUT_INDEX}/_upgrade", dict(PEOPLE)["reader"], None
            ),
            "POST /_reindex secret->public :: ops": call(
                url,
                "POST",
                "/_reindex",
                dict(PEOPLE)["ops"],
                {"source": {"index": SHUT_INDEX}, "dest": {"index": OPEN_INDEX}},
            ),
            # the other direction: the checks must not have closed what a
            # caller may legitimately reach
            "POST /_msearch/template over the open index :: reader": call(
                url,
                "POST",
                "/_msearch/template",
                dict(PEOPLE)["reader"],
                f'{{"index":"{OPEN_INDEX}"}}\n'
                '{"source":"{\\"query\\":{\\"match_all\\":{}}}","params":{}}\n',
                True,
            ),
            "POST /_sql over the open index :: reader": call(
                url,
                "POST",
                "/_plugins/_sql",
                dict(PEOPLE)["reader"],
                {"query": f"SELECT * FROM {OPEN_INDEX}"},
            ),
            "GET /public/_search :: reader": call(
                url, "GET", f"/{OPEN_INDEX}/_search", dict(PEOPLE)["reader"], None
            ),
            "POST /_sql over a forbidden index :: reader": call(
                url,
                "POST",
                "/_plugins/_sql",
                dict(PEOPLE)["reader"],
                {"query": f"SELECT * FROM {SHUT_INDEX}"},
            ),
        }
        for name, (status, answer) in extra.items():
            found[name] = verdict(status, answer)

        if args.write_baseline:
            BASELINE.write_text(
                json.dumps(
                    {
                        "what": "what each identity may reach, one line per route and caller. "
                        "A line that changes from refused to allowed is a permission "
                        "check that stopped happening.",
                        "answers": found,
                    },
                    indent=1,
                    sort_keys=True,
                )
                + "\n"
            )
            allowed = sum(1 for v in found.values() if v == "allowed")
            print(f"  wrote {len(found)} answers ({allowed} allowed) to {BASELINE.name}")
            return 0

        if not BASELINE.exists():
            print(f"  {BASELINE.name} is not there: run with --write-baseline first")
            return 2
        want = json.loads(BASELINE.read_text())["answers"]
        opened, closed, missing, added = [], [], [], []
        for key, answer in sorted(found.items()):
            if key not in want:
                added.append(key)
            elif want[key] != answer:
                (opened if answer == "allowed" else closed).append(
                    f"{key}: {want[key]} -> {answer}"
                )
        for key in want:
            if key not in found:
                missing.append(key)
        print(f"  {len(found)} answers over {len(routes())} routes")
        for title, rows in [
            ("REACHABLE NOW AND NOT BEFORE (a check that stopped happening)", opened),
            ("refused now and allowed before", closed),
            ("routes the baseline has and this run did not probe", missing),
            ("routes this run probed and the baseline does not have", added),
        ]:
            if rows:
                print(f"\n  {title}:")
                for row in rows[:40]:
                    print(f"    {row}")
        if opened:
            print("\nRESULT a caller reached something the baseline says they may not")
            return 1
        if closed or missing or added:
            print("\nRESULT the matrix moved; look at the lines above and rewrite the baseline")
            return 1
        print("\nRESULT every route answers each caller as the baseline says")
        return 0
    finally:
        node.send_signal(signal.SIGTERM)
        try:
            node.wait(timeout=10)
        except subprocess.TimeoutExpired:
            node.kill()
        if not args.keep:
            shutil.rmtree(data, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
