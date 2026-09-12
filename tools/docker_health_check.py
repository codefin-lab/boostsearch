#!/usr/bin/env python3
"""The image's own healthcheck, in the container, in every mode it ships for.

PR-09 of the production-readiness review of 2026-09-07 asked for the image to
be probed with security off, with authentication on and with TLS on, and for
healthy to be told apart from unready. `tools/health_check.py` asks the
questions of a binary; this asks them of the built image, which is what an
orchestrator actually watches: Docker runs the HEALTHCHECK itself, and this
reads the verdict back with `docker inspect`.

Four containers, each removed afterwards:

  * security off, which must become healthy
  * authentication on, which must become healthy without credentials -- and a
    caller with none must still be refused everything else
  * TLS on, which must become healthy over https
  * one of a cluster whose other members never come, which has no cluster
    manager, refuses every write, and must never be called healthy

No ports are published: everything is asked from inside the container, so
this runs beside anything else without taking a port from it.

    python3 tools/docker_health_check.py [--image boostsearch:r32]
"""

import argparse
import json
import pathlib
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
CERTS = ROOT / "study/security/bwc-test/src/test/resources/security"
PROBE = "/_plugins/_security/health"


def docker(*args, timeout=120):
    return subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout)


def health(name):
    """starting / healthy / unhealthy, or '' when there is no verdict yet."""
    r = docker("inspect", "--format", "{{if .State.Health}}{{.State.Health.Status}}{{end}}", name)
    return r.stdout.strip()


def wait_health(name, want, seconds=150):
    """Wait for a verdict; the answer is what it was when the wait ended."""
    end = time.time() + seconds
    last = ""
    while time.time() < end:
        last = health(name)
        if last == want:
            return True, last
        if last == "" and docker("inspect", "--format", "{{.State.Running}}", name).stdout.strip() != "true":
            return False, "the container stopped"
        time.sleep(2)
    return False, last or "no verdict"


def inside(name, *cmd):
    r = docker("exec", name, *cmd)
    return r.returncode, (r.stdout + r.stderr).strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--image", default="boostsearch:r32")
    a = ap.parse_args()

    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")

    work = pathlib.Path(tempfile.mkdtemp(prefix="bsdocker."))
    config = work / "config"
    config.mkdir()
    (config / "boostsearch.yml").write_text(
        "plugins.security.ssl.http.enabled: true\n"
        "plugins.security.ssl.http.pemcert_filepath: /etc/boostsearch/certs/esnode.pem\n"
        "plugins.security.ssl.http.pemkey_filepath: /etc/boostsearch/certs/esnode-key.pem\n"
        "plugins.security.ssl.http.pemtrustedcas_filepath: /etc/boostsearch/certs/root-ca.pem\n"
    )

    single = ["-e", "BOOSTSEARCH_TRANSPORT_INSECURE=true"]
    kinds = [
        ("bs-off", ["-e", "BOOSTSEARCH_PLUGINS_SECURITY_DISABLED=true", *single], "healthy"),
        ("bs-auth", ["-e", "BOOSTSEARCH_DISABLED=false", *single], "healthy"),
        ("bs-tls", ["-e", "BOOSTSEARCH_DISABLED=false", *single,
                    "-v", f"{config}:/etc/boostsearch:ro",
                    "-v", f"{CERTS}:/etc/boostsearch/certs:ro"], "healthy"),
        # its two peers never come: no cluster manager, ever
        ("bs-unready", ["-e", "BOOSTSEARCH_PLUGINS_SECURITY_DISABLED=true",
                        "-e", "BOOSTSEARCH_TRANSPORT_INSECURE=true",
                        "-e", "BOOSTSEARCH_NODE_NAME=unready",
                        "-e", "BOOSTSEARCH_DISCOVERY_SEED_HOSTS=10.255.255.1:9300,10.255.255.2:9300",
                        "-e", "BOOSTSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES=unready,absent-1,absent-2"],
         "unhealthy"),
    ]

    started = []
    try:
        for name, args, _want in kinds:
            docker("rm", "-f", name)
            r = docker("run", "-d", "--name", name, *args, a.image)
            if r.returncode != 0:
                check(f"{name}: the container starts", False, r.stderr.strip()[:300])
                continue
            started.append(name)
        for name, _args, want in kinds:
            if name not in started:
                continue
            ok, saw = wait_health(name, want)
            what = "healthy" if want == "healthy" else "unready"
            check(f"{name}: the image's healthcheck calls it {what}", ok, f"it said {saw!r}")
            # what the probe before the thirtieth review would have said
            code, _ = inside(name, "curl", "-sf", "http://127.0.0.1:9200/_cluster/health")
            print(f"         (the old probe: {'healthy' if code == 0 else 'unhealthy'})")
        if "bs-auth" in started:
            for path in ["/_cluster/health", "/", "/_plugins/_security/api/internalusers"]:
                code, out = inside("bs-auth", "curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                                   f"http://127.0.0.1:9200{path}")
                check(f"bs-auth: {path} with no credentials is still refused",
                      out.strip() == "401", f"answered {out.strip()!r}")
            code, out = inside("bs-auth", "curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                               f"http://127.0.0.1:9200{PROBE}")
            check("bs-auth: the probe's own path answers without credentials",
                  out.strip() == "200", f"answered {out.strip()!r}")
        if "bs-unready" in started:
            code, out = inside("bs-unready", "curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                               f"http://127.0.0.1:9200{PROBE}")
            check("bs-unready: the probe's path says 503", out.strip() == "503",
                  f"answered {out.strip()!r}")
    finally:
        for name in started:
            docker("rm", "-f", name)

    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "the image tells healthy from unready in every mode"
          if results and all(results) else "FAILED")
    return 0 if results and all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
