#!/usr/bin/env python3
"""The container's healthcheck, against the nodes it has to tell apart.

A healthcheck that asks `_cluster/health` over plain http with no
credentials reports a healthy node unhealthy: a node with authentication on
answers 401 and a node with TLS on does not answer at all. The probe has to
tell healthy from unready with security off, with authentication on and with
TLS on.

This starts a node of each kind and runs the probe the Dockerfile runs --
https first, plain http after, no credentials -- against it, and the old
probe beside it so the difference is on the page. A fourth node is started
as one of a cluster whose other members never come: it has no cluster
manager, refuses every write, and the probe must call it unready.

With authentication on, the probe's endpoint is the only thing a caller with
no credentials is let through to: the check asks `_cluster/health` and `/`
too, and a write to the health path, and each must still be refused.

    python3 tools/health_check.py [--binary ./target/release/velosearch]
"""

import argparse
import json
import os
import pathlib
import shutil
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
CERTS = ROOT / "study/security/bwc-test/src/test/resources/security"
PROBE_PATH = "/_plugins/_security/health"

UNVERIFIED = ssl.create_default_context()
UNVERIFIED.check_hostname = False
UNVERIFIED.verify_mode = ssl.CERT_NONE


def ask(url, method="GET", timeout=3):
    """(status, body) -- status 0 when nothing answered at all."""
    req = urllib.request.Request(url, method=method, data=b"{}" if method != "GET" else None,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=UNVERIFIED) as r:
            raw = r.read()
            return r.status, raw.decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception:
        return 0, ""


def new_probe(port):
    """What the Dockerfile's HEALTHCHECK does: curl -sfk https || curl -sf http."""
    for scheme in ("https", "http"):
        status, body = ask(f"{scheme}://127.0.0.1:{port}{PROBE_PATH}")
        if status:
            return 200 <= status < 300, status, body
    return False, 0, ""


def old_probe(port):
    status, _ = ask(f"http://127.0.0.1:{port}/_cluster/health")
    return 200 <= status < 300, status


class Node:
    def __init__(self, binary, root, name, port, env, yml=""):
        self.name = name
        self.port = port
        data = pathlib.Path(root) / name
        config = data / "config"
        (config / "security").mkdir(parents=True, exist_ok=True)
        if yml:
            (config / "velosearch.yml").write_text(yml)
        full = dict(os.environ)
        for k in [k for k in full if k.startswith("VELOSEARCH_")]:
            del full[k]
        full.update({
            "VELOSEARCH_ADDR": f"127.0.0.1:{port}",
            "VELOSEARCH_DATA": str(data),
            "VELOSEARCH_CONFIG": str(config),
            "VELOSEARCH_TRANSPORT_PORT": str(port + 100),
            "VELOSEARCH_NODE_NAME": name,
        })
        full.update(env)
        self.log_path = data / "node.log"
        self.log = open(self.log_path, "wb")
        self.proc = subprocess.Popen([binary], env=full, stdout=self.log, stderr=subprocess.STDOUT)

    def wait_answering(self, seconds=40):
        end = time.time() + seconds
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            for scheme in ("https", "http"):
                if ask(f"{scheme}://127.0.0.1:{self.port}/", timeout=1)[0]:
                    return True
            time.sleep(0.3)
        return False

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()

    def tail(self):
        try:
            return "\n".join(self.log_path.read_text(errors="replace").splitlines()[-8:])
        except OSError:
            return ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target/release/velosearch"))
    ap.add_argument("--keep", action="store_true", help="leave the data directories behind")
    a = ap.parse_args()

    root = tempfile.mkdtemp(prefix="bshealth.")
    tls_yml = (
        "plugins.security.ssl.http.enabled: true\n"
        f"plugins.security.ssl.http.pemcert_filepath: {CERTS / 'esnode.pem'}\n"
        f"plugins.security.ssl.http.pemkey_filepath: {CERTS / 'esnode-key.pem'}\n"
        f"plugins.security.ssl.http.pemtrustedcas_filepath: {CERTS / 'root-ca.pem'}\n"
    )
    single = {"VELOSEARCH_TRANSPORT_INSECURE": "true"}
    kinds = [
        ("security-off", 9388, {**single, "VELOSEARCH_PLUGINS_SECURITY_DISABLED": "true"}, ""),
        ("auth-on", 9390, {**single, "VELOSEARCH_DISABLED": "false"}, ""),
        ("tls-on", 9392, {**single, "VELOSEARCH_DISABLED": "false"}, tls_yml),
        # one of three whose other two never come: no cluster manager, ever
        ("unready", 9394, {
            "VELOSEARCH_PLUGINS_SECURITY_DISABLED": "true",
            "VELOSEARCH_TRANSPORT_INSECURE": "true",
            "VELOSEARCH_DISCOVERY_SEED_HOSTS": "127.0.0.1:9496,127.0.0.1:9498",
            "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": "unready,absent-1,absent-2",
        }, ""),
    ]
    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")

    nodes = []
    try:
        for name, port, env, yml in kinds:
            nodes.append(Node(a.binary, root, name, port, env, yml))
        for n in nodes:
            if not n.wait_answering():
                check(f"{n.name}: the node starts and answers", False, n.tail())
        # the unready node is given longer than an election takes, so its
        # DOWN is not the moment before it elects itself
        time.sleep(6)
        for n in nodes:
            healthy, status, body = new_probe(n.port)
            old_healthy, old_status = old_probe(n.port)
            expect = n.name != "unready"
            what = "healthy" if expect else "unready"
            check(
                f"{n.name}: the probe calls it {what}",
                healthy == expect,
                f"probe answered {status} {body[:160]!r}",
            )
            print(f"         (the old probe: {'healthy' if old_healthy else 'unhealthy'}, {old_status or 'no answer'})")
            if n.name == "unready":
                check("unready: the probe's answer says why", status == 503 and '"DOWN"' in body,
                      f"{status} {body[:160]!r}")
        auth = next(n for n in nodes if n.name == "auth-on")
        for method, path in [("GET", "/_cluster/health"), ("GET", "/"), ("POST", PROBE_PATH),
                             ("GET", "/_plugins/_security/api/internalusers")]:
            status, _ = ask(f"http://127.0.0.1:{auth.port}{path}", method)
            check(f"auth-on: {method} {path} with no credentials is still refused",
                  status == 401 or status == 405, f"answered {status}")
    finally:
        for n in nodes:
            n.stop()
        if not a.keep:
            shutil.rmtree(root, ignore_errors=True)
        else:
            print(f"data under {root}")

    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "the probe tells healthy from unready in every kind" if all(results) else "FAILED")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
