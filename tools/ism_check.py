#!/usr/bin/env python3
"""Index State Management, end to end.

OpenSearch keeps index management in a plugin with its own repository and its
own suite, which is not part of the corpus this repository runs. This is what
stands in for it: a policy is written, an index is put under it, and the thing
is watched actually happening -- states entered, actions run, the index rolled
over and in the end deleted by the policy rather than by anyone.

Run against a node started with a short job interval, which is what
`BOOSTSEARCH_ISM_INTERVAL_MS` is for:

    BOOSTSEARCH_ISM_INTERVAL_MS=2000 ./target/release/boostsearch
    python3 tools/ism_check.py
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

NODE = os.environ.get("BOOST_URL", "http://127.0.0.1:9213")
# how long one tick takes, so the checks wait for a tick rather than a guess
TICK = float(os.environ.get("BOOST_ISM_TICK", "2.5"))
failures = []


def req(method, path, body=None):
    request = urllib.request.Request(
        NODE + path,
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            return json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as e:
        return {"error": e.code, "body": e.read()[:300].decode()}


def expect(what, got, want):
    if got != want:
        failures.append(f"{what}: {got!r}, expected {want!r}")


def wait_for(what, look, want, ticks=8):
    """Wait for the engine to get somewhere, rather than for a fixed time."""
    for _ in range(ticks):
        if look() == want:
            return True
        time.sleep(TICK)
    failures.append(f"{what}: still {look()!r} after {ticks} ticks, expected {want!r}")
    return False


def state_of(index):
    one = req("GET", f"/_plugins/_ism/explain/{index}").get(index, {})
    return (one.get("state") or {}).get("name")


def policy_crud():
    policy = {
        "policy": {
            "description": "for the sake of being read back",
            "default_state": "only",
            "states": [{"name": "only", "actions": [], "transitions": []}],
        }
    }
    made = req("PUT", "/_plugins/_ism/policies/crud-policy", policy)
    expect("writing a policy", made.get("_id"), "crud-policy")
    read = req("GET", "/_plugins/_ism/policies/crud-policy")
    expect(
        "reading it back",
        read.get("policy", {}).get("description"),
        "for the sake of being read back",
    )
    listed = req("GET", "/_plugins/_ism/policies")
    expect(
        "it is in the list",
        any(p["_id"] == "crud-policy" for p in listed.get("policies", [])),
        True,
    )
    expect("deleting it", req("DELETE", "/_plugins/_ism/policies/crud-policy").get("result"), "deleted")
    expect(
        "reading a policy that is gone",
        req("GET", "/_plugins/_ism/policies/crud-policy").get("error"),
        404,
    )


def states_and_actions():
    """The whole point: an index moves itself along and ends up deleted."""
    req("DELETE", "/ism-life")
    req(
        "PUT",
        "/_plugins/_ism/policies/life-policy",
        {
            "policy": {
                "description": "read-only, then gone",
                "default_state": "hot",
                "states": [
                    {
                        "name": "hot",
                        "actions": [],
                        "transitions": [
                            {"state_name": "warm", "conditions": {"min_doc_count": 3}}
                        ],
                    },
                    {
                        "name": "warm",
                        "actions": [{"read_only": {}}],
                        "transitions": [
                            {"state_name": "gone", "conditions": {"min_state_age": "1s"}}
                        ],
                    },
                    {"name": "gone", "actions": [{"delete": {}}], "transitions": []},
                ],
            }
        },
    )
    req("PUT", "/ism-life")
    added = req("POST", "/_plugins/_ism/add/ism-life", {"policy_id": "life-policy"})
    expect("putting an index under a policy", added.get("updated_indices"), 1)
    expect("adding it twice", req("POST", "/_plugins/_ism/add/ism-life", {"policy_id": "life-policy"}).get("failures"), True)
    expect("the state it starts in", state_of("ism-life"), "hot")

    # three documents is what the transition is waiting for
    for i in range(4):
        req("POST", f"/ism-life/_doc/{i}?refresh=true", {"n": i})
    wait_for("moving to warm", lambda: state_of("ism-life"), "warm")
    # the action of that state is read_only, which is a real setting
    for _ in range(6):
        settings = req("GET", "/ism-life/_settings")
        blocked = settings.get("ism-life", {}).get("settings", {}).get("index", {}).get("blocks", {})
        if blocked.get("write") in (True, "true"):
            break
        time.sleep(TICK)
    # settings come back as strings, which is what OpenSearch does too
    expect("the index was made read-only", str(blocked.get("write")).lower(), "true")
    # and then the policy deletes it
    wait_for("the index is deleted by its policy", lambda: req("GET", "/ism-life/_count").get("error"), 404)
    req("DELETE", "/_plugins/_ism/policies/life-policy")


def rollover():
    req("DELETE", "/rolling-000001,rolling-000002")
    req(
        "PUT",
        "/_plugins/_ism/policies/roll-policy",
        {
            "policy": {
                "description": "roll when there are documents in it",
                "default_state": "open",
                "states": [
                    {
                        "name": "open",
                        "actions": [{"rollover": {"min_doc_count": 2}}],
                        "transitions": [],
                    }
                ],
            }
        },
    )
    req("PUT", "/rolling-000001", {"aliases": {"rolling": {"is_write_index": True}}})
    req("POST", "/_plugins/_ism/add/rolling-000001", {"policy_id": "roll-policy"})
    for i in range(3):
        req("POST", f"/rolling-000001/_doc/{i}?refresh=true", {"n": i})
    wait_for(
        "the next index exists",
        lambda: req("GET", "/rolling-000002/_count").get("error") is None,
        True,
    )
    behind = req("GET", "/rolling/_alias")
    expect("the alias moved to the new index", list(behind), ["rolling-000002"])
    req("DELETE", "/rolling-000001,rolling-000002")
    req("DELETE", "/_plugins/_ism/policies/roll-policy")


def templates():
    """A policy that names index patterns picks up indices made afterwards."""
    req("DELETE", "/auto-managed-1")
    req(
        "PUT",
        "/_plugins/_ism/policies/template-policy",
        {
            "policy": {
                "description": "anything called auto-*",
                "default_state": "only",
                "states": [{"name": "only", "actions": [], "transitions": []}],
                "ism_template": [{"index_patterns": ["auto-*"], "priority": 10}],
            }
        },
    )
    req("PUT", "/auto-managed-1")
    wait_for(
        "an index made afterwards is picked up",
        lambda: req("GET", "/_plugins/_ism/explain/auto-managed-1")
        .get("auto-managed-1", {})
        .get("policy_id"),
        "template-policy",
    )
    req("DELETE", "/auto-managed-1")
    req("DELETE", "/_plugins/_ism/policies/template-policy")


def change_and_remove():
    req("DELETE", "/ism-change")
    for name in ("first-policy", "second-policy"):
        req(
            "PUT",
            f"/_plugins/_ism/policies/{name}",
            {
                "policy": {
                    "description": name,
                    "default_state": "idle",
                    "states": [{"name": "idle", "actions": [], "transitions": []}],
                }
            },
        )
    req("PUT", "/ism-change")
    req("POST", "/_plugins/_ism/add/ism-change", {"policy_id": "first-policy"})
    req("POST", "/_plugins/_ism/change_policy/ism-change", {"policy_id": "second-policy"})
    expect(
        "the policy changed",
        req("GET", "/_plugins/_ism/explain/ism-change").get("ism-change", {}).get("policy_id"),
        "second-policy",
    )
    expect(
        "removing the policy",
        req("POST", "/_plugins/_ism/remove/ism-change").get("updated_indices"),
        1,
    )
    expect(
        "it is no longer managed",
        req("GET", "/_plugins/_ism/explain/ism-change").get("ism-change", {}).get("policy_id"),
        None,
    )
    expect(
        "removing it again",
        req("POST", "/_plugins/_ism/remove/ism-change").get("failures"),
        True,
    )
    req("DELETE", "/ism-change")
    for name in ("first-policy", "second-policy"):
        req("DELETE", f"/_plugins/_ism/policies/{name}")


def retry_after_failure():
    """An action that cannot work is retried, and `retry` clears the failure."""
    req("DELETE", "/ism-failing")
    req(
        "PUT",
        "/_plugins/_ism/policies/failing-policy",
        {
            "policy": {
                "description": "snapshot into a repository that is not there",
                "default_state": "trying",
                "states": [
                    {
                        "name": "trying",
                        "actions": [{"snapshot": {"repository": "nowhere", "snapshot": "never"}}],
                        "transitions": [],
                    }
                ],
            }
        },
    )
    req("PUT", "/ism-failing")
    req("POST", "/_plugins/_ism/add/ism-failing", {"policy_id": "failing-policy"})
    wait_for(
        "the action is recorded as failed",
        lambda: req("GET", "/_plugins/_ism/explain/ism-failing")
        .get("ism-failing", {})
        .get("retry_info", {})
        .get("failed"),
        True,
    )
    expect(
        "retrying clears it",
        req("POST", "/_plugins/_ism/retry/ism-failing").get("updated_indices"),
        1,
    )
    expect(
        "the retry count is back to nothing",
        req("GET", "/_plugins/_ism/explain/ism-failing")
        .get("ism-failing", {})
        .get("retry_info", {})
        .get("consumed_retries"),
        0,
    )
    req("DELETE", "/ism-failing")
    req("DELETE", "/_plugins/_ism/policies/failing-policy")


if __name__ == "__main__":
    for name, check in [
        ("policies can be written, read and deleted", policy_crud),
        ("an index moves through its states", states_and_actions),
        ("a policy rolls an index over", rollover),
        ("a policy claims the indices it names", templates),
        ("a policy can be changed and removed", change_and_remove),
        ("a failed action is retried", retry_after_failure),
    ]:
        before = len(failures)
        check()
        print(f"  {'ok    ' if len(failures) == before else 'FAILED'} {name}")
    for line in failures:
        print("   ", line)
    sys.exit(1 if failures else 0)
