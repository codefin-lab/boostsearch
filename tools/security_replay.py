#!/usr/bin/env python3
"""The security surface, answered by both engines and compared.

The gates compare this server with OpenSearch on everything a caller without
a password can ask. They say nothing about the surface that only exists when
security is on: who may reach what, what a refusal says, what the security
REST API answers to a badly shaped role, and what a caller sees of a document
a filter hides from them.

That comparison had been done before -- twice, in scripts written into /tmp
and lost with it, which is why it kept being called outstanding. This is the
same comparison, kept.

Both sides are set up through their own REST API, so nothing is seeded from
files and nothing depends on fixtures made by hand:

  * a role `r1` over `logs-*`: read, with a document filter of
    `{"term": {"public": true}}`, the field `secret` excluded, and `ssn`
    masked
  * a user `u1` mapped to it, and the same three documents in `logs-2026`
  * then every question below is asked of both engines as `admin` and as
    `u1`, and the answers are compared after the parts that cannot match --
    timings, node and cluster names, index uuids -- are taken out

The reference is a security-enabled OpenSearch started for this purpose; its
password is one this check sets itself (`--ref-password`), not anyone's
secret. This server is started the same way `docs` describes, with security
on and the demo administrator.

    docker run -d --name os-secure -p 9253:9200 \\
        -e discovery.type=single-node \\
        -e OPENSEARCH_INITIAL_ADMIN_PASSWORD=... \\
        opensearchproject/opensearch:3.1.0
    python3 tools/security_replay.py --ref https://127.0.0.1:9253 --ref-password ...
"""

import argparse
import base64
import json
import os
import pathlib
import re
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
PORT = int(os.environ.get("BOOST_SEC_PORT", "9368"))
TRANSPORT = int(os.environ.get("BOOST_SEC_TRANSPORT", "9468"))
LAX = ssl._create_unverified_context()


def call(base, path, method="GET", body=None, creds=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"content-type": "application/json"})
    if creds:
        token = base64.b64encode(f"{creds[0]}:{creds[1]}".encode()).decode()
        req.add_header("authorization", f"Basic {token}")
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=LAX) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw.decode(errors="replace")[:400]}
    except Exception as e:
        return 0, {"error": f"{type(e).__name__}: {e}"[:200]}


VOLATILE = re.compile(r'"(took|_scroll_id|cluster_uuid|index_uuid|cluster_name|name|node|'
                      r'_primary_term|_seq_no|_version|start_time_in_millis|time_in_millis|'
                      r'timestamp|duration_in_millis|_id)"\s*:\s*("[^"]*"|[0-9.]+|null)')
ADDRESS = re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}:\d+|localhost:\d+")


def scrub(v):
    """What cannot match between two engines, taken out before comparing."""
    text = json.dumps(v, sort_keys=True)
    text = VOLATILE.sub(lambda m: f'"{m.group(1)}":"~"', text)
    text = ADDRESS.sub("~addr~", text)
    # a refusal names the user in the reference's own words; the shape is what
    # is compared, so the identity inside it is levelled
    text = re.sub(r"User \[name=[^\]]*\]", "User [~]", text)
    return text


class Node:
    """A security-enabled BoostSearch, started as the docs describe."""

    def __init__(self, binary, data, log):
        env = {k: v for k, v in os.environ.items() if not k.startswith("BOOSTSEARCH_")}
        env.update({
            "BOOSTSEARCH_ADDR": f"127.0.0.1:{PORT}",
            "BOOSTSEARCH_DATA": str(data),
            "BOOSTSEARCH_TRANSPORT_PORT": str(TRANSPORT),
            "BOOSTSEARCH_TRANSPORT_INSECURE": "true",
            "BOOSTSEARCH_DISABLED": "false",
            "BOOSTSEARCH_RESTAPI_ROLES_ENABLED": "all_access",
        })
        self.log = open(log, "ab")
        self.proc = subprocess.Popen([binary], env=env, stdout=self.log, stderr=subprocess.STDOUT)

    def answering(self, base, creds, seconds=45):
        end = time.time() + seconds
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            if call(base, "/", creds=creds)[0]:
                return True
            time.sleep(0.4)
        return False

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


DOCS = [
    ("1", {"public": True, "who": "alice", "secret": "s1", "ssn": "000-00-0001", "n": 1}),
    ("2", {"public": False, "who": "bob", "secret": "s2", "ssn": "000-00-0002", "n": 2}),
    ("3", {"public": True, "who": "carol", "secret": "s3", "ssn": "000-00-0003", "n": 3}),
]

ROLE = {
    "index_permissions": [{
        "index_patterns": ["logs-*"],
        "dls": json.dumps({"term": {"public": True}}),
        "fls": ["~secret"],
        "masked_fields": ["ssn"],
        "allowed_actions": ["read", "search"],
    }]
}


# the entities the questions below write to: deleted on both sides first, so
# that what the second run compares is not what the first run left behind
PROBES = [("roles", "bad1"), ("roles", "bad2"), ("internalusers", "bad3"),
          ("internalusers", "bad4"), ("internalusers", "tenantx"),
          ("internalusers", "short1"), ("rolesmapping", "r2"),
          ("actiongroups", "ag1")]


def setup(base, admin, user_password):
    """The same fixture on either engine, through its own API."""
    out = {}
    for kind, name in PROBES:
        call(base, f"/_plugins/_security/api/{kind}/{name}", "DELETE", creds=admin)
    # and the indices, so that a second run is not comparing what the first
    # one left standing
    for index in ("logs-2026", "other-2026"):
        call(base, f"/{index}", "DELETE", creds=admin)
    out["index"] = call(base, "/logs-2026", "PUT", {
        "settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}},
        "mappings": {"properties": {"public": {"type": "boolean"}, "who": {"type": "keyword"},
                                    "secret": {"type": "keyword"}, "ssn": {"type": "keyword"},
                                    "n": {"type": "long"}}}}, creds=admin)
    for doc_id, src in DOCS:
        out[f"doc{doc_id}"] = call(base, f"/logs-2026/_doc/{doc_id}?refresh=true", "PUT", src, creds=admin)
    out["other"] = call(base, "/other-2026", "PUT",
                        {"settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}}},
                        creds=admin)
    out["other-doc"] = call(base, "/other-2026/_doc/1?refresh=true", "PUT", {"who": "dave"}, creds=admin)
    out["role"] = call(base, "/_plugins/_security/api/roles/r1", "PUT", ROLE, creds=admin)
    out["user"] = call(base, "/_plugins/_security/api/internalusers/u1", "PUT",
                       {"password": user_password}, creds=admin)
    out["mapping"] = call(base, "/_plugins/_security/api/rolesmapping/r1", "PUT",
                          {"users": ["u1"]}, creds=admin)
    return out


def questions(user_password):
    """(name, as_admin, method, path, body) -- asked of both engines."""
    q = [
        # what a filtered caller sees of the documents
        ("search as the filtered user", False, "POST", "/logs-2026/_search",
         {"size": 10, "query": {"match_all": {}}, "sort": [{"n": "asc"}]}),
        ("count as the filtered user", False, "POST", "/logs-2026/_count", {"query": {"match_all": {}}}),
        ("get a document the filter allows", False, "GET", "/logs-2026/_doc/1", None),
        ("get a document the filter hides", False, "GET", "/logs-2026/_doc/2", None),
        ("mget across both", False, "POST", "/logs-2026/_mget",
         {"docs": [{"_id": "1"}, {"_id": "2"}, {"_id": "3"}]}),
        ("a terms aggregation", False, "POST", "/logs-2026/_search",
         {"size": 0, "aggs": {"w": {"terms": {"field": "who", "size": 10}}}}),
        ("an aggregation over the masked field", False, "POST", "/logs-2026/_search",
         {"size": 0, "aggs": {"s": {"terms": {"field": "ssn", "size": 10}}}}),
        ("a query on the excluded field", False, "POST", "/logs-2026/_search",
         {"size": 10, "query": {"term": {"secret": "s1"}}}),
        ("source filtering cannot ask for the excluded field", False, "POST", "/logs-2026/_search",
         {"size": 10, "_source": ["secret", "who"], "query": {"match_all": {}}}),
        ("docvalue_fields on the masked field", False, "POST", "/logs-2026/_search",
         {"size": 10, "docvalue_fields": ["ssn"], "query": {"match_all": {}}}),
        ("field_caps", False, "GET", "/logs-2026/_field_caps?fields=*", None),
        ("termvectors of an allowed document", False, "GET",
         "/logs-2026/_termvectors/1?fields=who", None),
        ("explain on a hidden document", False, "POST", "/logs-2026/_explain/2",
         {"query": {"match_all": {}}}),
        # what it may not reach at all
        ("the index it was not granted", False, "POST", "/other-2026/_search", {"query": {"match_all": {}}}),
        ("a write to the index it may read", False, "PUT", "/logs-2026/_doc/9", {"public": True}),
        ("a delete", False, "DELETE", "/logs-2026/_doc/1", None),
        ("cluster health", False, "GET", "/_cluster/health", None),
        ("cluster state", False, "GET", "/_cluster/state", None),
        ("the security API", False, "GET", "/_plugins/_security/api/internalusers", None),
        ("its own permissions", False, "GET", "/_plugins/_security/authinfo", None),
        ("a wildcard search over everything", False, "POST", "/_search", {"query": {"match_all": {}}}),
        ("an index it names in the body", False, "POST", "/_msearch",
         {"raw": "not used"}),
        # the shapes the security API answers the administrator
        ("read the role back", True, "GET", "/_plugins/_security/api/roles/r1", None),
        ("read the user back", True, "GET", "/_plugins/_security/api/internalusers/u1", None),
        ("read the mapping back", True, "GET", "/_plugins/_security/api/rolesmapping/r1", None),
        ("a role that does not exist", True, "GET", "/_plugins/_security/api/roles/nope", None),
        ("a user that does not exist", True, "GET", "/_plugins/_security/api/internalusers/nope", None),
        ("a role with an unknown field", True, "PUT", "/_plugins/_security/api/roles/bad1",
         {"index_permissions": [{"index_patterns": ["x"], "allowed_actions": ["read"]}], "nonsense": 1}),
        ("a role with a malformed permission", True, "PUT", "/_plugins/_security/api/roles/bad2",
         {"index_permissions": "not an array"}),
        ("a user with no password at all", True, "PUT", "/_plugins/_security/api/internalusers/bad3", {}),
        ("a user whose password is its name", True, "PUT",
         "/_plugins/_security/api/internalusers/tenantx", {"password": "tenantx"}),
        ("a password of one character", True, "PUT",
         "/_plugins/_security/api/internalusers/short1", {"password": "a"}),
        ("both password and hash", True, "PUT", "/_plugins/_security/api/internalusers/bad4",
         {"password": user_password, "hash": "$2y$12$abcdefghijklmnopqrstuv"}),
        ("a mapping to nobody", True, "PUT", "/_plugins/_security/api/rolesmapping/r2", {}),
        ("an action group", True, "PUT", "/_plugins/_security/api/actiongroups/ag1",
         {"allowed_actions": ["indices:data/read/search"]}),
        ("the action group back", True, "GET", "/_plugins/_security/api/actiongroups/ag1", None),
        ("delete the action group", True, "DELETE", "/_plugins/_security/api/actiongroups/ag1", None),
        ("delete it twice", True, "DELETE", "/_plugins/_security/api/actiongroups/ag1", None),
        ("the tenants", True, "GET", "/_plugins/_security/api/tenants", None),
        ("account info", True, "GET", "/_plugins/_security/api/account", None),
        ("the plugin's health", True, "GET", "/_plugins/_security/health", None),
    ]
    return [x for x in q if x[0] != "an index it names in the body"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target/release/boostsearch"))
    ap.add_argument("--ref", default="https://127.0.0.1:9253")
    ap.add_argument("--ref-password", default=os.environ.get("BOOST_REF_PASSWORD", ""))
    ap.add_argument("--user-password", default="Zq7-mesa-lantern-42")
    ap.add_argument("--show", type=int, default=8, help="how many differences to print in full")
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    if not a.ref_password:
        print("give the reference's admin password with --ref-password "
              "(the one the container was started with)")
        return 2

    ours = f"http://127.0.0.1:{PORT}"
    ref_admin = ("admin", a.ref_password)
    our_admin = ("admin", "admin")

    work = pathlib.Path(tempfile.mkdtemp(prefix="bssecrep."))
    (work / "data" / "config" / "security").mkdir(parents=True)
    node = Node(a.binary, work / "data", work / "node.log")
    try:
        if not node.answering(ours, our_admin):
            print("the security-enabled node never answered; its log:")
            print((work / "node.log").read_text(errors="replace")[-800:])
            return 1
        st, _ = call(a.ref, "/", creds=ref_admin)
        if st != 200:
            print(f"the reference at {a.ref} answered {st} to the administrator; is it up, "
                  "and is --ref-password the password it was started with?")
            return 1

        print("setting the same fixture up on both sides")
        ours_setup = setup(ours, our_admin, a.user_password)
        ref_setup = setup(a.ref, ref_admin, a.user_password)
        for name in ours_setup:
            o, r = ours_setup[name][0], ref_setup[name][0]
            if not (200 <= o < 300) or not (200 <= r < 300):
                print(f"  setup differs at {name}: ours {o} {str(ours_setup[name][1])[:120]} | "
                      f"reference {r} {str(ref_setup[name][1])[:120]}")
        time.sleep(2)

        same, differ, rows = 0, 0, []
        for name, as_admin, method, path, body in questions(a.user_password):
            our_creds = our_admin if as_admin else ("u1", a.user_password)
            ref_creds = ref_admin if as_admin else ("u1", a.user_password)
            o_st, o_body = call(ours, path, method, body, creds=our_creds)
            r_st, r_body = call(a.ref, path, method, body, creds=ref_creds)
            o, r = scrub(o_body), scrub(r_body)
            if o_st == r_st and o == r:
                same += 1
            else:
                differ += 1
                rows.append((name, method, path, o_st, r_st, o, r))

        print(f"\n  {same} of {same + differ} answers identical, {differ} differ")
        for name, method, path, o_st, r_st, o, r in rows[:a.show]:
            print(f"\n  {name}  [{method} {path}]  status ours {o_st} / reference {r_st}")
            print(f"    reference: {r[:400]}")
            print(f"    ours:      {o[:400]}")
        if len(rows) > a.show:
            print(f"\n  ... and {len(rows) - a.show} more")
        out = ROOT / "security-replay.json"
        out.write_text(json.dumps([{
            "name": n, "method": m, "path": p, "status": [o_st, r_st],
            "ours": o, "reference": r} for n, m, p, o_st, r_st, o, r in rows], indent=1))
        print(f"\nwritten to {out}")
        print("RESULT", "the security surface answers as the reference does"
              if differ == 0 else f"{differ} of {same + differ} differ")
        return 0 if differ == 0 else 1
    finally:
        node.stop()
        if not a.keep:
            import shutil
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
