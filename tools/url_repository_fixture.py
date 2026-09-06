#!/usr/bin/env python3
"""The three repositories the URL suite is written against.

OpenSearch's build registers these before the suite runs and serves the
shared snapshot directory over HTTP from a URLFixture, and the suite's own
header says so. This is that fixture: a static server over the shared
directory, and the three repositories pointed at it -- one filesystem, one
reaching it over `http://`, one over `file://`.

Run it as `yaml_runner.py --before`, which calls it once per section; every
step here is idempotent, so a second call costs a few requests and changes
nothing.

Where a node may read a repository from is the node's own setting, not a
cluster one, so it cannot be arranged from here: start the node with

    BOOSTSEARCH_PATH_REPO=/tmp/boost-url-repo BOOSTSEARCH_URL_ALLOWED="http://snapshot.test*,http://127.0.0.1:9280*"

which is the same arrangement OpenSearch's build gives its own node: a
`file://` repository is allowed by sitting under the repository root, and
every other one by being named. `http://snapshot.test*` is in that list
because the suite registers a repository there to check that registering one
works, and never reads from it.
"""
import functools
import http.server
import json
import os
import pathlib
import socket
import time
import urllib.request

NODE = os.environ.get("BOOST_URL", "http://127.0.0.1:9213")
SHARED = pathlib.Path(os.environ.get("BOOST_URL_REPO", "/tmp/boost-url-repo"))
PORT = int(os.environ.get("BOOST_URL_FIXTURE_PORT", "9280"))


def serving():
    """Whether something already answers on the fixture's port."""
    with socket.socket() as s:
        s.settimeout(0.2)
        return s.connect_ex(("127.0.0.1", PORT)) == 0


def serve():
    """A read-only view of the shared directory, for the URL repository.

    This runs as `--before`, once per section, in a process that exits as
    soon as it is done -- so the server is handed to a child that outlives
    it rather than to a thread that would go down with it.
    """
    if os.fork() != 0:
        # the parent goes on to register the repositories against a server
        # the child is about to open; a moment for it to bind
        for _ in range(50):
            if serving():
                return
            time.sleep(0.02)
        return
    os.setsid()
    class Quiet(http.server.SimpleHTTPRequestHandler):
        """A fixture that narrates every read drowns the suite's own output."""

        def log_message(self, *_args):
            pass

    handler = functools.partial(Quiet, directory=str(SHARED))
    try:
        http.server.ThreadingHTTPServer(("127.0.0.1", PORT), handler).serve_forever()
    finally:
        os._exit(0)


def put(path, body):
    request = urllib.request.Request(
        NODE + path,
        method="PUT",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    try:
        urllib.request.urlopen(request).read()
    except Exception:
        pass


def main():
    SHARED.mkdir(parents=True, exist_ok=True)
    if not serving():
        serve()
    put("/_snapshot/repository-fs", {"type": "fs", "settings": {"location": str(SHARED)}})
    put(
        "/_snapshot/repository-url",
        {"type": "url", "settings": {"url": f"http://127.0.0.1:{PORT}/"}},
    )
    put("/_snapshot/repository-file", {"type": "url", "settings": {"url": f"file://{SHARED}/"}})


if __name__ == "__main__":
    main()
