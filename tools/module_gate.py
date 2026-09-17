#!/usr/bin/env python3
"""The module corpus, run the way its suites were written.

Most of it runs against one node. Two suites do not, and no request can make
them: the URL repository suite is written against three repositories and an
HTTP fixture its build sets up, and the ingest-disabled suite is written
against a cluster with no ingest node -- which is a different cluster, not a
different request. So this runs the corpus in the passes the suites imply and
adds the numbers up, rather than letting either sit permanently red.

    tools/gate_node.sh &
    VELO_PORT=9214 VELO_DATA=/tmp/velo-noingest \\
      VELO_URL_REPO=/tmp/velo-noingest-repo \\
      VELO_ROLES=data,cluster_manager,remote_cluster_client tools/gate_node.sh &
    tools/module_gate.py
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile

MANIFEST = "tools/modules_manifest.json"
NODE = os.environ.get("VELO_URL", "http://127.0.0.1:9213")
NO_INGEST = os.environ.get("VELO_NO_INGEST_URL", "http://127.0.0.1:9214")
# the suite written against a cluster with no ingest node
APART = "smoke-test-ingest-disabled"


def run(url, files, before=None):
    """One pass of the runner over a set of files, as its own manifest."""
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        json.dump({"clean": files}, f)
        manifest = f.name
    out = tempfile.mktemp(suffix=".json")
    command = [
        sys.executable, "tools/yaml_runner.py",
        "--url", url, "--manifest", manifest, "--json-out", out,
    ]
    if before:
        command += ["--before", before]
    ran = subprocess.run(command, check=False, capture_output=True, text=True)
    # a runner that could not run wrote no answer: it used to end this gate
    # in a traceback about a missing temporary file, which says nothing about
    # what went wrong
    if not pathlib.Path(out).exists():
        print(f"the runner did not finish (exit {ran.returncode}):")
        print("\n".join((ran.stdout + ran.stderr).splitlines()[-15:]))
        sys.exit(2)
    found = json.loads(pathlib.Path(out).read_text())
    os.unlink(manifest)
    os.unlink(out)
    return found


def main():
    manifest = json.loads(pathlib.Path(MANIFEST).read_text())
    files = manifest["clean"]
    together = [f for f in files if APART not in f]
    apart = [f for f in files if APART in f]
    passes = [
        ("one node", run(NODE, together, "tools/url_repository_fixture.py")),
        ("no ingest node", run(NO_INGEST, apart)),
    ]
    total = passed = failed = skipped = 0
    for name, found in passes:
        print(
            f"  {name:18} {found['passed']:4} passed  {found['failed']:3} failed  "
            f"{found['skipped']:3} skipped"
        )
        total += found["total"]
        passed += found["passed"]
        failed += found["failed"]
        skipped += found["skipped"]
    print(f"  {'':18} {passed:4} of {total} sections, {failed} failed, {skipped} skipped")
    # files left out of the count, and why -- kept in sight so that setting one
    # aside stays a decision somebody can argue with rather than a number that
    # quietly moved
    for key, heading in (
        ("not_a_server_test", "are not tests of a server"),
        ("cannot_hold_for_one_binary", "cannot hold for one binary"),
    ):
        apart = manifest.get(key, {})
        if apart:
            print(f"\n  {len(apart)} files {heading}:")
            for name, why in apart.items():
                print(f"    {name.split('/test/')[-1]}\n      {why}")
    for name, found in passes:
        for f in found["failures"]:
            print(f"    [{name}] {f['file'].split('/test/')[-1]} :: {f['section']}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
