#!/usr/bin/env python3
"""What the console's server answers that its own API suite never asks about.

`test/api_integration` is 166 cases and covers saved objects thoroughly, but
it never touches several things Phase 13 has to answer: the shell the browser
boots from, `uiSettings`, the capabilities object, the Dev Tools proxy, and
half of the saved-object management routes. A replacement could pass that
whole suite and still not serve a single page.

So this is the other half. Every check says what the answer must be, and every
expectation here was measured against OpenSearch Dashboards 3.1.0 rather than
read out of a document -- the metadata the front end boots from is a contract
between two halves of one program that nobody wrote down, which is exactly why
it has to be checked rather than described.

    tools/dashboards_check.py                     against the reference
    tools/dashboards_check.py --url http://…      against ours
"""
import argparse
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

failures = []
expected_failures = []
NODE = "http://127.0.0.1:5613"

# What OpenSearch Dashboards 3.1.0 itself does not answer, measured against the
# released container. A replacement is not held to these -- but they are named
# rather than dropped, because a check quietly deleted is a check nobody
# remembers to bring back when the reason goes away.
REFERENCE_FAILS = {
    "and names the object it refers to":
        "follows from the 400 above; there is nothing to name when the request "
        "was refused.",
    "what an object refers to is answerable":
        "the released server answers 400: the `.kibana` mapping it creates does "
        "not declare `references` as nested, and the query it builds needs that. "
        "Its own suite fails the same route in `saved objects management apis "
        "relationships`.",
}


def call(path, method="GET", body=None, headers=None):
    """One request, and what came back: status, headers, body."""
    data = None
    head = {"content-type": "application/json", "osd-xsrf": "true"}
    head.update(headers or {})
    if body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
    request = urllib.request.Request(NODE + path, data=data, headers=head, method=method)
    opener = urllib.request.build_opener(NoRedirect)
    try:
        with opener.open(request, timeout=30) as answer:
            return answer.status, dict(answer.headers), answer.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """A redirect is an answer worth checking, not something to follow."""

    def redirect_request(self, *_args):
        return None


def as_json(raw):
    try:
        return json.loads(raw or b"{}")
    except ValueError:
        return None


def note(what, detail):
    """A check that did not hold, unless the reference does not hold it either."""
    if what in REFERENCE_FAILS:
        expected_failures.append(what)
        return
    failures.append(f"{what}{detail}")


def expect(what, got, want):
    if got != want:
        note(what, f"\n      got  {got!r}\n      want {want!r}")


def holds(what, condition, why=""):
    if not condition:
        note(what, f": {why}" if why else "")


# ---- the shell (13.1) ------------------------------------------------------

def the_shell():
    """What the browser is served before it has run any JavaScript."""
    status, headers, body = call("/app/home")
    expect("an application path is a page", status, 200)
    holds("and it is HTML", headers.get("content-type", "").startswith("text/html"))
    # the front end boots from metadata the server puts in the page; without it
    # the application starts and immediately fails, which looks like a browser
    # problem and is not
    text = body.decode(errors="replace")
    holds("the page carries the metadata the front end boots from",
          "injectedMetadata" in text or "osd-injected-metadata" in text or "csp.nonce" in text,
          "no injected metadata in the served page")
    holds("and the script that starts it", "bootstrap.js" in text)

    # the policy is the reason an injected script cannot run, so it is part of
    # the answer rather than a header somebody may add later
    status, headers, _ = call("/app/home")
    policy = headers.get("content-security-policy", "")
    holds("a page is served with a content security policy", bool(policy))
    holds("that names where scripts may come from", "script-src" in policy)

    status, _, _ = call("/bootstrap.js")
    expect("the boot script is served", status, 200)

    status, headers, _ = call("/translations/en.json")
    expect("the translations the front end asks for are served", status, 200)
    holds("as JSON", "json" in headers.get("content-type", ""))

    # the root is not a page: it says where to go
    status, headers, _ = call("/")
    expect("the root redirects", status, 302)
    holds("to an application", "/app/" in headers.get("location", ""),
          f"went to {headers.get('location')!r}")


# ---- settings and capabilities (13.2) --------------------------------------

def settings_and_capabilities():
    status, _, body = call("/api/opensearch-dashboards/settings")
    expect("the settings are readable", status, 200)
    found = as_json(body) or {}
    holds("and come back under `settings`", "settings" in found)

    # a setting written is a setting read back
    status, _, _ = call("/api/opensearch-dashboards/settings", "POST",
                        {"changes": {"dateFormat:tz": "Asia/Bangkok"}})
    expect("a setting can be written", status, 200)
    _, _, body = call("/api/opensearch-dashboards/settings")
    found = (as_json(body) or {}).get("settings", {})
    expect("and reads back as what was written",
           found.get("dateFormat:tz", {}).get("userValue"), "Asia/Bangkok")

    status, _, _ = call("/api/opensearch-dashboards/settings/dateFormat:tz", "DELETE")
    expect("and can be set back to its default", status, 200)
    _, _, body = call("/api/opensearch-dashboards/settings")
    found = (as_json(body) or {}).get("settings", {})
    holds("which leaves no user value behind", "dateFormat:tz" not in found)

    status, _, body = call("/api/core/capabilities", "POST", {"applications": []})
    expect("the capabilities are readable", status, 200)
    found = as_json(body) or {}
    for key in ("navLinks", "management", "catalogue"):
        holds(f"capabilities carry `{key}`", key in found)


# ---- status and stats (13.2) -----------------------------------------------

def status_and_stats():
    status, _, body = call("/api/status")
    expect("status answers", status, 200)
    found = as_json(body) or {}
    for key in ("name", "uuid", "version", "status"):
        holds(f"status carries `{key}`", key in found)
    holds("and says how the server as a whole is",
          isinstance(found.get("status", {}).get("overall", {}).get("state"), str))


# ---- the saved-object management routes the pages call (13.3, 13.4) --------

def management_routes():
    status, _, body = call("/api/opensearch-dashboards/management/saved_objects/_allowed_types")
    expect("the types the management page may show are listed", status, 200)
    types = (as_json(body) or {}).get("types", [])
    for kind in ("index-pattern", "dashboard", "visualization", "config"):
        holds(f"`{kind}` is one of them", kind in types)

    # a saved object and something that refers to it
    call("/api/saved_objects/index-pattern/check-ip", "POST",
         {"attributes": {"title": "check-*"}})
    call("/api/saved_objects/visualization/check-vis", "POST",
         {"attributes": {"title": "a check"},
          "references": [{"name": "ref", "type": "index-pattern", "id": "check-ip"}]})

    status, _, body = call(
        "/api/opensearch-dashboards/management/saved_objects/relationships/visualization/check-vis"
        "?savedObjectTypes=index-pattern&savedObjectTypes=visualization")
    expect("what an object refers to is answerable", status, 200)
    related = as_json(body)
    holds("and names the object it refers to",
          isinstance(related, list) and any(r.get("id") == "check-ip" for r in related),
          f"got {str(related)[:120]}")

    status, _, body = call(
        "/api/opensearch-dashboards/management/saved_objects/visualization/check-vis")
    expect("one object is fetchable through the management route", status, 200)
    holds("and carries its title", (as_json(body) or {}).get("meta", {}).get("title") == "a check")

    call("/api/saved_objects/visualization/check-vis", "DELETE")
    call("/api/saved_objects/index-pattern/check-ip", "DELETE")


# ---- the Dev Tools proxy (13.4) --------------------------------------------

def console_proxy():
    status, _, body = call("/api/console/opensearch_config")
    expect("the console is told which engine it is talking to", status, 200)
    holds("by its address", "host" in (as_json(body) or {}))

    status, _, body = call("/api/console/proxy?path=/&method=GET", "POST")
    expect("the proxy carries a request through", status, 200)
    found = as_json(body) or {}
    holds("and brings the engine's own answer back", "version" in found, f"got {found!r}")

    # the proxy is a way to reach the engine, not a way to reach anything
    status, _, _ = call("/api/console/proxy?path=/_cat/indices&method=GET", "POST")
    expect("a cat request goes through too", status, 200)


# ---- index patterns (13.4) -------------------------------------------------

def index_patterns():
    status, _, body = call(
        "/api/index_patterns/_fields_for_wildcard?pattern=*&meta_fields=_source&meta_fields=_id")
    expect("the fields behind a pattern are answerable", status, 200)
    fields = (as_json(body) or {}).get("fields")
    holds("and come back as a list", isinstance(fields, list))
    if isinstance(fields, list) and fields:
        one = fields[0]
        for key in ("name", "type", "searchable", "aggregatable"):
            holds(f"each field says `{key}`", key in one, f"got {sorted(one)}")


def main():
    global NODE
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=NODE)
    args = ap.parse_args()
    NODE = args.url.rstrip("/")

    for name, check in [
        ("the shell the browser boots from", the_shell),
        ("settings and capabilities", settings_and_capabilities),
        ("status", status_and_stats),
        ("the saved-object management routes", management_routes),
        ("the Dev Tools proxy", console_proxy),
        ("the fields behind an index pattern", index_patterns),
    ]:
        before = len(failures)
        try:
            check()
        except Exception as e:  # a check that cannot run is a check that failed
            failures.append(f"{name}: {type(e).__name__}: {e}")
        print(f"  {'ok    ' if len(failures) == before else 'FAILED'} {name}")
    for line in failures:
        print("   ", line)
    for what in dict.fromkeys(expected_failures):
        print(f"    [the reference does not answer this either] {what}")
        print(f"      {REFERENCE_FAILS[what]}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
