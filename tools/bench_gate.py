#!/usr/bin/env python3
"""The performance gate: this build against the last one, and against the
numbers OpenSearch gave when it was measured.

ADR 0004 asks for two gates -- every commit against our own last measurement,
every release against OpenSearch -- and for a long time neither existed in any
workflow, because the second one needs an OpenSearch to measure against and CI
has none. It does not need one every time: OpenSearch was measured on this
corpus on a machine, its numbers were kept (`bench/results/final-os-clean-*.json`),
and what a run has to prove afterwards is that this engine has not fallen
behind *itself*. The recorded pair is reported beside it, so the claim that
this engine is ahead is a number somebody can read rather than a memory.

    python3 tools/bench_gate.py --write     # take a baseline on this machine
    python3 tools/bench_gate.py             # measure, compare, and say

A baseline records the machine it was taken on. On another machine the numbers
are printed and compared but nothing fails: a slower laptop is not a
regression. `--strict` fails anyway, which is what a machine that is meant to
be the same one should use.
"""
import argparse
import glob
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "tools" / "bench_baseline.json"
RECORDED_US = "bench/results/final-velosearch-clean-*.json"
RECORDED_THEM = "bench/results/final-os-clean-*.json"

# How far a dimension may fall before the gate is red. ADR 0004's number.
SLIP = 0.05

# ...and how far it has to fall in its own units as well.
#
# Five per cent of half a millisecond is twenty-five microseconds, which is
# less than the spread between two runs of the same build: without a floor the
# gate reddens on noise and is then ignored, which is worse than not having
# one. So a baseline is taken from several runs and records what this machine's
# own spread is, dimension by dimension, and a fall smaller than that spread is
# not a fall. The numbers below are the least it will ever demand, for the case
# where several runs happened to agree exactly.
FLOOR = [
    ("rss_mb", 8.0),        # megabytes
    ("latency_", 0.15),     # milliseconds
    ("_p50_", 0.15),
    ("_p90_", 0.15),
]

# how many runs a baseline is taken from
ROUNDS_FOR_BASELINE = 3


def floor_for(name, spread):
    """How far this dimension must move before the move means anything."""
    least = 0.0
    for part, amount in FLOOR:
        if part in name:
            least = amount
            break
    # what the machine was seen to vary by, with a little room over it
    return max(least, spread * 1.5)


def machine():
    """What the numbers were taken on, closely enough to know it changed."""
    cpu = platform.processor() or platform.machine()
    if sys.platform == "darwin":
        try:
            cpu = subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"],
                capture_output=True, text=True, check=False,
            ).stdout.strip() or cpu
        except Exception:
            pass
    return {"system": platform.system(), "release": platform.release(), "cpu": cpu,
            "cores": os.cpu_count()}


def dimensions(run):
    """One run of `tools/bench.py`, as the numbers the gate holds.

    Higher is better for the throughputs, lower for the latencies and the
    memory, which is what `better` says.
    """
    out = {}
    out["index_docs_per_sec"] = (run["indexing"]["index_docs_per_sec"], "higher")
    out["rss_mb_idle"] = (run.get("rss_mb_idle", 0), "lower")
    out["rss_mb_after_index"] = (run.get("rss_mb_after_index", 0), "lower")
    out["rss_mb_after_search"] = (run.get("rss_mb_after_search", 0), "lower")
    for level in run.get("search", []):
        at = level["concurrency"]
        out[f"qps_c{at}"] = (level["qps"], "higher")
        out[f"latency_p50_c{at}"] = (level["latency_ms"]["p50"], "lower")
        out[f"latency_p90_c{at}"] = (level["latency_ms"]["p90"], "lower")
        for name, per in level.get("per_query_ms", {}).items():
            out[f"{name}_p50_c{at}"] = (per["p50"], "lower")
    return out


def median_of(pattern):
    """The middle of several recorded runs, dimension by dimension."""
    runs = [json.load(open(f)) for f in sorted(glob.glob(str(ROOT / pattern)))]
    if not runs:
        return {}
    each = [dimensions(r) for r in runs]
    names = set().union(*[set(d) for d in each])
    out = {}
    for name in sorted(names):
        values = [d[name][0] for d in each if name in d]
        way = next(d[name][1] for d in each if name in d)
        if values:
            out[name] = (statistics.median(values), way)
    return out


def measure(binary, port, transport, rounds, data):
    """Start a node, run the bench against it, and read the numbers back."""
    where = tempfile.mkdtemp(prefix="velo-bench-")
    env = dict(os.environ)
    env.update({"VELOSEARCH_ADDR": f"127.0.0.1:{port}", "VELOSEARCH_DATA": where,
                "VELOSEARCH_TRANSPORT_PORT": str(transport)})
    log = open(pathlib.Path(where) / "node.log", "w")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    url = f"http://127.0.0.1:{port}"
    try:
        for _ in range(60):
            try:
                urllib.request.urlopen(url, timeout=2).read()
                break
            except Exception:
                if node.poll() is not None:
                    sys.exit(f"the node did not start; its log is in {where}/node.log")
                time.sleep(1)
        out = tempfile.mktemp(suffix=".json")
        ran = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "bench.py"), "--url", url,
             "--rounds", str(rounds), "--data", data, "--out", out,
             # this node's own memory, not the largest of whatever else on
             # this machine happens to be called velosearch
             "--proc", f"pid:{node.pid}"],
            capture_output=True, text=True, check=False, cwd=ROOT,
        )
        if not pathlib.Path(out).exists():
            print("\n".join((ran.stdout + ran.stderr).splitlines()[-15:]))
            sys.exit("the bench did not finish")
        return json.load(open(out))
    finally:
        node.terminate()
        try:
            node.wait(timeout=10)
        except Exception:
            node.kill()


def compare(now, before, spreads, slip):
    """Every dimension that fell further than `slip`, and by how much."""
    lost = []
    for name, (value, way) in sorted(now.items()):
        if name not in before:
            continue
        was = before[name][0]
        if not was:
            continue
        change = (value - was) / was
        # a latency going up is a loss; a throughput going down is a loss
        worse = change if way == "lower" else -change
        # a fall smaller than this dimension's own noise is not a fall
        if abs(value - was) < floor_for(name, spreads.get(name, 0.0)):
            worse = min(worse, 0.0)
        lost.append((name, was, value, worse))
    return lost


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "velosearch"))
    ap.add_argument("--port", type=int, default=9231)
    ap.add_argument("--transport", type=int, default=9331)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--data", default=str(ROOT / "bench" / "data" / "http_logs.ndjson"))
    ap.add_argument("--write", action="store_true", help="take this run as the baseline")
    ap.add_argument("--strict", action="store_true", help="fail even on another machine")
    args = ap.parse_args()

    if not pathlib.Path(args.data).exists():
        print(f"no corpus at {args.data}; see bench/README or tools/gen_dataset.py")
        return 2

    if args.write:
        # several runs, so the baseline knows what this machine's own spread
        # is: a gate that cannot tell noise from a change is a gate nobody
        # believes
        taken = []
        for round_at in range(ROUNDS_FOR_BASELINE):
            print(f"  run {round_at + 1} of {ROUNDS_FOR_BASELINE}…")
            taken.append(
                dimensions(measure(args.binary, args.port, args.transport, args.rounds, args.data))
            )
        names = sorted(set().union(*[set(d) for d in taken]))
        held = {}
        for name in names:
            values = [d[name][0] for d in taken if name in d]
            way = next(d[name][1] for d in taken if name in d)
            held[name] = {
                "value": statistics.median(values),
                "better": way,
                "spread": round(max(values) - min(values), 4),
            }
        BASELINE.write_text(json.dumps(
            {"machine": machine(), "taken": time.strftime("%Y-%m-%d"),
             "runs": ROUNDS_FOR_BASELINE, "dimensions": held},
            indent=1,
        ) + "\n")
        print(f"baseline written to {BASELINE.relative_to(ROOT)} ({len(held)} dimensions)")
        return 0

    run = measure(args.binary, args.port, args.transport, args.rounds, args.data)
    now = dimensions(run)

    if not BASELINE.exists():
        print("no baseline yet; take one with --write")
        return 2
    held = json.loads(BASELINE.read_text())
    before = {k: (v["value"], v["better"]) for k, v in held["dimensions"].items()}
    spreads = {k: v.get("spread", 0.0) for k, v in held["dimensions"].items()}
    same_machine = held.get("machine") == machine()

    print(f"baseline taken {held.get('taken')} on {held.get('machine', {}).get('cpu')}")
    if not same_machine:
        print("  this is a different machine: the comparison is printed, not enforced")

    lost = compare(now, before, spreads, SLIP)
    slipped = [row for row in lost if row[3] > SLIP]
    for name, was, value, worse in lost:
        mark = "  " if worse <= SLIP else "->"
        print(f"{mark} {name:28} {was:>12.2f} -> {value:>12.2f}  {worse * -100:+6.1f}%")

    # what OpenSearch gave when it was measured, and what we gave beside it
    theirs = median_of(RECORDED_THEM)
    ours_then = median_of(RECORDED_US)
    if theirs and ours_then:
        behind = []
        for name, (their_value, way) in sorted(theirs.items()):
            if name not in ours_then:
                continue
            our_value = ours_then[name][0]
            if not their_value:
                continue
            ahead = (their_value - our_value) / their_value
            if way == "higher":
                ahead = (our_value - their_value) / their_value
            if ahead <= 0:
                behind.append(name)
        print(
            f"\nrecorded against OpenSearch ({len(theirs)} dimensions, "
            f"measured once and kept): ahead on {len(theirs) - len(behind)}"
        )
        for name in behind:
            print(f"  behind: {name}")

    if slipped:
        print(f"\nRESULT {len(slipped)} dimension(s) fell more than {SLIP:.0%}")
        return 1 if (same_machine or args.strict) else 0
    print(f"\nRESULT nothing fell more than {SLIP:.0%}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
