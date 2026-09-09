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
import http.client
import http.server
import json
import os
import pathlib
import socket
import time
import sys
import urllib.error
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
    # the child outlives the process that forked it, and whatever was reading
    # that process's output is still waiting on the pipe until every holder of
    # it lets go -- so this one does, before it starts serving
    devnull = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(devnull, fd)

    class Quiet(http.server.SimpleHTTPRequestHandler):
        """A fixture that narrates every read drowns the suite's own output."""

        def log_message(self, *_args):
            pass

    handler = functools.partial(Quiet, directory=str(SHARED))
    try:
        http.server.ThreadingHTTPServer(("127.0.0.1", PORT), handler).serve_forever()
    finally:
        os._exit(0)


def register(repositories):
    """Register them all, over one connection, insisting until they are there.

    This runs before every section of the suite, and it used to open a fresh
    connection for each repository: nine hundred sections is nearly three
    thousand connections, which on a busy machine is where the refusals come
    from. One connection does all three, kept alive.

    A registration that quietly failed is a section that fails for a reason
    nothing explains -- the repository is simply not there -- so it is tried
    again, and said out loud if it never lands.
    """
    host = NODE.split("://", 1)[-1]
    left = list(repositories)
    for attempt in range(20):
        conn = None
        try:
            conn = http.client.HTTPConnection(host, timeout=10)
            still = []
            for path, body in left:
                try:
                    conn.request(
                        "PUT",
                        path,
                        json.dumps(body),
                        {"content-type": "application/json"},
                    )
                    conn.getresponse().read()
                except Exception:
                    still.append((path, body))
                    raise
            left = still
        except Exception:
            time.sleep(min(0.2 * (attempt + 1), 2.0))
        finally:
            if conn is not None:
                conn.close()
        if not left:
            return
    for path, _ in left:
        print(f"  the fixture could not register {path}", file=sys.stderr)


def main():
    SHARED.mkdir(parents=True, exist_ok=True)
    if not serving():
        serve()
    register(
        [
            ("/_snapshot/repository-fs", {"type": "fs", "settings": {"location": str(SHARED)}}),
            (
                "/_snapshot/repository-url",
                {"type": "url", "settings": {"url": f"http://127.0.0.1:{PORT}/"}},
            ),
            (
                "/_snapshot/repository-file",
                {"type": "url", "settings": {"url": f"file://{SHARED}/"}},
            ),
        ]
    )


if __name__ == "__main__":
    main()
