#!/usr/bin/env python3
"""Generate a deterministic http-log-shaped dataset as NDJSON.

Deterministic so the same corpus can be replayed against any engine; the shape
mirrors the `http_logs` workload OpenSearch Benchmark uses.
"""
import argparse, json, random, pathlib, datetime

STATUS = [200] * 80 + [301, 302, 304, 400, 401, 403] + [404] * 8 + [500] * 4
METHODS = ["GET"] * 85 + ["POST"] * 10 + ["PUT", "DELETE", "HEAD"] * 2
PATHS = [
    "/", "/index.html", "/images/logo.png", "/api/v1/search", "/api/v1/users",
    "/static/app.js", "/static/app.css", "/health", "/metrics", "/favicon.ico",
    "/api/v1/orders", "/api/v1/orders/{id}", "/downloads/report.pdf", "/login",
    "/logout", "/admin/dashboard", "/blog/how-to-scale-search",
]
AGENTS = [
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/120 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko/20100101 Firefox/121.0",
    "curl/8.4.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) Mobile/15E148 Safari/604.1",
    "Googlebot/2.1 (+http://www.google.com/bot.html)",
    "python-requests/2.31.0",
]
REGIONS = ["us-east-1", "us-west-2", "eu-west-1", "ap-southeast-1", "ap-northeast-1"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--docs", type=int, default=200_000)
    ap.add_argument("--out", default="bench/data/http_logs.ndjson")
    ap.add_argument("--seed", type=int, default=20260826)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    start = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)

    with out.open("w") as f:
        for i in range(args.docs):
            ts = start + datetime.timedelta(seconds=i * 3)
            path = rng.choice(PATHS).replace("{id}", str(rng.randint(1, 99999)))
            doc = {
                "@timestamp": ts.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "clientip": f"{rng.randint(1,223)}.{rng.randint(0,255)}."
                            f"{rng.randint(0,255)}.{rng.randint(1,254)}",
                "method": rng.choice(METHODS),
                "request": path,
                "status": rng.choice(STATUS),
                "size": max(0, int(rng.lognormvariate(7.5, 1.6))),
                "response_ms": round(max(0.5, rng.lognormvariate(3.0, 1.0)), 2),
                "agent": rng.choice(AGENTS),
                "region": rng.choice(REGIONS),
                "referer": rng.choice(["-", "https://example.com/", "https://news.example/"]),
            }
            f.write(json.dumps(doc, separators=(",", ":")) + "\n")

    size = out.stat().st_size
    print(f"wrote {args.docs} docs to {out} ({size/1e6:.1f} MB)")


main()
