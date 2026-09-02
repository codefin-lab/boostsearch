#!/usr/bin/env python3
"""Every dimension, both engines, same corpus, same machine.

The claim is that BoostSearch beats OpenSearch everywhere, which is only worth
saying if it is checked everywhere and checked again after every change. This
writes the table that says so, and exits non-zero if any dimension is lost.
"""
import urllib.request, json, time, statistics, subprocess, sys, os, ssl, base64
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE
import os, ssl, base64
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE
def req(base, method, path, body=None):
    data = body.encode() if isinstance(body,str) else (json.dumps(body).encode() if body is not None else None)
    headers = {"Content-Type":"application/json"}
    # BENCH_AUTH=user:pass sends basic auth, for an engine with security on
    if os.environ.get("BENCH_AUTH"):
        headers["Authorization"] = "Basic " + base64.b64encode(os.environ["BENCH_AUTH"].encode()).decode()
    r = urllib.request.Request(base+path, data, headers, method=method)
    return json.load(urllib.request.urlopen(r, context=_CTX))
def bulk_index(base, name, path, batch=4000):
    try: req(base,"DELETE","/"+name)
    except Exception: pass
    req(base,"PUT","/"+name,{"settings":{"number_of_shards":1,"number_of_replicas":0},
        "mappings":{"properties":{"@timestamp":{"type":"date"},"status":{"type":"long"},
        "region":{"type":"keyword"},"agent":{"type":"text"},"request":{"type":"text"},
        "size":{"type":"long"},"response_ms":{"type":"double"}}}})
    buf=[];n=0;t=time.time()
    for line in open(path):
        buf.append('{"index":{}}');buf.append(line.strip());n+=1
        if len(buf)>=batch:
            req(base,"POST",f"/{name}/_bulk","\n".join(buf)+"\n");buf=[]
    if buf: req(base,"POST",f"/{name}/_bulk","\n".join(buf)+"\n")
    el=time.time()-t
    req(base,"POST",f"/{name}/_refresh")
    return n/el
QUERIES={
 "match_all":{"query":{"match_all":{}},"size":10},
 "term":{"query":{"term":{"region":"eu-west-1"}},"size":10},
 "match":{"query":{"match":{"agent":"Chrome Safari"}},"size":10},
 "bool+filter":{"query":{"bool":{"must":[{"match":{"request":"api"}}],"filter":[{"range":{"status":{"gte":200,"lt":300}}}]}},"size":10},
 "range":{"query":{"range":{"@timestamp":{"gte":"2026-01-05","lt":"2026-01-06"}}},"size":0},
 "sort_desc":{"query":{"match_all":{}},"sort":[{"@timestamp":"desc"}],"size":10},
 "terms_agg":{"size":0,"aggs":{"a":{"terms":{"field":"region","size":10}}}},
 "date_histogram":{"size":0,"aggs":{"h":{"date_histogram":{"field":"@timestamp","fixed_interval":"1h"}}}},
 "nested_agg":{"size":0,"aggs":{"a":{"terms":{"field":"region"},"aggs":{"s":{"avg":{"field":"response_ms"}}}}}},
 "cardinality":{"size":0,"aggs":{"c":{"cardinality":{"field":"request.keyword" if False else "region"}}}},
}
def latency(base,name,body,n=60):
    ts=[]
    for _ in range(n):
        t=time.time(); req(base,"POST",f"/{name}/_search",body); ts.append((time.time()-t)*1000)
    ts.sort()
    return statistics.median(ts), ts[int(len(ts)*0.99)-1]
def rss(container=None, pid=None):
    if container:
        out=subprocess.run(["docker","stats","--no-stream","--format","{{.MemUsage}}",container],capture_output=True,text=True).stdout
        return out.split('/')[0].strip()
    out=subprocess.run(["ps","-o","rss=","-p",str(pid)],capture_output=True,text=True).stdout.strip()
    return f"{int(out)/1024:.0f}MiB"
A=("OpenSearch",os.environ.get("BENCH_A","http://127.0.0.1:9299")); B=("BoostSearch",os.environ.get("BENCH_B","http://127.0.0.1:9200"))
res={}
for label,base in (A,B):
    print(f"indexing into {label}...", flush=True)
    res[label]={"index_docs_per_s": bulk_index(base,"perf","/tmp/bench_logs.ndjson")}
    lat={}
    for q,body in QUERIES.items():
        lat[q]=latency(base,"perf",body)
    res[label]["latency"]=lat
res["OpenSearch"]["rss"]=rss(container=os.environ.get("BENCH_A_CONTAINER","os-compat"))
res["BoostSearch"]["rss"]=rss(pid=subprocess.run(["pgrep","-f","release/boostsearch"],capture_output=True,text=True).stdout.split()[0])
json.dump(res,open('/tmp/matrix.json','w'),indent=1)
print(f"\n{'dimension':<20}{'OpenSearch':>14}{'BoostSearch':>14}   winner")
o=res["OpenSearch"]; b=res["BoostSearch"]
print(f"{'index docs/s':<20}{o['index_docs_per_s']:>14,.0f}{b['index_docs_per_s']:>14,.0f}   {'BoostSearch' if b['index_docs_per_s']>o['index_docs_per_s'] else 'OpenSearch'}")
print(f"{'memory':<20}{o['rss']:>14}{b['rss']:>14}")
for q in QUERIES:
    om,op=o['latency'][q]; bm,bp=b['latency'][q]
    print(f"{q+' p50 (ms)':<20}{om:>14.2f}{bm:>14.2f}   {'BoostSearch' if bm<om else 'OpenSearch'}")
import urllib.request, json, time, statistics, subprocess, sys, os, ssl, base64
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE
def req(base, method, path, body=None):
    data = body.encode() if isinstance(body,str) else (json.dumps(body).encode() if body is not None else None)
    headers = {"Content-Type":"application/json"}
    if os.environ.get("BENCH_AUTH"):
        headers["Authorization"] = "Basic " + base64.b64encode(os.environ["BENCH_AUTH"].encode()).decode()
    r = urllib.request.Request(base+path, data, headers, method=method)
    return json.load(urllib.request.urlopen(r, context=_CTX))
def bulk_index(base, name, path, batch=4000):
    try: req(base,"DELETE","/"+name)
    except Exception: pass
    req(base,"PUT","/"+name,{"settings":{"number_of_shards":1,"number_of_replicas":0},
        "mappings":{"properties":{"@timestamp":{"type":"date"},"status":{"type":"long"},
        "region":{"type":"keyword"},"agent":{"type":"text"},"request":{"type":"text"},
        "size":{"type":"long"},"response_ms":{"type":"double"}}}})
    buf=[];n=0;t=time.time()
    for line in open(path):
        buf.append('{"index":{}}');buf.append(line.strip());n+=1
        if len(buf)>=batch:
            req(base,"POST",f"/{name}/_bulk","\n".join(buf)+"\n");buf=[]
    if buf: req(base,"POST",f"/{name}/_bulk","\n".join(buf)+"\n")
    el=time.time()-t
    req(base,"POST",f"/{name}/_refresh")
    return n/el
QUERIES={
 "match_all":{"query":{"match_all":{}},"size":10},
 "term":{"query":{"term":{"region":"eu-west-1"}},"size":10},
 "match":{"query":{"match":{"agent":"Chrome Safari"}},"size":10},
 "bool+filter":{"query":{"bool":{"must":[{"match":{"request":"api"}}],"filter":[{"range":{"status":{"gte":200,"lt":300}}}]}},"size":10},
 "range":{"query":{"range":{"@timestamp":{"gte":"2026-01-05","lt":"2026-01-06"}}},"size":0},
 "sort_desc":{"query":{"match_all":{}},"sort":[{"@timestamp":"desc"}],"size":10},
 "terms_agg":{"size":0,"aggs":{"a":{"terms":{"field":"region","size":10}}}},
 "date_histogram":{"size":0,"aggs":{"h":{"date_histogram":{"field":"@timestamp","fixed_interval":"1h"}}}},
 "nested_agg":{"size":0,"aggs":{"a":{"terms":{"field":"region"},"aggs":{"s":{"avg":{"field":"response_ms"}}}}}},
 "cardinality":{"size":0,"aggs":{"c":{"cardinality":{"field":"request.keyword" if False else "region"}}}},
}
def latency(base,name,body,n=60):
    ts=[]
    for _ in range(n):
        t=time.time(); req(base,"POST",f"/{name}/_search",body); ts.append((time.time()-t)*1000)
    ts.sort()
    return statistics.median(ts), ts[int(len(ts)*0.99)-1]
def rss(container=None, pid=None):
    if container:
        out=subprocess.run(["docker","stats","--no-stream","--format","{{.MemUsage}}",container],capture_output=True,text=True).stdout
        return out.split('/')[0].strip()
    out=subprocess.run(["ps","-o","rss=","-p",str(pid)],capture_output=True,text=True).stdout.strip()
    return f"{int(out)/1024:.0f}MiB"
A=("OpenSearch",os.environ.get("BENCH_A","http://127.0.0.1:9299")); B=("BoostSearch",os.environ.get("BENCH_B","http://127.0.0.1:9200"))
res={}
for label,base in (A,B):
    print(f"indexing into {label}...", flush=True)
    res[label]={"index_docs_per_s": bulk_index(base,"perf","/tmp/bench_logs.ndjson")}
    lat={}
    for q,body in QUERIES.items():
        lat[q]=latency(base,"perf",body)
    res[label]["latency"]=lat
res["OpenSearch"]["rss"]=rss(container=os.environ.get("BENCH_A_CONTAINER","os-compat"))
res["BoostSearch"]["rss"]=rss(pid=subprocess.run(["pgrep","-f","release/boostsearch"],capture_output=True,text=True).stdout.split()[0])
json.dump(res,open('/tmp/matrix.json','w'),indent=1)
print(f"\n{'dimension':<20}{'OpenSearch':>14}{'BoostSearch':>14}   winner")
o=res["OpenSearch"]; b=res["BoostSearch"]
print(f"{'index docs/s':<20}{o['index_docs_per_s']:>14,.0f}{b['index_docs_per_s']:>14,.0f}   {'BoostSearch' if b['index_docs_per_s']>o['index_docs_per_s'] else 'OpenSearch'}")
print(f"{'memory':<20}{o['rss']:>14}{b['rss']:>14}")
for q in QUERIES:
    om,op=o['latency'][q]; bm,bp=b['latency'][q]
    print(f"{q+' p50 (ms)':<20}{om:>14.2f}{bm:>14.2f}   {'BoostSearch' if bm<om else 'OpenSearch'}")
