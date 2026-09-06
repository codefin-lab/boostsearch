#!/usr/bin/env python3
"""SQL and PPL, end to end.

OpenSearch keeps both languages in a plugin with its own repository and its
own suite, which is not part of the corpus this repository runs. This is what
stands in for it: every check asks a question in one of the two languages and
says what the answer must be, so a wrong answer is a failure rather than
something to read past.
"""
import json
import os
import sys
import urllib.error
import urllib.request

NODE = os.environ.get("BOOST_URL", "http://127.0.0.1:9213")
INDEX = "sqlcheck"
failures = []

DOCUMENTS = [
    {"region": "north", "product": "widget", "price": 9.5, "units": 3, "note": "a fine widget"},
    {"region": "north", "product": "gadget", "price": 21.0, "units": 1, "note": "a heavy gadget"},
    {"region": "south", "product": "widget", "price": 8.0, "units": 7, "note": "a cheap widget"},
    {"region": "south", "product": "gizmo", "price": 33.25, "units": 2, "note": "a rare gizmo"},
    {"region": "east", "product": "widget", "price": 11.0, "units": 4, "note": "a widget again"},
]


def call(path, body, raw=False):
    request = urllib.request.Request(
        NODE + path,
        method="POST",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            text = response.read()
            return text.decode() if raw else json.loads(text or b"{}")
    except urllib.error.HTTPError as e:
        # an error is an answer too, and its body is the part worth reading
        text = e.read()
        if raw:
            return text.decode()
        try:
            return json.loads(text or b"{}")
        except ValueError:
            return {"error": {"type": f"HTTP {e.code}", "details": text[:400].decode()}}


def sql(query, fmt=None):
    return call(f"/_plugins/_sql{'?format=' + fmt if fmt else ''}", {"query": query}, raw=bool(fmt))


def ppl(query, fmt=None):
    return call(f"/_plugins/_ppl{'?format=' + fmt if fmt else ''}", {"query": query}, raw=bool(fmt))


def expect(what, got, want):
    if got != want:
        failures.append(f"{what}\n      got  {got!r}\n      want {want!r}")


def rows(answer):
    return answer.get("datarows")


def setup():
    plain = urllib.request.Request(NODE + f"/{INDEX}", method="DELETE")
    try:
        urllib.request.urlopen(plain)
    except Exception:
        pass
    made = urllib.request.Request(
        NODE + f"/{INDEX}",
        method="PUT",
        data=json.dumps(
            {
                "mappings": {
                    "properties": {
                        "region": {"type": "keyword"},
                        "product": {"type": "keyword"},
                        "price": {"type": "double"},
                        "units": {"type": "long"},
                        "note": {"type": "text"},
                    }
                }
            }
        ).encode(),
        headers={"content-type": "application/json"},
    )
    urllib.request.urlopen(made)
    for at, document in enumerate(DOCUMENTS):
        urllib.request.urlopen(
            urllib.request.Request(
                NODE + f"/{INDEX}/_doc/{at}?refresh=true",
                method="POST",
                data=json.dumps(document).encode(),
                headers={"content-type": "application/json"},
            )
        )


def selecting():
    expect(
        "a filter and a sort",
        rows(sql(f"SELECT region, price FROM {INDEX} WHERE price > 10 ORDER BY price DESC")),
        [["south", 33.25], ["north", 21.0], ["east", 11.0]],
    )
    expect(
        "a limit and an offset",
        rows(sql(f"SELECT price FROM {INDEX} ORDER BY price ASC LIMIT 2 OFFSET 1")),
        [[9.5], [11.0]],
    )
    expect(
        "BETWEEN and IN",
        rows(
            sql(
                f"SELECT product FROM {INDEX} WHERE price BETWEEN 9 AND 22 "
                "AND region IN ('north', 'east') ORDER BY price"
            )
        ),
        [["widget"], ["widget"], ["gadget"]],
    )
    expect(
        "LIKE speaks SQL's wildcards",
        rows(sql(f"SELECT DISTINCT product FROM {INDEX} WHERE product LIKE 'wid%'")),
        [["widget"]],
    )
    expect(
        "IS NOT NULL",
        len(rows(sql(f"SELECT region FROM {INDEX} WHERE price IS NOT NULL"))),
        5,
    )


def grouping():
    expect(
        "count and average by group",
        rows(
            sql(
                f"SELECT region, count(*) AS n, avg(price) AS mean FROM {INDEX} "
                "GROUP BY region ORDER BY region"
            )
        ),
        [["east", 1, 11.0], ["north", 2, 15.25], ["south", 2, 20.625]],
    )
    expect(
        "aggregates with no group at all",
        rows(sql(f"SELECT count(*), max(price), min(units) FROM {INDEX}")),
        [[5, 33.25, 1.0]],
    )
    expect(
        "HAVING, named as the aggregate",
        rows(
            sql(
                f"SELECT region, count(*) AS n FROM {INDEX} GROUP BY region "
                "HAVING count(*) > 1 ORDER BY region"
            )
        ),
        [["north", 2], ["south", 2]],
    )
    expect(
        "HAVING, named as the alias",
        rows(
            sql(
                f"SELECT region, count(*) AS n FROM {INDEX} GROUP BY region "
                "HAVING n > 1 ORDER BY region"
            )
        ),
        [["north", 2], ["south", 2]],
    )
    expect(
        "ordering by an aggregate",
        rows(
            sql(f"SELECT region, avg(price) AS m FROM {INDEX} GROUP BY region ORDER BY m DESC")
        ),
        [["south", 20.625], ["north", 15.25], ["east", 11.0]],
    )
    expect(
        "counting what is different",
        rows(sql(f"SELECT count(DISTINCT region) FROM {INDEX}")),
        [[3]],
    )
    expect(
        "two grouping keys",
        len(rows(sql(f"SELECT region, product, count(*) FROM {INDEX} GROUP BY region, product"))),
        5,
    )


def full_text():
    expect(
        "MATCH is a search, not a comparison",
        rows(sql(f"SELECT region FROM {INDEX} WHERE MATCH(note, 'cheap')")),
        [["south"]],
    )
    expect(
        "and it finds every document the words are in",
        len(rows(sql(f"SELECT region FROM {INDEX} WHERE MATCH(note, 'widget')"))),
        3,
    )


def expressions():
    expect(
        "arithmetic over a row",
        rows(
            sql(
                f"SELECT region, price * units AS total FROM {INDEX} "
                "WHERE region = 'south' ORDER BY total"
            )
        ),
        [["south", 56], ["south", 66.5]],
    )
    expect(
        "the scalar functions",
        rows(sql(f"SELECT upper(region), round(price), length(product) FROM {INDEX} LIMIT 1")),
        [["NORTH", 10, 6]],
    )


def piped():
    expect(
        "a pipeline narrows and picks",
        rows(ppl(f"source={INDEX} | where price > 10 | fields region, price | sort + price")),
        [["east", 11.0], ["north", 21.0], ["south", 33.25]],
    )
    expect(
        "stats by",
        rows(ppl(f"source={INDEX} | stats count() by region | sort + region")),
        [[1, "east"], [2, "north"], [2, "south"]],
    )
    expect(
        "eval then fields",
        rows(
            ppl(
                f"source={INDEX} | where region = 'south' | eval total = price * units "
                "| fields region, total | sort + total"
            )
        ),
        [["south", 56], ["south", 66.5]],
    )
    expect("head is a limit", len(rows(ppl(f"source={INDEX} | head 2"))), 2)
    expect(
        "two wheres are both applied",
        rows(ppl(f"source={INDEX} | where price > 10 | where region = 'north' | fields product")),
        [["gadget"]],
    )


def formats():
    csv = sql(f"SELECT region, units FROM {INDEX} ORDER BY units DESC LIMIT 2", "csv")
    expect("csv", csv.strip().splitlines(), ["region,units", "south,7", "east,4"])
    drawn = sql(f"SELECT region FROM {INDEX} LIMIT 1", "table")
    expect("a drawn table has its borders", drawn.count("+--------+"), 3)
    plain = sql(f"SELECT count(*) FROM {INDEX}", "json")
    expect("json omits the status", "status" in plain, False)


def mistakes():
    expect(
        "a syntax error is a syntax error",
        sql("SELECT FROM").get("error", {}).get("type"),
        "SyntaxAnalysisException",
    )
    expect(
        "an ungrouped column is refused",
        sql(f"SELECT product, count(*) FROM {INDEX} GROUP BY region").get("error", {}).get("type"),
        "SemanticAnalysisException",
    )
    expect(
        "an index that is not there is not there",
        sql("SELECT * FROM nowhere").get("error", {}).get("type"),
        "IndexNotFoundException",
    )


def explaining():
    found = call("/_plugins/_sql/_explain", {"query": f"SELECT count(*) FROM {INDEX}"})
    request = found.get("root", {}).get("description", {}).get("request", "")
    expect("explain says which index", INDEX in request, True)
    expect("and shows the search itself", "aggregations" in request or "size" in request, True)


if __name__ == "__main__":
    setup()
    for name, check in [
        ("selecting, filtering, sorting", selecting),
        ("grouping and aggregating", grouping),
        ("full text from SQL", full_text),
        ("expressions and functions", expressions),
        ("the pipeline language", piped),
        ("the response formats", formats),
        ("mistakes are named", mistakes),
        ("explain shows the search", explaining),
    ]:
        before = len(failures)
        check()
        print(f"  {'ok    ' if len(failures) == before else 'FAILED'} {name}")
    for line in failures:
        print("   ", line)
    sys.exit(1 if failures else 0)
