#!/usr/bin/env python3
"""How many of OpenSearch's REST APIs this engine routes at all.

The README used to claim a number for this that nothing measured. This is
what measures it: OpenSearch publishes one JSON file per API in its
`rest-api-spec`, each naming the paths and methods that API answers on, and
this crosses those against the routes the server registers in `src/main.rs`.

It is a claim about routing, not about behaviour -- an API counted here is one
a request reaches a handler through. What the handler then answers is what the
YAML suites (`tools/yaml_runner.py`, `tools/module_gate.py`) and the
comparison against a real node (`tools/compat_audit.py`) are for.

    python3 tools/endpoint_gate.py            # the counts
    python3 tools/endpoint_gate.py --missing  # and what is not routed
"""
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = ROOT / "study/OpenSearch/rest-api-spec/src/main/resources/rest-api-spec/api"
MAIN = ROOT / "src/main.rs"

METHODS = ("get", "post", "put", "delete", "head", "patch", "any")


def routes():
    """Every path the server registers, with the methods it takes there."""
    text = MAIN.read_text()
    found = {}
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', text):
        path = m.group(1)
        # the handler expression runs to the next `.route(` or the end
        rest = text[m.end():]
        stop = rest.find(".route(")
        body = rest if stop < 0 else rest[:stop]
        methods = {v.upper() for v in METHODS if re.search(rf"\b{v}\(", body)}
        if "ANY" in methods:
            methods = {"GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"}
        found.setdefault(path, set()).update(methods)
    return found


def segments(path):
    return [s for s in path.strip("/").split("/") if s != ""]


def routed(path, method, table):
    """Whether a request on this path and method reaches a handler."""
    want = segments(path)
    for route, methods in table.items():
        have = segments(route)
        if len(have) != len(want) or method not in methods:
            continue
        ok = True
        for h, w in zip(have, want):
            if h.startswith("{"):
                continue  # a parameter takes whatever is there
            if h != w:
                ok = False
                break
        if ok:
            return True
    return False


def main():
    table = routes()
    answered, partial, missing = [], [], []
    detail = {}
    for file in sorted(SPEC.glob("*.json")):
        if file.name == "_common.json":
            continue
        spec = json.loads(file.read_text())
        name = next(iter(spec))
        url = spec[name].get("url", {})
        paths = url.get("paths", [])
        asked, reached = [], []
        for p in paths:
            for method in p.get("methods", []):
                asked.append((p["path"], method))
                if routed(p["path"], method, table):
                    reached.append((p["path"], method))
        if not asked:
            continue
        detail[name] = (len(reached), len(asked))
        if len(reached) == len(asked):
            answered.append(name)
        elif reached:
            partial.append(name)
        else:
            missing.append(name)
    total = len(detail)
    print(f"{len(answered)} of {total} APIs are routed on every path and method they name")
    print(f"{len(partial)} of {total} are routed on some of them")
    print(f"{len(missing)} of {total} are not routed at all")
    if "--missing" in sys.argv:
        for name in partial:
            got, asked = detail[name]
            print(f"  partial: {name} ({got} of {asked})")
        for name in missing:
            print(f"  none:    {name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
