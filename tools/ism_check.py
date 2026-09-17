#!/usr/bin/env python3
"""Index State Management, end to end.

OpenSearch keeps index management in a plugin with its own repository and its
own suite, which is not part of the corpus this repository runs. This is what
stands in for it: a policy is written, an index is put under it, and the thing
is watched actually happening -- states entered, actions run, the index rolled
over and in the end deleted by the policy rather than by anyone.

The read surfaces the plugins publish beside it are checked here too -- the
scheduler's job list, the long-running-operation notifications, the sweeper's
own statistics -- because they answer out of the same configuration index and
the same sweeps, and an index under a policy is what puts anything in them.

Run against a node started with a short job interval, which is what
`VELOSEARCH_ISM_INTERVAL_MS` is for:

    VELOSEARCH_ISM_INTERVAL_MS=2000 ./target/release/velosearch
    python3 tools/ism_check.py
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

NODE = os.environ.get("VELO_URL", "http://127.0.0.1:9213")
# how long one tick takes, so the checks wait for a tick rather than a guess
TICK = float(os.environ.get("VELO_ISM_TICK", "2.5"))
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
    # An alias that names a write index keeps naming the index it rolled out
    # of -- read-only -- and the new index becomes the one written through.
    # This used to expect the alias to leave the old index altogether, which
    # is what a *plain* alias does; a write alias does not, here or in the
    # reference, and the server was right while this check was wrong.
    behind = req("GET", "/rolling/_alias")
    expect(
        "the alias still names both indices",
        sorted(behind),
        ["rolling-000001", "rolling-000002"],
    )
    write_index = [
        name
        for name, body in behind.items()
        if body.get("aliases", {}).get("rolling", {}).get("is_write_index") is True
    ]
    expect("the write index moved to the new index", write_index, ["rolling-000002"])
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
    # The engine ticks every couple of seconds and this policy's action fails
    # every time it runs, so between the retry and this read the count may
    # already have been spent once more. Nothing larger than that is a race:
    # a retry that did not reset the count leaves it at three.
    consumed = (
        req("GET", "/_plugins/_ism/explain/ism-failing")
        .get("ism-failing", {})
        .get("retry_info", {})
        .get("consumed_retries")
    )
    expect("the retry count was reset", consumed in (0, 1), True)
    req("DELETE", "/ism-failing")
    req("DELETE", "/_plugins/_ism/policies/failing-policy")


def scheduled_job_surface():
    """The scheduler's own read surface lists the jobs that are really there."""
    req("DELETE", "/ism-scheduled")
    req(
        "PUT",
        "/_plugins/_ism/policies/scheduled-policy",
        {
            "policy": {
                "description": "nothing to do, only to be scheduled",
                "default_state": "only",
                "states": [{"name": "only", "actions": [], "transitions": []}],
            }
        },
    )
    req("PUT", "/ism-scheduled")
    req("POST", "/_plugins/_ism/add/ism-scheduled", {"policy_id": "scheduled-policy"})
    jobs = req("GET", "/_plugins/_job_scheduler/api/jobs")
    mine = [j for j in jobs.get("jobs", []) if j.get("name") == "ism-scheduled"]
    expect("the index under a policy is listed as a job", len(mine), 1)
    if mine:
        job = mine[0]
        expect("under the job type index management registers", job.get("job_type"),
                "opendistro-index-management")
        expect("out of the configuration index", job.get("index_name"), ".opendistro-ism-config")
        expect("scheduled", (job.get("enabled"), job.get("descheduled")), (True, False))
        expect("on an interval", (job.get("schedule") or {}).get("type"), "interval")
    expect("the count matches the list", jobs.get("total_jobs"), len(jobs.get("jobs", [])))
    expect("no job holds a lock", req("GET", "/_plugins/_job_scheduler/api/locks"),
            {"total_locks": 0, "locks": {}})
    # the sweeper is running, so the node reports itself on schedule
    stats = req("GET", "/_plugins/_alerting/stats")
    expect("the node is on schedule", stats.get("nodes_on_schedule"), 1)
    expect("and none is not", stats.get("nodes_not_on_schedule"), 0)
    node = next(iter((stats.get("nodes") or {}).values()), {})
    expect("its schedule status is green", node.get("schedule_status"), "green")
    expect(
        "the sweep is on time",
        (node.get("job_scheduling_metrics") or {}).get("full_sweep_on_time"),
        True,
    )
    expect("no alerting job is registered", node.get("jobs_info"), {})
    expect(
        "no notification is set on a long-running operation",
        req("GET", "/_plugins/_im/lron"),
        {"lron_configs": [], "total_number": 0},
    )
    req("DELETE", "/ism-scheduled")
    req("DELETE", "/_plugins/_ism/policies/scheduled-policy")


def plugin_read_surface():
    """The plugin read surfaces answer, in the shape a client reads them in.

    Each of these reports a subsystem that is not run here, so what is checked
    is that the answer is the empty or zero one rather than a refusal: a
    dashboard that asks for any of them must get a body it can parse.
    """
    empty = {
        "/_insights/top_queries": {"top_queries": []},
        "/_insights/live_queries": {"live_queries": []},
        "/_plugins/_query/_datasources": [],
        "/_plugins/_replication/autofollow_stats": {
            "num_success_start_replication": 0,
            "num_failed_start_replication": 0,
            "num_failed_leader_calls": 0,
            "failed_indices": [],
            "autofollow_stats": [],
        },
    }
    for path, want in empty.items():
        expect(f"GET {path}", req("GET", path), want)
    # the ones whose bodies carry a node id or the cluster's name are checked
    # by the keys they must hold
    for path, keys in [
        ("/_insights/health_stats", ["TopQueriesHealthStats", "FieldTypeCacheStats"]),
        ("/_insights/settings", []),
        ("/_plugins/_ltr/stats", ["cache", "request_total_count"]),
        ("/_plugins/_ltr/stats/", ["cache", "request_total_count"]),
        ("/_plugins/_knn/stats/", []),
    ]:
        body = req("GET", path)
        expect(f"GET {path} answers", body.get("error"), None)
        under = body.get("nodes", body)
        one = next(iter(under.values()), {}) if isinstance(under, dict) else {}
        for key in keys:
            expect(f"GET {path} reports {key}", key in one, True)
    # the insights settings are a settings read, so where an operator has
    # written nothing the reference's own default is what comes back
    expect(
        "the query insights settings read back their defaults",
        req("GET", "/_insights/settings").get("persistent"),
        {
            "latency": {"enabled": True, "top_n_size": 10, "window_size": "5m"},
            "cpu": {"enabled": True, "top_n_size": 10, "window_size": "5m"},
            "memory": {"enabled": True, "top_n_size": 10, "window_size": "5m"},
            "grouping": {"group_by": "none"},
            "exporter": {"type": "local_index", "delete_after_days": 7},
        },
    )
    # and what the operator writes stands in place of the default
    req(
        "PUT",
        "/_cluster/settings",
        {
            "persistent": {
                "search.insights.top_queries.cpu.enabled": False,
                "search.insights.top_queries.latency.top_n_size": 25,
                "search.insights.top_queries.grouping.group_by": "similarity",
                "search.insights.top_queries.exporter.type": "none",
            }
        },
    )
    written = req("GET", "/_insights/settings").get("persistent") or {}
    expect("a collector an operator turned off", written.get("cpu", {}).get("enabled"), False)
    expect("a size an operator set", written.get("latency", {}).get("top_n_size"), 25)
    expect("a grouping an operator chose", written.get("grouping"), {"group_by": "similarity"})
    expect(
        "an exporter an operator turned off",
        written.get("exporter"),
        {"type": "none", "delete_after_days": 7},
    )
    expect(
        "a default the operator left alone",
        written.get("memory"),
        {"enabled": True, "top_n_size": 10, "window_size": "5m"},
    )
    req(
        "PUT",
        "/_cluster/settings",
        {
            "persistent": {
                "search.insights.top_queries.cpu.enabled": None,
                "search.insights.top_queries.latency.top_n_size": None,
                "search.insights.top_queries.grouping.group_by": None,
                "search.insights.top_queries.exporter.type": None,
            }
        },
    )
    expect(
        "the defaults come back once the operator's values are cleared",
        (req("GET", "/_insights/settings").get("persistent") or {}).get("cpu"),
        {"enabled": True, "top_n_size": 10, "window_size": "5m"},
    )
    # the channel types name what a notification configuration may be, so the
    # list is the reference's even though no configuration is kept here
    expect(
        "the notification channel types the API spells",
        req("GET", "/_plugins/_notifications/features"),
        {
            "allowed_config_type_list": [
                "slack",
                "chime",
                "microsoft_teams",
                "webhook",
                "email",
                "sns",
                "ses_account",
                "smtp_account",
                "email_group",
            ],
            "plugin_features": {"tooltip_support": "true"},
        },
    )
    # the workflow steps name what a template may say; nothing provisions one
    steps = req("GET", "/_plugins/_flow_framework/workflow/_steps")
    expect("the workflow step catalogue's size", len(steps), 21)
    expect(
        "the workflow steps, in the reference's order",
        list(steps),
        [
            "update_search_pipeline",
            "update_ingest_pipeline",
            "reindex",
            "register_model_group",
            "undeploy_model",
            "register_remote_model",
            "register_local_sparse_encoding_model",
            "create_index",
            "delete_agent",
            "create_ingest_pipeline",
            "register_local_pretrained_model",
            "update_index",
            "create_tool",
            "noop",
            "create_connector",
            "register_agent",
            "deploy_model",
            "create_search_pipeline",
            "register_local_custom_model",
            "delete_connector",
            "delete_model",
        ],
    )
    expect(
        "a step that takes nothing and gives nothing",
        steps.get("noop"),
        {"inputs": [], "outputs": [], "required_plugins": []},
    )
    expect(
        "a step that names the plugin it needs and how long it waits",
        steps.get("deploy_model"),
        {
            "inputs": ["model_id"],
            "outputs": ["model_id"],
            "required_plugins": ["opensearch-ml"],
            "timeout": "15s",
        },
    )
    expect(
        "a step with no plugin behind it",
        steps.get("create_index"),
        {
            "inputs": ["index_name", "configurations"],
            "outputs": ["index_name"],
            "required_plugins": [],
        },
    )
    # the follower and leader counters are zero, and zero for every index
    for path in ("follower_stats", "leader_stats"):
        body = req("GET", f"/_plugins/_replication/{path}")
        expect(f"cross-cluster {path} are empty", body.get("index_stats"), {})
        expect(f"cross-cluster {path} read nothing", body.get("operations_read"), 0)
    # the performance analyzer reports its switches off, on every path it
    # takes a read on
    for feature in ("", "rca/", "logging/", "batch/", "threadContentionMonitoring/"):
        node = req("GET", f"/_plugins/_performanceanalyzer/{feature}config")
        expect(f"the {feature or 'plugin'} switch is off",
                node.get("performanceAnalyzerEnabled"), False)
        cluster = req("GET", f"/_plugins/_performanceanalyzer/{feature}cluster/config")
        expect(f"the cluster holds no {feature or 'plugin'} state",
                cluster.get("currentPerformanceAnalyzerClusterState"), 0)
    overrides = req("GET", "/_plugins/_performanceanalyzer/override/cluster/config").get(
        "overrides"
    )
    expect(
        "nothing is overridden",
        json.loads(overrides or "{}"),
        {
            "enable": {"rcas": [], "deciders": [], "actions": [], "collectors": []},
            "disable": {"rcas": [], "deciders": [], "actions": [], "collectors": []},
        },
    )


if __name__ == "__main__":
    for name, check in [
        ("policies can be written, read and deleted", policy_crud),
        ("an index moves through its states", states_and_actions),
        ("a policy rolls an index over", rollover),
        ("a policy claims the indices it names", templates),
        ("a policy can be changed and removed", change_and_remove),
        ("a failed action is retried", retry_after_failure),
        ("the scheduler lists the jobs it holds", scheduled_job_surface),
        ("the plugin read surfaces answer", plugin_read_surface),
    ]:
        before = len(failures)
        check()
        print(f"  {'ok    ' if len(failures) == before else 'FAILED'} {name}")
    for line in failures:
        print("   ", line)
    sys.exit(1 if failures else 0)
