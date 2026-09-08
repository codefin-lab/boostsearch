#!/usr/bin/env python3
"""OpenSearch Dashboards' own API suite, against whichever server is named.

Phase 13 replaces the Node server the Dashboards front end talks to. The
suite that says whether the replacement answers the same way is the one
OpenSearch Dashboards already has: `test/api_integration`, 166 cases over
saved objects, index patterns, settings, status, stats and the rest. It takes
a running server through `TEST_OPENSEARCH_DASHBOARDS_URL`, which is what makes
it possible to point at ours.

The released Node server does not pass all of it. Twenty-four cases fail
against OpenSearch Dashboards 3.1.0 itself -- the branch the suite lives on
has drifted from the release, and a few of the tests were broken by OpenSearch
3.x rather than by anything Dashboards did. Those are recorded here with what
each one says, so that what this gate reports is how our server compares with
the real one rather than with a perfect score nothing reaches.

    tools/dashboards_gate.py                     against the reference
    tools/dashboards_gate.py --url http://…      against ours

The server has to be started the way the suite's own config starts it --
`tools/dashboards_reference.sh` does that, and prints the settings that
matter and why.
"""
import argparse
import json
import os
import pathlib
import re
import subprocess
import sys

REPO = pathlib.Path("study/OpenSearch-Dashboards")
BASELINE = pathlib.Path("tools/dashboards_baseline.json")


def run(url, opensearch, node):
    """The suite, against a server that is already up."""
    env = dict(os.environ)
    env["TEST_OPENSEARCH_DASHBOARDS_URL"] = url
    env["TEST_OPENSEARCH_URL"] = opensearch
    # the repo pins its Node, and the one on the path is usually not it
    command = [node, "scripts/functional_test_runner", "--config", "test/api_integration/config.js"]
    out = subprocess.run(command, cwd=REPO, env=env, capture_output=True, text=True)
    return out.returncode, out.stdout + out.stderr


def read(text):
    """What the runner said: the totals and which cases failed."""
    totals = {}
    for name in ("passing", "failing", "pending"):
        found = re.search(rf"(\d+) {name}", text)
        totals[name] = int(found.group(1)) if found else 0
    failed = [
        line.split("✖ fail: ", 1)[1].strip()
        for line in text.splitlines()
        if "✖ fail: " in line
    ]
    # the cases that ran and passed, by name -- a case whose suite's hook
    # failed never ran, and is neither passed nor failed
    passed = [
        line.split('"', 1)[1].rsplit('"', 1)[0]
        for line in text.splitlines()
        if "✓ pass" in line and line.count('"') >= 2
    ]
    return totals, failed, passed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://admin:admin@127.0.0.1:5613",
                    help="the Dashboards-compatible server under test")
    ap.add_argument("--opensearch", default="http://admin:admin@127.0.0.1:9221",
                    help="the engine behind it, which the suite loads its fixtures into")
    ap.add_argument("--node", default="node", help="a Node the repo can run under (20.x)")
    ap.add_argument("--write-baseline", action="store_true",
                    help="record this run as what the reference server itself does")
    args = ap.parse_args()

    if not REPO.exists():
        print(f"{REPO} is not there. It is the suite, so it has to be fetched:")
        print("  git clone --depth 1 -b 3.1 "
              "https://github.com/opensearch-project/OpenSearch-Dashboards study/OpenSearch-Dashboards")
        print("  cd study/OpenSearch-Dashboards && yarn osd bootstrap")
        return 2

    status, text = run(args.url, args.opensearch, args.node)
    totals, failed, passed = read(text)
    # A runner that could not run -- the wrong Node, a config it could not
    # read, a server that is not there -- prints no totals, and the counts
    # come back zero. That used to read as "nothing of ours failed": the
    # gate was green having measured nothing.
    if not passed and not failed:
        print(f"  the suite ran no case at all (the runner exited {status})")
        print("\n".join(text.splitlines()[-15:]))
        return 2

    if args.write_baseline:
        BASELINE.write_text(json.dumps({
            "what": "what the reference server does against its own suite: the cases "
                    "that ran and failed, and the cases that ran and passed. A case in "
                    "neither never ran, because a hook before it failed.",
            "totals": totals,
            "failing": failed,
            "passing": passed,
        }, indent=2) + "\n")
        print(f"  {totals['passing']} passing, {totals['failing']} failing, "
              f"{totals['pending']} pending -- written to {BASELINE}")
        return 0

    known = {}
    if BASELINE.exists():
        known = json.loads(BASELINE.read_text())
    reference = set(known.get("failing", []))
    reference_passing = set(known.get("passing", []))
    # a failure is ours alone when the reference passed that case; one the
    # reference also failed, or never ran, is not ours
    ours = [f for f in failed if f in reference_passing] if reference_passing else [
        f for f in failed if f not in reference
    ]
    also = [f for f in failed if f not in ours]

    print(f"  {totals['passing']:3} passing   {totals['failing']:3} failing   "
          f"{totals['pending']:3} pending")
    if reference:
        print(f"  {len(also):3} of the failures are ones the reference server has too")
        print(f"  {len(ours):3} are ours alone")
    for f in ours:
        print(f"    {f}")
    # a case the reference fails and we pass is worth saying out loud: either
    # we are better than it or the case is not measuring what it thinks
    fixed = [f for f in reference if f in passed]
    for f in fixed:
        print(f"    [passes here, fails against the reference] {f}")
    never_ran = [f for f in passed if reference_passing and f not in reference_passing and f not in reference]
    if never_ran:
        print(f"  {len(never_ran):3} pass here that never ran against the reference (a hook failed there)")
    return 1 if ours else 0


if __name__ == "__main__":
    sys.exit(main())
