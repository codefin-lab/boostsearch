# Performance against OpenSearch

Thirty-four dimensions, the same corpus, the same machine, both engines driven
by the same client (`tools/bench.py`). BoostSearch is ahead on all 34, on both
machines it has been measured on.

## On a cloud machine, both engines measured the same day

A Google Compute Engine `n2-standard-8`: eight vCPUs, 32 GB, Ubuntu 24.04, an
SSD. OpenSearch 3.1.0 from `opensearchproject/opensearch:3.1.0` with security
off, BoostSearch as this repository builds it. Each was measured on 2026-09-17
with nothing else running on the machine, five runs each, three query rounds
per run; the median is shown. The runs are kept in `bench/results/vm-bs-*.json`
and `bench/results/vm-os-3.1.0-*.json`.

| dimension | unit | OpenSearch 3.1.0 | BoostSearch | better by |
|---|---|---|---|---|
| queries a second, one client | q/s | 134.9 | 378.7 | +181% |
| memory, idle | MB | 1,501 | 37.7 | +97% |
| memory, after the search run | MB | 1,664 | 204.1 | +88% |
| memory, after indexing 200k | MB | 1,614 | 216.5 | +87% |
| agg nested p50 c1 | ms | 6.6 | 1.2 | +82% |
| agg date hist p50 c1 | ms | 6.5 | 1.2 | +81% |
| time range agg p50 c1 | ms | 6.4 | 1.4 | +79% |
| agg terms p50 c1 | ms | 6.0 | 1.4 | +77% |
| term numeric p50 c1 | ms | 7.5 | 1.9 | +75% |
| match all p50 c1 | ms | 6.6 | 1.8 | +73% |
| term keyword p50 c1 | ms | 6.5 | 1.8 | +72% |
| latency p50, one client | ms | 7.0 | 2.2 | +69% |
| match text p50 c1 | ms | 8.5 | 2.6 | +69% |
| time range p50 c1 | ms | 6.6 | 2.7 | +59% |
| sort paged p50 c1 | ms | 8.7 | 3.6 | +58% |
| queries a second, eight clients | q/s | 425.8 | 665.9 | +56% |
| range numeric p50 c1 | ms | 7.2 | 3.6 | +50% |
| latency p90, one client | ms | 9.1 | 4.7 | +48% |
| bool filter p50 c1 | ms | 9.1 | 4.8 | +46% |
| agg date hist p50 c8 | ms | 16.3 | 8.8 | +46% |
| term numeric p50 c8 | ms | 16.6 | 9.1 | +45% |
| match text p50 c8 | ms | 18.5 | 10.5 | +43% |
| time range agg p50 c8 | ms | 14.1 | 8.1 | +42% |
| term keyword p50 c8 | ms | 15.8 | 9.2 | +41% |
| match all p50 c8 | ms | 14.8 | 8.8 | +40% |
| agg terms p50 c8 | ms | 14.7 | 9.2 | +37% |
| latency p50, eight clients | ms | 16.4 | 10.3 | +37% |
| agg nested p50 c8 | ms | 15.3 | 9.6 | +37% |
| time range p50 c8 | ms | 16.1 | 10.1 | +37% |
| range numeric p50 c8 | ms | 16.9 | 11.4 | +33% |
| latency p90, eight clients | ms | 23.9 | 16.4 | +31% |
| sort paged p50 c8 | ms | 17.6 | 12.6 | +29% |
| bool filter p50 c8 | ms | 18.6 | 14.5 | +22% |
| indexing throughput | docs/s | 21,725 | 24,921 | +15% |

Read the ratios rather than the absolute numbers: this machine is a third the
speed of the laptop below, for both engines. The memory figures are the one
server process's resident set, the container's own figure for OpenSearch.

## On a laptop, OpenSearch measured once and kept

### How these numbers were taken

- **OpenSearch 3.1.0** was measured once, five runs, on 2026-08-27, from the
  official image with security off; the runs are kept in
  `bench/results/final-os-clean-*.json` and the median of the five is shown.
- **BoostSearch** is this repository's current build, three runs taken
  2026-09-10 on Apple M4 Max (14 cores); the median is
  shown and kept in `tools/bench_baseline.json`, together with how far the three
  runs spread, dimension by dimension.
- The corpus is 200,000 web-log documents (`bench/data/http_logs.ndjson`); the
  queries are twelve shapes, each measured with one client and with eight.
- Memory is the resident set of the one server process: `pid:<n>` for
  BoostSearch, the container's own figure for OpenSearch.

OpenSearch is not measured again on every change. What every change is held to
is this repository's own last numbers (`tools/bench_gate.py`, ADR 0004), and
this table is what those numbers say beside the kept OpenSearch measurement.
Measuring OpenSearch again is a thing to do when the version it is compared to
changes.

### The table

| dimension | unit | OpenSearch 3.1.0 | BoostSearch | better by |
|---|---|---|---|---|
| queries a second, one client | q/s | 380.9 | 1,414.8 | +271% |
| memory, idle | MB | 1,095.2 | 19.0 | +98% |
| time range agg p50 c1 | ms | 2.4 | 0.4 | +82% |
| agg terms p50 c1 | ms | 2.3 | 0.5 | +81% |
| agg date hist p50 c1 | ms | 2.3 | 0.4 | +81% |
| agg nested p50 c1 | ms | 2.4 | 0.5 | +81% |
| latency p50, one client | ms | 2.5 | 0.6 | +78% |
| term numeric p50 c1 | ms | 2.3 | 0.5 | +78% |
| term keyword p50 c1 | ms | 2.3 | 0.5 | +78% |
| match all p50 c1 | ms | 2.1 | 0.5 | +77% |
| memory, after indexing 200k | MB | 1,085.6 | 261.3 | +76% |
| memory, after the search run | MB | 1,112.4 | 269.2 | +76% |
| match text p50 c1 | ms | 2.7 | 0.8 | +71% |
| sort paged p50 c1 | ms | 3.1 | 0.9 | +71% |
| time range p50 c1 | ms | 2.5 | 0.8 | +70% |
| latency p90, one client | ms | 3.2 | 1.1 | +68% |
| bool filter p50 c1 | ms | 3.0 | 1.1 | +63% |
| range numeric p50 c1 | ms | 2.4 | 0.9 | +63% |
| indexing throughput | docs/s | 72,704.0 | 106,447.0 | +46% |
| queries a second, eight clients | q/s | 1,621.9 | 2,307.6 | +42% |
| agg nested p50 c8 | ms | 4.9 | 2.9 | +41% |
| agg date hist p50 c8 | ms | 4.8 | 3.0 | +37% |
| term numeric p50 c8 | ms | 4.6 | 2.9 | +36% |
| time range agg p50 c8 | ms | 4.5 | 2.9 | +36% |
| match all p50 c8 | ms | 4.6 | 3.0 | +35% |
| term keyword p50 c8 | ms | 4.5 | 3.0 | +34% |
| match text p50 c8 | ms | 4.9 | 3.2 | +33% |
| agg terms p50 c8 | ms | 4.4 | 3.0 | +33% |
| latency p50, eight clients | ms | 4.6 | 3.2 | +31% |
| latency p90, eight clients | ms | 5.9 | 4.3 | +28% |
| range numeric p50 c8 | ms | 4.6 | 3.4 | +27% |
| time range p50 c8 | ms | 4.6 | 3.4 | +26% |
| sort paged p50 c8 | ms | 5.0 | 3.7 | +25% |
| bool filter p50 c8 | ms | 5.0 | 3.8 | +25% |

Lower is better for latency and memory, higher for throughput; "better by" is
the margin in BoostSearch's favour either way. The latency rows named
`<query> p50 c1` and `c8` are that query shape's median with one client and
with eight.

## What to read into it

- The client is Python, and its own ceiling is lower than either engine's:
  the queries-a-second rows are what this client reached, not what either
  engine can do. The latencies hold up; for throughput use a native load
  generator (`k6 run tools/load.js`).
- These are single-node numbers. A cluster adds replication to every write and
  a network hop to some reads; they have not been measured against an
  OpenSearch cluster.
- The closest margins are the eight-client latencies, around +23% to +30%.
  That is where a change is most likely to cost the lead, and where the gate's
  5% matters most.
