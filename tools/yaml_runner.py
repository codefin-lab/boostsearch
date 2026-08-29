#!/usr/bin/env python3
"""Run OpenSearch's own rest-api-spec YAML tests against our Rust server.

This is the conformance harness for the port: the spec is OpenSearch's, the
implementation under test is ours. Nothing here is rewritten by hand, so a
passing test means we match OpenSearch's documented behaviour, not our idea
of it.
"""
import argparse, datetime, json, os, pathlib, re, sys, time, traceback
import yaml
import requests

SPEC_ROOT = pathlib.Path("study/OpenSearch/rest-api-spec/src/main/resources/rest-api-spec")
API_DIR = SPEC_ROOT / "api"
TEST_ROOT = SPEC_ROOT / "test"

# yaml `skip: features:` values we can honour; anything else skips the section
CATCH_CODES = {
    "bad_request": 400, "unauthorized": 401, "forbidden": 403, "missing": 404,
    "request_timeout": 408, "conflict": 409, "request": 500, "unavailable": 503,
}

SUPPORTED_FEATURES = {
    "warnings", "warnings_regex", "allowed_warnings", "allowed_warnings_regex",
    "default_shards", "contains", "headers",
}


class Loader(yaml.SafeLoader):
    """OpenSearch yaml uses duplicate keys in a few places; last one wins."""


def _no_dup(loader, node, deep=False):
    mapping = {}
    for k, v in node.value:
        mapping[loader.construct_object(k, deep=deep)] = loader.construct_object(v, deep=deep)
    return mapping


Loader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, _no_dup)


def load_api_specs():
    specs = {}
    for f in API_DIR.glob("*.json"):
        if f.name == "_common.json":
            continue
        try:
            d = json.loads(f.read_text())
        except Exception:
            continue
        for name, body in d.items():
            specs[name] = body
    return specs


class Failure(Exception):
    pass


def flatten_path(obj, path):
    """Resolve a dotted assertion path like `hits.hits.0._source.count`."""
    if path in ("", "$body"):
        return obj
    cur = obj
    # split on unescaped dots
    parts = re.split(r"(?<!\\)\.", path)
    for raw in parts:
        key = raw.replace("\\.", ".")
        if cur is None:
            return None
        if isinstance(cur, list):
            try:
                cur = cur[int(key)]
            except (ValueError, IndexError):
                return None
        elif isinstance(cur, dict):
            if key not in cur:
                return None
            cur = cur[key]
        else:
            return None
    return cur


class Runner:
    def __init__(self, base_url, specs, verbose=False):
        self.base = base_url.rstrip("/")
        self.specs = specs
        self.stash = {}
        self.last = None
        self.last_req = None
        self.verbose = verbose
        self.session = requests.Session()

    # ---- stash -----------------------------------------------------------
    def unstash(self, value):
        if isinstance(value, str):
            if value.startswith("$"):
                return self.stash.get(value[1:], value)
            if "${" in value:
                def sub(m):
                    return str(self.stash.get(m.group(1), m.group(0)))
                return re.sub(r"\$\{([^}]+)\}", sub, value)
            return value
        if isinstance(value, dict):
            return {k: self.unstash(v) for k, v in value.items()}
        if isinstance(value, list):
            return [self.unstash(v) for v in value]
        return value

    # ---- request building ------------------------------------------------
    def resolve_path(self, api, params):
        spec = self.specs.get(api)
        if spec is None:
            raise Failure(f"unknown api [{api}]")
        candidates = spec["url"]["paths"]
        best, best_score = None, -1
        for cand in candidates:
            parts = cand.get("parts", {})
            if not set(parts).issubset(set(params)):
                continue
            score = len(parts)
            if score > best_score:
                best, best_score = cand, score
        if best is None:
            raise Failure(f"no path for [{api}] with params {sorted(params)}")
        path = best["path"]
        used = set()
        for name in best.get("parts", {}):
            val = params[name]
            if isinstance(val, list):
                val = ",".join(str(v) for v in val)
            elif val is None:
                # `index: null` in a test means the caller sent nothing there,
                # not the four letters of Python's None
                val = ""
            path = path.replace("{" + name + "}", requests.utils.quote(str(val), safe=",*"))
            used.add(name)
        method = best["methods"][0]
        if "POST" in best["methods"] and "body" in params:
            method = "POST"
        return method, path, used

    def do(self, action):
        action = dict(action)
        catch = action.pop("catch", None)
        for meta in ("warnings", "allowed_warnings", "warnings_regex",
                     "allowed_warnings_regex", "headers", "node_selector"):
            action.pop(meta, None)
        if not action:
            raise Failure("empty do block")
        # a few suite files put more than one action in a single `do`; the
        # OpenSearch runner executes each of them in order
        apis = list(action.items())
        for api, params in apis[:-1]:
            self.do_one(api, params, None)
        api, params = apis[-1]
        return self.do_one(api, params, catch)

    def do_one(self, api, params, catch):
        params = self.unstash(params or {})
        if not isinstance(params, dict):
            params = {}
        body = params.pop("body", None)
        ignore = params.pop("ignore", None)
        ignore_codes = set()
        if ignore is not None:
            vals = ignore if isinstance(ignore, list) else [ignore]
            ignore_codes = {int(v) for v in vals}
        try:
            method, path, used = self.resolve_path(api, params)
        except Failure:
            # `catch: param` expects the client to refuse the call because a
            # required path part is missing -- which is exactly what a failure
            # to resolve one means
            if catch == "param":
                return
            raise
        query = {k: v for k, v in params.items() if k not in used and v is not None}
        for k, v in list(query.items()):
            if isinstance(v, bool):
                query[k] = "true" if v else "false"
            elif isinstance(v, list):
                query[k] = ",".join(str(x) for x in v)

        headers = {"Content-Type": "application/json"}
        data = None
        if body is not None:
            if isinstance(body, (list, tuple)):  # bulk / msearch ndjson
                data = "".join(
                    (x if isinstance(x, str) else json.dumps(x)) + "\n" for x in body
                )
                headers["Content-Type"] = "application/x-ndjson"
            elif isinstance(body, str):
                data = body
            else:
                data = json.dumps(body, default=_json_default)

        url = self.base + path
        resp = self.session.request(method, url, params=query, data=data,
                                    headers=headers, timeout=30)
        try:
            parsed = resp.json() if resp.content else None
        except Exception:
            # cat APIs answer in plain text; the suite matches on the body itself
            parsed = resp.text
        # HEAD-style APIs return no body and the suite asserts on the outcome
        # itself; a GET that simply answered with an empty body is still a body.
        self.last_req = (method, url, body)
        if parsed is None:
            self.last = (resp.status_code < 400) if method == "HEAD" else ""
        else:
            self.last = parsed

        if catch:
            if resp.status_code < 400:
                raise Failure(f"expected catch [{catch}] but got {resp.status_code}")
            # a `catch` must match the error OpenSearch documents, not just any
            # error -- otherwise a blanket 501 would pass every negative test
            if isinstance(catch, str) and catch.startswith("/") and catch.endswith("/"):
                blob = json.dumps(parsed)
                if not re.search(catch[1:-1], blob):
                    raise Failure(f"catch {catch} did not match {blob[:200]}")
            else:
                want = CATCH_CODES.get(catch)
                if want and resp.status_code != want:
                    raise Failure(f"catch [{catch}] expected HTTP {want}, "
                                  f"got {resp.status_code}")
            return
        if resp.status_code in ignore_codes:
            return
        if resp.status_code >= 400:
            if method == "HEAD" and not resp.content:
                return  # exists-style API: False is the answer, not a failure
            raise Failure(f"{method} {path} -> {resp.status_code}: "
                          f"{json.dumps(parsed)[:300]}")

    # ---- assertions ------------------------------------------------------
    def assert_match(self, spec):
        for path, expected in spec.items():
            actual = flatten_path(self.last, path)
            expected = self.unstash(expected)
            # a regex may carry surrounding whitespace from a yaml block scalar
            stripped = expected.strip() if isinstance(expected, str) else expected
            if isinstance(stripped, str) and len(stripped) > 2 \
                    and stripped.startswith("/") and stripped.endswith("/"):
                # Verbose mode is what the multi-line patterns need -- they
                # are laid out in columns with comments. It also throws away
                # the spaces inside a one-line pattern, where they are part of
                # the text being matched, so both readings are tried.
                text = str(actual if actual is not None else "")
                pat = stripped[1:-1]
                if not (re.search(pat, text, re.X) or re.search(pat, text)):
                    raise Failure(f"match {path}: {actual!r} !~ {expected}")
                continue
            if isinstance(expected, (int, float)) and isinstance(actual, (int, float)) \
                    and not isinstance(expected, bool):
                if float(actual) != float(expected):
                    raise Failure(f"match {path}: {actual!r} != {expected!r}")
                continue
            if actual != expected:
                raise Failure(f"match {path}: {actual!r} != {expected!r}")

    def assert_length(self, spec):
        for path, expected in spec.items():
            actual = flatten_path(self.last, path)
            if actual is None:
                raise Failure(f"length {path}: missing")
            if len(actual) != int(self.unstash(expected)):
                raise Failure(f"length {path}: {len(actual)} != {expected}")

    def assert_bool(self, spec, want):
        paths = spec if isinstance(spec, list) else [spec]
        for path in paths:
            actual = flatten_path(self.last, path)
            truthy = not (actual is None or actual is False or actual == 0
                          or actual == "" or actual == "false")
            if truthy != want:
                raise Failure(f"is_{'true' if want else 'false'} {path}: {actual!r}")

    def assert_cmp(self, spec, op):
        import operator
        fn = {"gt": operator.gt, "lt": operator.lt,
              "gte": operator.ge, "lte": operator.le}[op]
        for path, expected in spec.items():
            actual = flatten_path(self.last, path)
            expected = self.unstash(expected)
            if actual is None or not fn(actual, expected):
                raise Failure(f"{op} {path}: {actual!r} vs {expected!r}")

    def do_set(self, spec):
        for path, name in spec.items():
            self.stash[name] = flatten_path(self.last, path)

    def assert_contains(self, spec):
        for path, expected in spec.items():
            actual = flatten_path(self.last, path)
            expected = self.unstash(expected)
            if not isinstance(actual, list) or expected not in actual:
                raise Failure(f"contains {path}: {expected!r} not in {actual!r}")

    # ---- driving ---------------------------------------------------------
    def run_steps(self, steps):
        for step in steps or []:
            if not isinstance(step, dict):
                continue
            for key, val in step.items():
                if key == "do":
                    self.do(val)
                elif key == "match":
                    self.assert_match(val)
                elif key == "length":
                    self.assert_length(val)
                elif key == "is_true":
                    self.assert_bool(val, True)
                elif key == "is_false":
                    self.assert_bool(val, False)
                elif key in ("gt", "lt", "gte", "lte"):
                    self.assert_cmp(val, key)
                elif key == "set":
                    self.do_set(val)
                elif key == "contains":
                    self.assert_contains(val)
                elif key in ("skip", "transform_and_set"):
                    pass
                else:
                    raise Failure(f"unsupported step [{key}]")


def should_skip(steps):
    for step in steps or []:
        if isinstance(step, dict) and "skip" in step:
            sk = step["skip"] or {}
            if str(sk.get("version", "")).strip().lower() == "all":
                return "disabled upstream (skip version: all)"
            feats = sk.get("features")
            if feats:
                feats = [feats] if isinstance(feats, str) else feats
                unsupported = set(feats) - SUPPORTED_FEATURES
                if unsupported:
                    return f"features {sorted(unsupported)}"
    return None


def _json_default(o):
    """YAML turns an unquoted date into a datetime; the wire wants the text."""
    if isinstance(o, (datetime.datetime, datetime.date)):
        return o.isoformat()
    raise TypeError(f"not JSON serialisable: {type(o).__name__}")


def reset(base):
    # Templates outlive a `DELETE /*`, and an index template left behind by an
    # earlier file changes how the next file's indices are created -- so the
    # suite has to start from nothing, not just from no indices.
    # a point in time outlives the indices it was opened over
    try:
        requests.delete(base + "/_search/point_in_time/_all", timeout=10)
    except Exception:
        pass
    for path in ("/*", "/_index_template/*", "/_template/*", "/_component_template/*"):
        try:
            requests.delete(base + path, timeout=10)
        except Exception:
            pass
    # Cluster settings outlive an index too, and one file's transient setting
    # changes what the next file sees. There is no wildcard delete for them,
    # so whatever is set is read back and cleared by name.
    try:
        held = requests.get(base + "/_cluster/settings?flat_settings=true", timeout=10).json()
        clear = {
            scope: {k: None for k in held.get(scope, {})}
            for scope in ("persistent", "transient")
            if held.get(scope)
        }
        if clear:
            requests.put(base + "/_cluster/settings", json=clear, timeout=10)
    except Exception:
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:9200")
    ap.add_argument("--manifest", default="tools/phase1_manifest.json")
    ap.add_argument("--filter", default="")
    ap.add_argument("--json-out", default="")
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--show", type=int, default=15, help="how many failures to print")
    args = ap.parse_args()

    specs = load_api_specs()
    files = json.loads(pathlib.Path(args.manifest).read_text())["clean"]
    if args.filter:
        files = [f for f in files if args.filter in f]

    try:
        requests.get(args.url, timeout=5)
    except Exception as e:
        print(f"cannot reach server at {args.url}: {e}")
        sys.exit(2)

    total = passed = failed = skipped = 0
    failures = []
    per_file = {}

    for rel in files:
        path = TEST_ROOT / rel
        try:
            docs = [d for d in yaml.load_all(path.read_text(errors="replace"), Loader=Loader) if d]
        except Exception as e:
            failures.append((rel, "<parse>", f"yaml parse: {e}"))
            failed += 1
            total += 1
            continue

        setup = teardown = None
        sections = []
        for doc in docs:
            for name, steps in doc.items():
                if name == "setup":
                    setup = steps
                elif name == "teardown":
                    teardown = steps
                else:
                    sections.append((name, steps))

        fp = fa = fs = 0
        for name, steps in sections:
            total += 1
            reason = should_skip(steps) or should_skip(setup)
            if reason:
                skipped += 1
                fs += 1
                continue
            reset(args.url)
            r = Runner(args.url, specs, args.verbose)
            try:
                r.run_steps(setup)
                r.run_steps(steps)
                passed += 1
                fp += 1
            except Failure as e:
                failed += 1
                fa += 1
                req = getattr(r, "last_req", None)
                ctx = (
                    f"  <- {req[0]} {req[1]} {json.dumps(req[2], default=_json_default)[:220]}"
                    if req
                    else ""
                )
                failures.append((rel, name, str(e) + ctx))
            except Exception as e:
                failed += 1
                fa += 1
                failures.append((rel, name, f"{type(e).__name__}: {e}"))
            finally:
                try:
                    r.run_steps(teardown)
                except Exception:
                    pass
        per_file[rel] = {"pass": fp, "fail": fa, "skip": fs}

    print(f"\n{'='*66}")
    print(f"  files {len(files)}   sections {total}")
    print(f"  PASS {passed}   FAIL {failed}   SKIP {skipped}")
    rate = (passed / (passed + failed) * 100) if (passed + failed) else 0.0
    print(f"  pass rate (excl. skipped): {rate:.1f}%")
    print(f"{'='*66}\n")

    if failures and args.show:
        buckets = {}
        for rel, name, msg in failures:
            key = re.sub(r"\d+", "N", msg.split(":")[0])[:70]
            buckets.setdefault(key, []).append((rel, name, msg))
        print("failure clusters (largest first):")
        for key, items in sorted(buckets.items(), key=lambda kv: -len(kv[1])):
            print(f"\n  [{len(items):>3}] {key}")
            for rel, name, msg in items[:3]:
                print(f"        {rel} :: {name}")
                print(f"          {msg[:150]}")

    if args.json_out:
        pathlib.Path(args.json_out).write_text(json.dumps({
            "total": total, "passed": passed, "failed": failed, "skipped": skipped,
            "per_file": per_file,
            "failures": [{"file": f, "section": s, "error": m} for f, s, m in failures],
        }, indent=1))

    sys.exit(0 if failed == 0 else 1)


main()
