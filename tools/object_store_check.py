#!/usr/bin/env python3
"""Snapshot to an object store, and read it back from another node's view.

OpenSearch's own S3, GCS and Azure suites are written against cloud accounts
and are not in the corpus this repository runs. These three run against the
emulators the vendors publish -- minio, Azurite and fake-gcs-server -- which
speak the same protocols, so what is checked here is what a real bucket would
see: the request signing, the layout written, and a restore that reads it back
without the node that wrote it.

    docker run -d --name bs-minio -p 9401:9000 \\
        -e MINIO_ROOT_USER=boostkey -e MINIO_ROOT_PASSWORD=boostsecret123 \\
        minio/minio:latest server /data
    docker run -d --name bs-azurite -p 9402:10000 \\
        mcr.microsoft.com/azure-storage/azurite:latest azurite-blob --blobHost 0.0.0.0
    docker run -d --name bs-gcs -p 9403:4443 \\
        fsouza/fake-gcs-server:latest -scheme http -public-host 127.0.0.1:9403 -backend memory

Then create a bucket in each -- `tools/object_store_setup.sh` does that -- and
run this against a node.
"""
import json
import os
import sys
import urllib.error
import urllib.request

NODE = os.environ.get("BOOST_URL", "http://127.0.0.1:9213")
AZURE_KEY = (
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
)

REPOSITORIES = {
    "s3": {
        "type": "s3",
        "settings": {
            "bucket": "snapshots",
            "endpoint": os.environ.get("BOOST_S3", "http://127.0.0.1:9401"),
            "access_key": "boostkey",
            "secret_key": "boostsecret123",
            "path_style_access": True,
            "base_path": "boost",
        },
    },
    "azure": {
        "type": "azure",
        "settings": {
            "container": "snapshots",
            "account": "devstoreaccount1",
            "key": AZURE_KEY,
            "endpoint": os.environ.get(
                "BOOST_AZURE", "http://127.0.0.1:9402/devstoreaccount1"
            ),
            "base_path": "boost",
        },
    },
    "gcs": {
        "type": "gcs",
        "settings": {
            "bucket": "snapshots",
            "endpoint": os.environ.get("BOOST_GCS", "http://127.0.0.1:9403"),
            "base_path": "boost",
        },
    },
}


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


def check(kind, spec):
    """One store, all the way round: write it, forget it, read it back."""
    index, repo, snapshot = f"objects-{kind}", f"repo-{kind}", f"snap-{kind}"
    failures = []

    def expect(what, got, want):
        if got != want:
            failures.append(f"{kind}: {what} is {got!r}, expected {want!r}")

    req("DELETE", f"/{index}")
    made = req("PUT", f"/_snapshot/{repo}", spec)
    expect("registering the repository", made.get("acknowledged"), True)
    req("PUT", f"/{index}", {"mappings": {"properties": {"n": {"type": "long"}}}})
    for i in range(5):
        req("POST", f"/{index}/_doc/{i}?refresh=true", {"n": i, "t": f"{kind} {i}"})

    taken = req("PUT", f"/_snapshot/{repo}/{snapshot}?wait_for_completion=true")
    expect("taking the snapshot", taken.get("snapshot", {}).get("state"), "SUCCESS")

    # what a node that did not write it sees: the repository is registered
    # again from nothing, and what it holds comes out of the store itself
    req("DELETE", f"/_snapshot/{repo}")
    req("PUT", f"/_snapshot/{repo}", spec)
    held = [s["snapshot"] for s in req("GET", f"/_snapshot/{repo}/_all").get("snapshots", [])]
    expect("what a fresh registration finds", held, [snapshot])

    req("DELETE", f"/{index}")
    back = req("POST", f"/_snapshot/{repo}/{snapshot}/_restore?wait_for_completion=true")
    expect(
        "restoring", back.get("snapshot", {}).get("shards", {}).get("successful"), 1
    )
    req("POST", f"/{index}/_refresh")
    expect("documents restored", req("GET", f"/{index}/_count").get("count"), 5)
    expect(
        "a document's content",
        req("GET", f"/{index}/_doc/3").get("_source"),
        {"n": 3, "t": f"{kind} 3"},
    )

    gone = req("DELETE", f"/_snapshot/{repo}/{snapshot}")
    expect("deleting the snapshot", gone.get("acknowledged"), True)
    left = [s["snapshot"] for s in req("GET", f"/_snapshot/{repo}/_all").get("snapshots", [])]
    expect("what is left", left, [])

    req("DELETE", f"/{index}")
    req("DELETE", f"/_snapshot/{repo}")
    return failures


if __name__ == "__main__":
    wanted = sys.argv[1:] or list(REPOSITORIES)
    everything = []
    for kind in wanted:
        found = check(kind, REPOSITORIES[kind])
        print(f"  {kind:6} {'ok' if not found else 'FAILED'}")
        everything.extend(found)
    for line in everything:
        print("   ", line)
    print(f"\n{len(wanted) - len(set(f.split(':')[0] for f in everything))} of {len(wanted)} stores round-tripped")
    sys.exit(1 if everything else 0)
