#!/usr/bin/env python3
"""The security configuration as the cluster's, not each node's.

Three things are checked, against nodes this script starts and stops itself:

  bootstrap   a node with security on and nothing configured lets nobody in:
              no built-in password works, a weak initial admin password is
              refused, a strong one is the only way in, and a configuration
              file the node cannot read is not replaced by anything
  saving      a change the node could not save is answered as a failure, on
              every write path of the security API, and a restart shows the
              configuration the answers described
  rejoining   three nodes: a user deleted, a password changed and a role
              mapping removed while one node is stopped (again while it is
              paused long enough to be dropped from the cluster, and again
              when it is the cluster manager) are never honoured by that
              node once it is back; writers on different nodes at once lose
              nothing; a change the manager cannot save is a failure through
              any node, and a change with no cluster manager is a 503

    python3 tools/security_cluster_check.py --binary target/release/velosearch

The nodes listen on HTTP 9711-9714 and transport 9811-9814, with data under
a fresh /tmp/secrepl-check.* directory (kept with --keep).
"""

import argparse
import base64
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
HTTP = [9711, 9712, 9713]
TRANSPORT = [9811, 9812, 9813]
NAMES = ["s1", "s2", "s3"]
SINGLE_HTTP = 9714
SINGLE_TRANSPORT = 9814
PASSWORD = "Velo-Check-Key-2026"
DEMO = ("admin", "admin")
API = "/_plugins/_security/api"

results = []


def check(name, ok, detail=""):
    results.append((name, ok))
    print(f"  {'ok    ' if ok else 'FAILED'} {name}", flush=True)
    if not ok and detail:
        print(f"         {detail}", flush=True)
    return ok


def call(port, method, path, body=None, auth=None, timeout=20):
    """(status, parsed body) -- status 0 when nothing answered."""
    headers = {"content-type": "application/json"}
    if auth:
        token = base64.b64encode(f"{auth[0]}:{auth[1]}".encode()).decode()
        headers["authorization"] = f"Basic {token}"
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data, method=method,
                                 headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw, status = r.read(), r.status
    except urllib.error.HTTPError as e:
        raw, status = e.read(), e.code
    except Exception:
        return 0, None
    try:
        return status, json.loads(raw)
    except Exception:
        return status, raw.decode(errors="replace")


def until(seconds, what, step=0.25):
    """Ask `what` until it is truthy or the time is up; its last answer."""
    end = time.time() + seconds
    while True:
        got = what()
        if got or time.time() >= end:
            return got
        time.sleep(step)


class Node:
    def __init__(self, binary, root, name, http, transport, env):
        self.binary = binary
        self.name = name
        self.http = http
        self.data = pathlib.Path(root) / name
        self.data.mkdir(parents=True, exist_ok=True)
        self.security_dir = self.data / "config" / "security"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("VELOSEARCH_")}
        self.env.update({
            "VELOSEARCH_ADDR": f"127.0.0.1:{http}",
            "VELOSEARCH_DATA": str(self.data),
            "VELOSEARCH_TRANSPORT_PORT": str(transport),
            "VELOSEARCH_NODE_NAME": name,
            "VELOSEARCH_DISABLED": "false",
            "VELOSEARCH_RESTAPI_ROLES_ENABLED": "all_access",
        })
        self.env.update(env)
        # the coordinator's notes, when the run is being looked into
        if os.environ.get("VELOSEARCH_CLUSTER_DEBUG"):
            self.env["VELOSEARCH_CLUSTER_DEBUG"] = os.environ["VELOSEARCH_CLUSTER_DEBUG"]
        self.proc = None
        self.log = None

    def start(self, extra=None):
        env = dict(self.env)
        env.update(extra or {})
        self.log = open(self.data / "node.log", "ab")
        self.proc = subprocess.Popen([self.binary], env=env, stdout=self.log,
                                     stderr=subprocess.STDOUT)

    def answering(self, seconds=40):
        return until(seconds, lambda: self.proc.poll() is not None or call(self.http, "GET", "/")[0] != 0) \
            and self.proc.poll() is None

    def exited(self, seconds):
        return until(seconds, lambda: self.proc.poll() is not None)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        if self.log:
            self.log.close()
            self.log = None


def user_ok(port, user, password):
    return call(port, "GET", "/_plugins/_security/authinfo", auth=(user, password))


def put_user(port, admin, name, password, backend_roles=None):
    return call(port, "PUT", f"{API}/internalusers/{name}",
                {"password": password, "backend_roles": backend_roles or []}, admin)


def admin_for(port):
    """The administrator's credentials the node takes: the initial password,
    or -- where the node seeded a demo administrator -- the demo ones, so the
    rest of the checks can still run and show what they show."""
    if user_ok(port, "admin", PASSWORD)[0] == 200:
        return ("admin", PASSWORD)
    if user_ok(port, *DEMO)[0] == 200:
        return DEMO
    return ("admin", PASSWORD)


# ---- bootstrap ------------------------------------------------------------------------

def bootstrap(binary, root):
    print("\nbootstrap: security on, nothing configured", flush=True)
    base = {"VELOSEARCH_TRANSPORT_INSECURE": "true"}

    n = Node(binary, root, "empty", SINGLE_HTTP, SINGLE_TRANSPORT, base)
    n.start()
    try:
        check("empty: the node starts", n.answering())
        time.sleep(2)
        status, _ = call(n.http, "GET", "/", auth=DEMO)
        check("empty: admin:admin is not let in", status not in (200, 0), f"answered {status}")
        status, _ = call(n.http, "GET", "/")
        check("empty: nobody is let in", status in (401, 503), f"answered {status}")
        status, body = call(n.http, "GET", "/_plugins/_security/health")
        check("empty: the security health says it is not ready",
              status == 503 and isinstance(body, dict) and body.get("status") == "DOWN",
              f"answered {status} {body}")
    finally:
        n.stop()

    n = Node(binary, root, "weak", SINGLE_HTTP, SINGLE_TRANSPORT,
             {**base, "VELOSEARCH_INITIAL_ADMIN_PASSWORD": "admin"})
    n.start()
    try:
        refused = n.exited(20)
        status = 0 if refused else call(n.http, "GET", "/", auth=DEMO)[0]
        check("weak: an initial admin password of admin is refused",
              refused and n.proc.returncode != 0, f"still running, admin:admin answered {status}")
    finally:
        n.stop()

    n = Node(binary, root, "strong", SINGLE_HTTP, SINGLE_TRANSPORT,
             {**base, "VELOSEARCH_INITIAL_ADMIN_PASSWORD": PASSWORD})
    n.start()
    try:
        check("strong: the node starts", n.answering())
        status = until(10, lambda: user_ok(n.http, "admin", PASSWORD)[0] == 200)
        check("strong: the initial admin password lets admin in", status)
        status, _ = call(n.http, "GET", "/", auth=DEMO)
        check("strong: admin:admin is not let in", status == 401, f"answered {status}")
        for demo in ["kibanaserver", "readall", "logstash", "snapshotrestore", "kibanaro",
                     "anomalyadmin"]:
            status, _ = user_ok(n.http, demo, demo)
            check(f"strong: the demo user {demo} is not let in", status == 401,
                  f"answered {status}")
        n.stop()
        # the password seeds a configuration once; after that it is the files
        n.env.pop("VELOSEARCH_INITIAL_ADMIN_PASSWORD")
        n.start()
        check("strong: restarted without the password, the node starts", n.answering())
        ok = until(10, lambda: user_ok(n.http, "admin", PASSWORD)[0] == 200)
        check("strong: restarted, the saved administrator still lets admin in", ok)
        n.stop()
        # a file the node cannot read is not the same as no file
        target = n.security_dir / "internal_users.yml"
        if os.geteuid() != 0 and target.exists():
            target.chmod(0)
            n.env["VELOSEARCH_INITIAL_ADMIN_PASSWORD"] = PASSWORD
            n.start()
            try:
                started = n.answering()
                s1 = call(n.http, "GET", "/", auth=DEMO)[0] if started else 0
                s2 = call(n.http, "GET", "/", auth=("admin", PASSWORD))[0] if started else 0
                check("unreadable: nobody is let in with a users file the node cannot read",
                      s1 != 200 and s2 != 200, f"admin:admin {s1}, admin:<initial> {s2}")
                n.stop()
            finally:
                target.chmod(0o600)
            check("unreadable: the file is left as it was",
                  "admin" in target.read_text() and "$2" in target.read_text())
        # a file that is not YAML at all
        mapping = n.security_dir / "roles_mapping.yml"
        if check("strong: the node saved the configuration it seeded", mapping.exists()):
            kept = mapping.read_text()
            mapping.write_text(": : [ not yaml\n")
            n.start()
            try:
                started = n.answering()
                s1 = call(n.http, "GET", "/", auth=("admin", PASSWORD))[0] if started else 0
                check("unparsable: a mapping file that is not YAML lets nobody in",
                      s1 != 200, f"admin answered {s1}")
            finally:
                n.stop()
                mapping.write_text(kept)
    finally:
        n.stop()


# ---- saving -----------------------------------------------------------------------------

def denied(status):
    return status >= 500


def saving(binary, root):
    print("\nsaving: a change the node cannot save", flush=True)
    n = Node(binary, root, "saving", SINGLE_HTTP, SINGLE_TRANSPORT, {
        "VELOSEARCH_TRANSPORT_INSECURE": "true",
        "VELOSEARCH_INITIAL_ADMIN_PASSWORD": PASSWORD,
        "VELOSEARCH_UNSUPPORTED_RESTAPI_ALLOW_SECURITYCONFIG_MODIFICATION": "true",
    })
    n.start()
    try:
        if not check("saving: the node starts", n.answering()):
            return
        admin = until(10, lambda: admin_for(n.http) if user_ok(n.http, *admin_for(n.http))[0] == 200 else None)
        if not check("saving: an administrator is let in", bool(admin)):
            return
        # what exists before the directory is made read-only
        check("saving: a user to delete is made",
              put_user(n.http, admin, "keeper", "Held-Secret-12345")[0] == 201)
        check("saving: a role to map is made",
              call(n.http, "PUT", f"{API}/roles/keeper_role",
                   {"cluster_permissions": ["cluster_monitor"]}, admin)[0] == 201)
        writes = [
            ("PUT internalusers", "PUT", f"{API}/internalusers/notdurable",
             {"password": "Not-Durable-12345"}),
            ("PATCH internalusers/<name>", "PATCH", f"{API}/internalusers/keeper",
             [{"op": "add", "path": "/description", "value": "changed"}]),
            ("PATCH internalusers", "PATCH", f"{API}/internalusers",
             [{"op": "add", "path": "/patched", "value": {"password": "Other-Secret-12345"}}]),
            ("DELETE internalusers", "DELETE", f"{API}/internalusers/keeper", None),
            ("PUT roles", "PUT", f"{API}/roles/notdurable_role",
             {"cluster_permissions": ["cluster_monitor"]}),
            ("PUT rolesmapping", "PUT", f"{API}/rolesmapping/keeper_role", {"users": ["keeper"]}),
            ("PUT actiongroups", "PUT", f"{API}/actiongroups/notdurable_group",
             {"allowed_actions": ["indices:data/read/search"]}),
            ("PUT tenants", "PUT", f"{API}/tenants/notdurable_tenant", {"description": "x"}),
            ("PATCH securityconfig", "PATCH", f"{API}/securityconfig",
             [{"op": "add", "path": "/config/dynamic/do_not_fail_on_forbidden", "value": True}]),
        ]
        sec = n.security_dir
        sec.chmod(0o555)
        try:
            for label, method, path, body in writes:
                status, answer = call(n.http, method, path, body, admin)
                check(f"saving: {label} that cannot be saved is not answered as done",
                      denied(status), f"answered {status} {answer}")
            status, answer = call(n.http, "PUT", f"{API}/account",
                                  {"current_password": "Held-Secret-12345",
                                   "password": "Held-Secret-67890"}, ("keeper", "Held-Secret-12345"))
            check("saving: PUT account that cannot be saved is not answered as done",
                  denied(status), f"answered {status} {answer}")
            status, answer = call(n.http, "PUT", f"{API}/internalusers/notdurable",
                                  {"password": "Not-Durable-12345"}, admin)
            check("saving: the failure is a 500 in the plugin's words",
                  status == 500 and isinstance(answer, dict)
                  and answer.get("status") == "INTERNAL_SERVER_ERROR",
                  f"answered {status} {answer}")
            # what was refused is not in force either
            check("saving: the refused user does not log in",
                  user_ok(n.http, "notdurable", "Not-Durable-12345")[0] == 401)
            check("saving: the user whose deletion was refused still logs in",
                  user_ok(n.http, "keeper", "Held-Secret-12345")[0] == 200)
        finally:
            sec.chmod(0o755)
        # the answers and the files agree after a restart
        n.stop()
        n.env.pop("VELOSEARCH_INITIAL_ADMIN_PASSWORD")
        n.start()
        n.answering()
        until(10, lambda: user_ok(n.http, *admin)[0] == 200)
        check("saving: after a restart the refused user still does not exist",
              user_ok(n.http, "notdurable", "Not-Durable-12345")[0] == 401)
        check("saving: after a restart the user whose deletion was refused logs in",
              user_ok(n.http, "keeper", "Held-Secret-12345")[0] == 200)
        status, _ = call(n.http, "GET", f"{API}/roles/notdurable_role", auth=admin)
        check("saving: after a restart the refused role does not exist", status == 404,
              f"answered {status}")
        status, answer = put_user(n.http, admin, "durable", "Kept-Secret-12345")
        check("saving: with the directory writable again a change is saved", status == 201,
              f"answered {status} {answer}")
        leftovers = [p.name for p in sec.iterdir() if p.name.endswith(".tmp")]
        check("saving: no half-written file is left behind", not leftovers, f"{leftovers}")
    finally:
        n.stop()


# ---- the cluster ------------------------------------------------------------------------

def cluster_nodes(binary, root):
    seeds = ",".join(f"127.0.0.1:{t}" for t in TRANSPORT)
    nodes = []
    for i in range(3):
        nodes.append(Node(binary, root, NAMES[i], HTTP[i], TRANSPORT[i], {
            "VELOSEARCH_DISCOVERY_SEED_HOSTS": seeds,
            "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": ",".join(NAMES),
            "VELOSEARCH_INITIAL_ADMIN_PASSWORD": PASSWORD,
        }))
    return nodes


def node_count(port, admin):
    status, body = call(port, "GET", "/_cluster/health", auth=admin, timeout=5)
    return body.get("number_of_nodes", 0) if status == 200 and isinstance(body, dict) else 0


def manager_name(port, admin):
    status, body = call(port, "GET", "/_cat/nodes?h=name,master&format=json", auth=admin)
    if status != 200 or not isinstance(body, list):
        return None
    return next((r["name"] for r in body if r.get("master") == "*"), None)


def roles_of(port, user, password):
    status, body = user_ok(port, user, password)
    return status, (body.get("roles", []) if isinstance(body, dict) else [])


def watch_stale(port, stale, seconds, stop_early=None):
    """Ask one node, over and over, whether it still honours what was taken
    away; every stale acceptance it gave, as (what, status)."""
    seen = []
    end = time.time() + seconds
    while time.time() < end:
        for what, ask in stale:
            got = ask(port)
            if got:
                seen.append((what, got))
        if stop_early and stop_early():
            break
        time.sleep(0.2)
    return seen


def rejoining(binary, root):
    print("\nrejoining: three nodes", flush=True)
    nodes = cluster_nodes(binary, root)
    for n in nodes:
        n.start()
    try:
        for n in nodes:
            n.answering()
        admin = until(30, lambda: admin_for(HTTP[0]) if user_ok(HTTP[0], *admin_for(HTTP[0]))[0] == 200 else None)
        if not check("cluster: an administrator is let in", bool(admin)):
            return
        formed = until(60, lambda: node_count(HTTP[0], admin) == 3)
        if not check("cluster: three nodes form", formed):
            return
        check("cluster: every node lets the administrator in",
              until(20, lambda: all(user_ok(p, *admin)[0] == 200 for p in HTTP)))

        # a node stopped, a node paused long enough to be dropped, and the
        # cluster manager itself stopped while another is elected
        for mode in ["restart", "pause", "manager"]:
            revoked, rotated, mapped = f"revoked-{mode}", f"rotated-{mode}", f"mapped-{mode}"
            role, backend = f"role_{mode}", f"backend_{mode}"
            old_pw, new_pw = "Old-Password-12345", "New-Password-67890"
            put_user(HTTP[0], admin, revoked, old_pw)
            put_user(HTTP[0], admin, rotated, old_pw)
            put_user(HTTP[0], admin, mapped, old_pw, [backend])
            call(HTTP[0], "PUT", f"{API}/roles/{role}", {"cluster_permissions": ["cluster_monitor"]},
                 admin)
            call(HTTP[0], "PUT", f"{API}/rolesmapping/{role}", {"backend_roles": [backend]}, admin)
            everywhere = until(20, lambda: all(
                user_ok(p, revoked, old_pw)[0] == 200 and user_ok(p, rotated, old_pw)[0] == 200
                and role in roles_of(p, mapped, old_pw)[1] for p in HTTP))
            if not check(f"{mode}: the users and the mapping are in force on every node", everywhere):
                continue
            away = nodes[2]
            if mode == "manager":
                named = manager_name(HTTP[0], admin)
                away = next((n for n in nodes if n.name == named), away)
            alive = [n.http for n in nodes if n is not away]
            if mode == "pause":
                away.proc.send_signal(signal.SIGSTOP)
            else:
                away.stop()
            dropped = until(40, lambda: node_count(alive[0], admin) == 2
                            and manager_name(alive[0], admin) is not None)
            check(f"{mode}: the cluster carries on without the node", dropped)
            # the changes, made through the node that is not the manager, so
            # the change travels
            manager = manager_name(alive[0], admin)
            here = next((n.http for n in nodes if n is not away and n.name != manager), alive[0])
            s1, _ = call(here, "DELETE", f"{API}/internalusers/{revoked}", auth=admin)
            s2, _ = put_user(here, admin, rotated, new_pw)
            s3, _ = call(here, "DELETE", f"{API}/rolesmapping/{role}", auth=admin)
            check(f"{mode}: the delete, the new password and the unmapping are answered",
                  s1 == 200 and s2 == 200 and s3 == 200, f"answered {s1}, {s2}, {s3}")
            stale = [
                (f"{revoked} logs in", lambda p: user_ok(p, revoked, old_pw)[0] == 200 and 200),
                (f"{rotated} logs in with the old password",
                 lambda p: user_ok(p, rotated, old_pw)[0] == 200 and 200),
                (f"{mapped} still has {role}",
                 lambda p: (lambda r: r[0] == 200 and role in r[1] and 200)(roles_of(p, mapped, old_pw))),
            ]
            seen = []
            if mode == "pause":
                away.proc.send_signal(signal.SIGCONT)
            else:
                away.start()
            seen += watch_stale(away.http, stale, 25,
                                stop_early=lambda: node_count(alive[0], admin) == 3
                                and user_ok(away.http, rotated, new_pw)[0] == 200)
            rejoined = until(40, lambda: node_count(alive[0], admin) == 3)
            check(f"{mode}: the node rejoins", rejoined)
            # a little longer on the rejoined node, now that it is back
            seen += watch_stale(away.http, stale, 3)
            check(f"{mode}: the node that was away never honoured what was taken away",
                  not seen, f"{sorted(set(w for w, _ in seen))}")
            caught_up = until(20, lambda: all(user_ok(p, rotated, new_pw)[0] == 200 for p in HTTP))
            check(f"{mode}: the new password works on every node", caught_up)
            final = [(p, user_ok(p, revoked, old_pw)[0], user_ok(p, rotated, old_pw)[0],
                      roles_of(p, mapped, old_pw)) for p in HTTP]
            check(f"{mode}: every node refuses the deleted user and the old password",
                  all(r == 401 and o == 401 for _, r, o, _ in final), f"{final}")
            check(f"{mode}: no node grants the removed mapping",
                  all(role not in m[1] for _, _, _, m in final), f"{final}")

        # writers on every node at once
        answers = []

        def writer(port, tag):
            for i in range(6):
                answers.append(put_user(port, admin, f"cw-{tag}-{i}", "Concurrent-12345"))
                answers.append(call(port, "PATCH", f"{API}/internalusers",
                                    [{"op": "add", "path": f"/cp-{tag}-{i}",
                                      "value": {"password": "Concurrent-12345"}}], admin))

        threads = [threading.Thread(target=writer, args=(p, t)) for p, t in zip(HTTP, "abc")]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        check("concurrent: every write is answered as done",
              all(s in (200, 201) for s, _ in answers),
              f"{[a for a in answers if a[0] not in (200, 201)]}")
        wanted = {f"{k}-{t}-{i}" for k in ("cw", "cp") for t in "abc" for i in range(6)}

        def listed(port):
            status, body = call(port, "GET", f"{API}/internalusers", auth=admin)
            return set(body) if status == 200 and isinstance(body, dict) else set()

        agreed = until(20, lambda: all(wanted <= listed(p) for p in HTTP))
        missing = {p: sorted(wanted - listed(p)) for p in HTTP}
        check("concurrent: every node holds every user written", agreed, f"missing {missing}")
        check("concurrent: the nodes hold the same users",
              until(10, lambda: len({frozenset(listed(p)) for p in HTTP}) == 1))

        # a change the manager cannot save, sent through another node
        manager = manager_name(HTTP[0], admin)
        m = next((n for n in nodes if n.name == manager), None)
        if check("no-save: the cluster names a manager", m is not None):
            other = next(n for n in nodes if n is not m)
            m.security_dir.chmod(0o555)
            try:
                status, answer = put_user(other.http, admin, "unsaved", "Lost-Secret-12345")
                check("no-save: a user the manager cannot save is not answered as created",
                      denied(status), f"answered {status} {answer}")
            finally:
                m.security_dir.chmod(0o755)
            time.sleep(2)
            check("no-save: no node lets that user in",
                  all(user_ok(p, "unsaved", "Lost-Secret-12345")[0] != 200 for p in HTTP))

        # no cluster manager at all
        nodes[1].stop()
        nodes[2].stop()
        until(20, lambda: call(HTTP[0], "GET", "/_plugins/_security/health")[0] == 503)
        status, answer = put_user(HTTP[0], admin, "nomanager", "No-Manager-12345")
        check("no-manager: a change with no cluster manager is refused with 503",
              status == 503, f"answered {status} {answer}")
        nodes[1].start()
        nodes[2].start()
        until(60, lambda: node_count(HTTP[0], admin) == 3)
        time.sleep(2)
        check("no-manager: the refused user is nowhere once the cluster is back",
              all(user_ok(p, "nomanager", "No-Manager-12345")[0] != 200 for p in HTTP))
    finally:
        for n in nodes:
            n.stop()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target/release/velosearch"))
    ap.add_argument("--only", choices=["bootstrap", "saving", "rejoining"])
    ap.add_argument("--keep", action="store_true", help="leave the data directories behind")
    a = ap.parse_args()
    binary = str(pathlib.Path(a.binary).resolve())
    root = tempfile.mkdtemp(prefix="secrepl-check.", dir="/tmp")
    try:
        if a.only in (None, "bootstrap"):
            bootstrap(binary, root)
        if a.only in (None, "saving"):
            saving(binary, root)
        if a.only in (None, "rejoining"):
            rejoining(binary, root)
    finally:
        if a.keep:
            print(f"\ndata under {root}")
        else:
            for p in pathlib.Path(root).rglob("*"):
                try:
                    p.chmod(0o755 if p.is_dir() else 0o644)
                except OSError:
                    pass
            shutil.rmtree(root, ignore_errors=True)
    failed = [n for n, ok in results if not ok]
    print(f"\n  {len(results) - len(failed)}/{len(results)} checks passed")
    print("RESULT", "the security configuration holds" if not failed else "FAILED")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
