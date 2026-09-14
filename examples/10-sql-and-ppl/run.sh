#!/usr/bin/env bash
# The same questions, asked in SQL and in PPL, answered in five shapes.
source "$(dirname "$0")/lib.sh"
IDX=orders

sql()  { req POST "/_plugins/_sql${2:+?format=$2}" "{\"query\": $(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")}"; }
ppl()  { req POST "/_plugins/_ppl${2:+?format=$2}" "{\"query\": $(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")}"; }

step "orders, the shape a report is written against"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-orders-the-shape-a-report-is.json
green "$IDX"
python3 - <<'PY' > /tmp/orders.ndjson
import json, random, datetime
random.seed(11)
cust = ["acme", "northwind", "contoso", "initech", "umbrella"]
ctry = {"acme": "TH", "northwind": "TH", "contoso": "SG", "initech": "JP", "umbrella": "SG"}
chan = ["web", "app", "phone"]
stat = ["paid", "paid", "paid", "refunded", "cancelled"]
out = []
for i in range(400):
    c = random.choice(cust)
    d = datetime.date(2026, 1, 1) + datetime.timedelta(days=random.randint(0, 250))
    out.append(json.dumps({"index": {"_id": f"o{i}"}}))
    out.append(json.dumps({
        "order_id": f"o{i}", "customer": c, "country": ctry[c],
        "channel": random.choice(chan), "placed": d.isoformat(),
        "items": random.randint(1, 9), "total": round(random.uniform(15, 4000), 2),
        "status": random.choice(stat), "note": random.choice(
            ["delivered on time", "late, customer complained", "gift wrapped", "left with neighbour"])}))
print("\n".join(out))
PY
ndjson "/$IDX/_bulk?refresh=true" /tmp/orders.ndjson > /dev/null
expect_docs "$IDX" 400 "orders"
req GET "/_cat/count/$IDX?v"

step "SQL: the report nobody wants to write as a JSON aggregation"
sql "SELECT country, customer, COUNT(*) AS orders, ROUND(SUM(total), 2) AS revenue, ROUND(AVG(total), 2) AS mean
     FROM orders
     WHERE status = 'paid'
     GROUP BY country, customer
     HAVING COUNT(*) > 20
     ORDER BY revenue DESC
     LIMIT 10"

step "the same answer as a table a human reads"
sql "SELECT country, COUNT(*) AS orders, ROUND(SUM(total),2) AS revenue
     FROM orders WHERE status='paid' GROUP BY country ORDER BY revenue DESC" table

step "as CSV, for a spreadsheet"
sql "SELECT customer, COUNT(*) AS orders FROM orders GROUP BY customer ORDER BY orders DESC" csv

step "as jdbc, which carries the column types a driver needs"
sql "SELECT customer, total FROM orders LIMIT 3" jdbc

step "and raw, tab separated"
sql "SELECT customer, total FROM orders LIMIT 3" raw

step "SQL over text: the full-text operators are there too"
sql "SELECT order_id, note FROM orders WHERE MATCH(note, 'complained late') LIMIT 5"
sql "SELECT order_id, customer FROM orders WHERE customer IN ('acme','initech') AND total BETWEEN 1000 AND 2000 ORDER BY total DESC LIMIT 5"

step "dates, arithmetic and CASE"
sql "SELECT
       MONTH(placed) AS month,
       COUNT(*) AS orders,
       SUM(CASE WHEN status = 'refunded' THEN 1 ELSE 0 END) AS refunds,
       ROUND(SUM(total) / COUNT(*), 2) AS mean_order
     FROM orders
     GROUP BY MONTH(placed)
     ORDER BY month"

step "what the SQL was turned into"
req POST "/_plugins/_sql/_explain" '{ "query": "SELECT country, COUNT(*) FROM orders GROUP BY country" }'

step "PPL: the same question, written as a pipeline"
ppl "source=orders | where status = 'paid' | stats count() as orders, sum(total) as revenue by country | sort - revenue"

step "PPL's strength is the step-by-step shape"
ppl "source=orders
     | where total > 500
     | eval band = if(total > 2000, 'large', 'medium')
     | stats count() as n, avg(total) as mean by band, channel
     | sort band, - n
     | head 10" table

step "fields, rename, dedup, the small verbs"
ppl "source=orders | fields customer, country, total | rename total as amount | sort - amount | head 5" table
ppl "source=orders | dedup customer | fields customer, country" table

step "top and rare, which SQL needs a window function for"
ppl "source=orders | top 3 customer" table
ppl "source=orders | rare 2 channel" table

step "what the PPL was turned into"
req POST "/_plugins/_ppl/_explain" '{ "query": "source=orders | stats count() by country" }'

step "a query that will not parse, in each language"
sql "SELECT FROM WHERE" || true
ppl "source=orders | frobnicate" || true

step "what this example leaves behind, checked rather than assumed"
done_
