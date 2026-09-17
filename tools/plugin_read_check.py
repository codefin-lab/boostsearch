#!/usr/bin/env python3
"""The analytics plugins' read surface, against a running node.

Machine learning, anomaly detection, forecasting, search relevance and
security analytics have no engine here. What they do have is the shape their
APIs answer with on a cluster where the plugin is installed and nothing has
used it, and that shape is what a console reads before it draws anything: an
empty registry, counters at zero, and a backing index reported as not there.
This holds the engine to it, so a route that stops answering -- or starts
answering something invented -- is caught.

The shapes were taken from OpenSearch 3.8.0. Run it against a node:

    ./target/release/velosearch
    VELO_URL=http://127.0.0.1:9200 python3 tools/plugin_read_check.py
"""
import json
import os
import sys
import urllib.error
import urllib.request

NODE = os.environ.get("VELO_URL", "http://127.0.0.1:9200")
DETECTORS = ".opendistro-anomaly-detectors"
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
            return response.status, json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as e:
        body = e.read() or b"{}"
        try:
            return e.code, json.loads(body)
        except ValueError:
            return e.code, {"body": body[:300].decode(errors="replace")}


def expect(what, got, want):
    if got != want:
        failures.append(f"{what}: {got!r}, expected {want!r}")


def answers(path, want, status=200):
    """The path answers, and every key asked about carries what it should."""
    got, body = req("GET", path)
    expect(f"GET {path}", got, status)
    for key, value in want.items():
        expect(f"GET {path} [{key}]", body.get(key), value)
    return body


def node_entry(body, path):
    """The one node's own figures out of a stats answer."""
    nodes = body.get("nodes")
    if not isinstance(nodes, dict) or len(nodes) != 1:
        failures.append(f"GET {path} [nodes]: {nodes!r}, expected one node's figures")
        return {}
    return next(iter(nodes.values()))


def machine_learning():
    for path in ("/_plugins/_ml/stats", "/_plugins/_ml/stats/"):
        body = answers(
            path,
            {
                "ml_model_count": 0,
                "ml_connector_count": 0,
                "ml_model_index_status": "non-existent",
                "ml_task_index_status": "non-existent",
                "ml_connector_index_status": "non-existent",
                "ml_config_index_status": "non-existent",
                "ml_controller_index_status": "non-existent",
            },
        )
        mine = node_entry(body, path)
        for counter in (
            "ml_request_count",
            "ml_failure_count",
            "ml_circuit_breaker_trigger_count",
            "ml_executing_task_count",
            "ml_deployed_model_count",
        ):
            expect(f"GET {path} [{counter}]", mine.get(counter), 0)
        expect(f"GET {path} [algorithms]", mine.get("algorithms"), {})
        expect(f"GET {path} [models]", mine.get("models"), {})
        expect(f"GET {path} [ml_jvm_heap_usage]", isinstance(mine.get("ml_jvm_heap_usage"), int), True)

    # a cluster figure asked for by name is answered on its own
    expect(
        "one ML stat",
        req("GET", "/_plugins/_ml/stats/ml_model_count")[1],
        {"ml_model_count": 0},
    )
    # and a node figure carries the breakdowns that travel with it
    mine = node_entry(
        answers("/_plugins/_ml/stats/ml_request_count", {}), "/_plugins/_ml/stats/ml_request_count"
    )
    expect("one ML node stat", mine, {"ml_request_count": 0, "algorithms": {}, "models": {}})
    # an unknown node in the path is ignored, as the plugin ignores it
    answers("/_plugins/_ml/no-such-node/stats", {"ml_model_count": 0})
    got, _ = req("GET", "/_plugins/_ml/stats/not_a_stat")
    expect("an ML stat that does not exist", got, 400)

    answers("/_plugins/_ml/memory", {"memories": []})
    answers("/_plugins/_ml/context_management", {"total": 0, "templates": []})
    for path in ("/_plugins/_ml/profile", "/_plugins/_ml/profile/models", "/_plugins/_ml/profile/tasks"):
        expect(f"GET {path}", req("GET", path)[1], {})
    # no tool implementation is shipped, and the list says so rather than
    # naming a tool that would never answer
    expect("the ML tool list", req("GET", "/_plugins/_ml/tools")[1], [])


def time_series():
    for path in ("/_plugins/_anomaly_detection/stats", "/_plugins/_anomaly_detection/stats/"):
        body = answers(
            path,
            {
                "detector_count": 0,
                "single_stream_detector_count": 0,
                "hc_detector_count": 0,
                "anomaly_detectors_index_status": "non-existent",
                "anomaly_results_index_status": "non-existent",
                "anomaly_detection_state_status": "non-existent",
                "models_checkpoint_index_status": "non-existent",
                "anomaly_detection_job_index_status": "non-existent",
                "forecast_config_index_status": "non-existent",
            },
        )
        mine = node_entry(body, path)
        expect(f"GET {path} [models]", mine.get("models"), [])
        for counter in (
            "model_count",
            "ad_execute_request_count",
            "ad_execute_failure_count",
            "ad_hc_execute_request_count",
            "ad_hc_execute_failure_count",
            "ad_executing_batch_task_count",
            "ad_total_batch_task_execution_count",
            "ad_batch_task_failure_count",
            "ad_canceled_batch_task_count",
            "ad_model_corruption_count",
        ):
            expect(f"GET {path} [{counter}]", mine.get(counter), 0)

    expect(
        "one detector stat",
        req("GET", "/_plugins/_anomaly_detection/stats/detector_count")[1].get("detector_count"),
        0,
    )
    got, _ = req("GET", "/_plugins/_anomaly_detection/stats/not_a_stat")
    expect("a detector stat that does not exist", got, 400)

    for path in ("/_plugins/_forecast/stats", "/_plugins/_forecast/stats/"):
        body = answers(
            path,
            {
                "forecaster_count": 0,
                "single_stream_forecaster_count": 0,
                "hc_forecaster_count": 0,
                "forecast_config_index_status": "non-existent",
                "forecast_results_index_status": "non-existent",
                "forecast_state_index_status": "non-existent",
                "forecast_models_checkpoint_index_status": "non-existent",
                "anomaly_detection_job_index_status": "non-existent",
            },
        )
        mine = node_entry(body, path)
        for counter in (
            "model_count",
            "forecast_execute_request_count",
            "forecast_execute_failure_count",
            "forecast_hc_execute_request_count",
            "forecast_hc_execute_failure_count",
            "forecast_model_corruption_count",
        ):
            expect(f"GET {path} [{counter}]", mine.get(counter), 0)
    got, _ = req("GET", "/_plugins/_forecast/stats/not_a_stat")
    expect("a forecast stat that does not exist", got, 400)

    for what in ("anomaly_detection/detectors", "forecast/forecasters"):
        answers(f"/_plugins/_{what}/count", {"count": 0, "match": False})
        answers(f"/_plugins/_{what}/match?name=nothing", {"count": 0, "match": False})


def counted_from_the_index():
    """What is counted is what is there, not a constant.

    The counts and the index status come out of the index the plugin keeps its
    detectors in, so a detector written there is counted and the status stops
    saying the index is not there.
    """
    req("DELETE", f"/{DETECTORS}")
    _, made = req(
        "PUT",
        f"/{DETECTORS}/_doc/one?refresh=true",
        {"name": "the-one", "category_field": ["host"]},
    )
    if made.get("error"):
        failures.append(f"writing a detector config: {made['error']}")
        return
    body = answers(
        "/_plugins/_anomaly_detection/stats",
        {"detector_count": 1, "hc_detector_count": 1, "single_stream_detector_count": 0},
    )
    status = body.get("anomaly_detectors_index_status")
    expect(
        "the detector index's status once it is there",
        status in ("green", "yellow", "red"),
        True,
    )
    answers("/_plugins/_anomaly_detection/detectors/count", {"count": 1, "match": False})
    answers("/_plugins/_anomaly_detection/detectors/match?name=the-one", {"count": 1, "match": True})
    answers("/_plugins/_anomaly_detection/detectors/match?name=other", {"count": 0, "match": False})
    req("DELETE", f"/{DETECTORS}")
    answers("/_plugins/_anomaly_detection/stats", {"detector_count": 0})


def relevance():
    empty = {
        "timed_out": False,
        "hits": {"total": {"value": 0, "relation": "eq"}, "max_score": None, "hits": []},
    }
    for what in (
        "query_sets",
        "search_configurations",
        "judgments",
        "experiments",
        "experiments/schedule",
    ):
        answers(f"/_plugins/_search_relevance/{what}", empty)

    for path in ("/_plugins/_search_relevance/stats", "/_plugins/_search_relevance/stats/"):
        body = answers(path, {"_nodes": {"total": 1, "successful": 1, "failed": 0}})
        # the cluster's total and the one node's own are the same figures here
        for counted in (body.get("all_nodes", {}), node_entry(body, path)):
            expect(f"GET {path} [judgments]", counted.get("judgments"), {
                "import_judgment_rating_generations": 0,
                "llm_judgment_rating_generations": 0,
                "ubi_judgment_rating_generations": 0,
            })
            expect(f"GET {path} [experiments]", counted.get("experiments"), {
                "experiment_executions": 0,
                "experiment_pairwise_comparison_executions": 0,
                "experiment_pointwise_evaluation_executions": 0,
                "experiment_hybrid_optimizer_executions": 0,
            })
    one = answers("/_plugins/_search_relevance/stats/experiment_executions", {})
    expect(
        "one relevance stat",
        one.get("all_nodes"),
        {"experiments": {"experiment_executions": 0}},
    )
    got, _ = req("GET", "/_plugins/_search_relevance/stats/not_a_stat")
    expect("a relevance stat that does not exist", got, 400)


def security_analytics():
    answers("/_plugins/_security_analytics/correlations", {"findings": []})
    answers(
        "/_plugins/_security_analytics/correlationAlerts",
        {"correlationAlerts": [], "total_alerts": 0},
    )
    # no rule set is installed, so no category is offered
    answers("/_plugins/_security_analytics/rules/categories", {"rule_categories": []})
    answers("/_plugins/_security_analytics/threat_intel/alerts", {"alerts": [], "total_alerts": 0})
    answers(
        "/_plugins/_security_analytics/threat_intel/findings/_search",
        {"total_findings": 0, "ioc_findings": []},
    )
    answers("/_plugins/_security_analytics/threat_intel/iocs", {"total": 0, "iocs": []})


if __name__ == "__main__":
    for name, check in [
        ("the machine-learning registry is empty and says so", machine_learning),
        ("no detector and no forecaster is configured", time_series),
        ("the counts come out of the index, not a constant", counted_from_the_index),
        ("relevance has no query set, judgment or experiment", relevance),
        ("security analytics has no finding, alert or indicator", security_analytics),
    ]:
        before = len(failures)
        check()
        print(f"  {'ok    ' if len(failures) == before else 'FAILED'} {name}")
    for line in failures:
        print("   ", line)
    sys.exit(1 if failures else 0)
