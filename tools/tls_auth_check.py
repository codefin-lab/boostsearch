#!/usr/bin/env python3
"""A node deployed the way a real one is: TLS on the wire, a password at the door.

Tier 2 of the production path -- one node holding data that matters -- asks
for a deployment with TLS and authentication, checked rather than assumed.
`tools/health_check.py` starts such a node to see what a probe makes of it;
this asks what a caller makes of it, and answers the questions a deployment
raises:

  * does HTTPS verify? Not `curl -k`, which proves only that something
    answered: the check pins the test CA and asks for the hostname in the
    certificate, as a client with a CA bundle would
  * is plain http refused on the port that speaks TLS
  * is a caller with no credentials refused -- everywhere except the probe's
    own path, which answers anyone by design
  * can the administrator administer, and is a user with one role held to it
  * is a wrong password refused
  * and does a node published on every interface with neither security
    configured nor security explicitly disabled refuse to start, as the
    image's own instructions say it does

The certificates are the ones in this repository's study tree, whose subject
alternative names cover localhost and 127.0.0.1; the passwords are the demo
ones a node ships with for exactly this purpose, and no secret of anyone's
goes near this check.

    python3 tools/tls_auth_check.py
"""

import argparse
import base64
import http.client
import json
import os
import pathlib
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
CERTS = ROOT / "study/security/bwc-test/src/test/resources/security"
PORT = int(os.environ.get("VELO_TLS_PORT", "9374"))
TRANSPORT = int(os.environ.get("VELO_TLS_TRANSPORT", "9474"))
OPEN_PORT = int(os.environ.get("VELO_TLS_OPEN_PORT", "9372"))
OPEN_TRANSPORT = int(os.environ.get("VELO_TLS_OPEN_TRANSPORT", "9472"))
PROBE = "/_plugins/_security/health"
ADMIN = ("admin", "Tls-Check-Key-2026")


def verified_context():
    """A client that checks the chain and the hostname, as a real one does."""
    ctx = ssl.create_default_context(cafile=str(CERTS / "root-ca.pem"))
    ctx.check_hostname = True
    ctx.verify_mode = ssl.CERT_REQUIRED
    return ctx


def ask(path, method="GET", body=None, creds=None, ctx=None, host="localhost",
        port=None, scheme="https", timeout=15):
    """(status, body, error) -- status 0 when nothing could be asked at all."""
    port = port or PORT
    headers = {"content-type": "application/json"}
    if creds:
        token = base64.b64encode(f"{creds[0]}:{creds[1]}".encode()).decode()
        headers["authorization"] = f"Basic {token}"
    try:
        if scheme == "https":
            conn = http.client.HTTPSConnection(host, port, timeout=timeout,
                                               context=ctx or verified_context())
        else:
            conn = http.client.HTTPConnection(host, port, timeout=timeout)
        conn.request(method, path, body=json.dumps(body) if body is not None else None,
                     headers=headers)
        r = conn.getresponse()
        raw = r.read()
        conn.close()
        try:
            return r.status, (json.loads(raw) if raw else {}), ""
        except ValueError:
            return r.status, {"raw": raw.decode(errors="replace")[:200]}, ""
    except Exception as e:
        return 0, {}, f"{type(e).__name__}: {e}"[:200]


class Node:
    def __init__(self, data, addr, transport, config=None, extra=None, log=None):
        env = {k: v for k, v in os.environ.items() if not k.startswith("VELOSEARCH_")}
        env.update({
            "VELOSEARCH_ADDR": addr,
            "VELOSEARCH_DATA": str(data),
            "VELOSEARCH_TRANSPORT_PORT": str(transport),
        })
        if config:
            env["VELOSEARCH_CONFIG"] = str(config)
        env.update(extra or {})
        self.log_path = log
        self.log = open(log, "ab")
        self.proc = subprocess.Popen([BINARY], env=env, stdout=self.log, stderr=subprocess.STDOUT)

    def wait_https(self, seconds=45):
        end = time.time() + seconds
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            if ask("/", creds=ADMIN)[0]:
                return True
            time.sleep(0.4)
        return False

    def wait_exit(self, seconds=20):
        end = time.time() + seconds
        while time.time() < end:
            if self.proc.poll() is not None:
                return True
            time.sleep(0.3)
        return False

    def listening(self, port):
        with socket.socket() as s:
            s.settimeout(1)
            return s.connect_ex(("127.0.0.1", port)) == 0

    def tail(self, lines=6):
        try:
            return "\n".join(pathlib.Path(self.log_path).read_text(errors="replace").splitlines()[-lines:])
        except OSError:
            return ""

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


BINARY = str(ROOT / "target/release/velosearch")


def main():
    global BINARY
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=BINARY)
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    BINARY = a.binary

    results = []

    def check(name, ok, detail=""):
        results.append(bool(ok))
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")

    work = pathlib.Path(tempfile.mkdtemp(prefix="bstls."))
    config = work / "config"
    (config / "security").mkdir(parents=True)
    (config / "velosearch.yml").write_text(
        "plugins.security.ssl.http.enabled: true\n"
        f"plugins.security.ssl.http.pemcert_filepath: {CERTS / 'esnode.pem'}\n"
        f"plugins.security.ssl.http.pemkey_filepath: {CERTS / 'esnode-key.pem'}\n"
        f"plugins.security.ssl.http.pemtrustedcas_filepath: {CERTS / 'root-ca.pem'}\n"
    )
    node = None
    open_node = None
    try:
        node = Node(work / "data", f"127.0.0.1:{PORT}", TRANSPORT, config=config,
                    extra={"VELOSEARCH_DISABLED": "false",
                           "VELOSEARCH_INITIAL_ADMIN_PASSWORD": ADMIN[1],
                           "VELOSEARCH_TRANSPORT_INSECURE": "true",
                           "VELOSEARCH_RESTAPI_ROLES_ENABLED": "all_access"},
                    log=work / "tls-node.log")
        if not node.wait_https():
            check("the node starts with TLS and security on", False, node.tail())
            return finish(results, work, a.keep)
        check("the node starts with TLS and security on", True)

        # the whole point: a client that verifies, not one that shrugs
        st, body, err = ask("/", creds=ADMIN)
        check("https verifies against the CA, with the hostname checked",
              st == 200 and body.get("name"), f"status {st} {err} {str(body)[:120]}")

        lax = ssl._create_unverified_context()
        st_lax, _, _ = ask("/", creds=ADMIN, ctx=lax)
        st_wrongname, _, err_wrongname = ask("/", creds=ADMIN, host="127.0.0.1")
        check("the certificate is the test CA's, not a self-signed accident",
              st_lax == 200 and st_wrongname == 200,
              f"unverified {st_lax}, by ip {st_wrongname} {err_wrongname}")

        st, _, err = ask("/", scheme="http", creds=ADMIN)
        check("plain http is not served on the port that speaks TLS",
              st == 0 or st >= 400, f"status {st} {err}")

        st, _, _ = ask("/_cluster/health")
        check("a caller with no credentials is refused", st == 401, f"answered {st}")
        st, _, _ = ask("/_cluster/health", creds=("admin", "not-the-password"))
        check("a wrong password is refused", st == 401, f"answered {st}")
        st, body, _ = ask(PROBE)
        check("the probe's own path answers without credentials, over TLS",
              st == 200 and body.get("status") == "UP", f"answered {st} {str(body)[:80]}")

        st, body, _ = ask("/_cluster/health", creds=ADMIN)
        check("the administrator is served", st == 200 and body.get("status"),
              f"answered {st} {str(body)[:80]}")

        # a user with one role is held to it
        ask("/theirs", "PUT", {"settings": {"index": {"number_of_shards": 1}}}, creds=ADMIN)
        ask("/not-theirs", "PUT", {"settings": {"index": {"number_of_shards": 1}}}, creds=ADMIN)
        ask("/theirs/_doc/1?refresh=true", "PUT", {"v": 1}, creds=ADMIN)
        ask("/not-theirs/_doc/1?refresh=true", "PUT", {"v": 1}, creds=ADMIN)
        # the setup is checked as well as used: a check that asserts on a user
        # it failed to create reports the server's fault as its own
        st_role, body_role, _ = ask("/_plugins/_security/api/roles/theirs_only", "PUT", {
            "index_permissions": [{"index_patterns": ["theirs"],
                                   "allowed_actions": ["read", "search"]}]}, creds=ADMIN)
        st_user, body_user, _ = ask("/_plugins/_security/api/internalusers/tenant", "PUT",
                                    {"password": "Zq7-mesa-lantern-42"}, creds=ADMIN)
        st_map, body_map, _ = ask("/_plugins/_security/api/rolesmapping/theirs_only", "PUT",
                                  {"users": ["tenant"]}, creds=ADMIN)
        check("a role, a user and a mapping can be written through the security API",
              all(200 <= st < 300 for st in (st_role, st_user, st_map)),
              f"role {st_role} {str(body_role)[:90]}; user {st_user} {str(body_user)[:90]}; "
              f"mapping {st_map} {str(body_map)[:90]}")
        tenant = ("tenant", "Zq7-mesa-lantern-42")
        st_theirs, body_theirs, _ = ask("/theirs/_search", "POST", {"query": {"match_all": {}}},
                                        creds=tenant)
        st_other, _, _ = ask("/not-theirs/_search", "POST", {"query": {"match_all": {}}},
                             creds=tenant)
        check("a user with one role reads the index it was granted",
              st_theirs == 200 and (body_theirs.get("hits") or {}).get("total"),
              f"answered {st_theirs} {str(body_theirs)[:120]}")
        check("and is refused the index it was not",
              st_other in (403, 404), f"answered {st_other}")
        st_admin_api, _, _ = ask("/_plugins/_security/api/internalusers", creds=tenant)
        check("and may not administer security", st_admin_api == 403,
              f"answered {st_admin_api}")

        # what the image says of itself: published to everyone, with neither
        # security configured nor security switched off, it does not start
        open_node = Node(work / "open", f"0.0.0.0:{OPEN_PORT}", OPEN_TRANSPORT,
                         extra={"VELOSEARCH_TRANSPORT_INSECURE": "true"},
                         log=work / "open-node.log")
        gone = open_node.wait_exit(20)
        answered = open_node.listening(OPEN_PORT)
        check("a node published on every interface with no security refuses to start",
              gone and not answered,
              f"exited {gone}, listening {answered}: {open_node.tail()}")
    finally:
        for n in (node, open_node):
            if n is not None:
                n.stop()
        if not a.keep:
            shutil.rmtree(work, ignore_errors=True)
    return finish(results, work, a.keep)


def finish(results, work, keep):
    if keep:
        print(f"data left under {work}")
    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "a TLS deployment verifies, asks for a password, and holds a user to its role"
          if results and all(results) else "FAILED")
    return 0 if results and all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
