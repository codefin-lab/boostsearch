#!/usr/bin/env python3
"""What the console's front end boots from, taken from a Dashboards that works.

The React application and the server it talks to are two halves of one
program, and the contract between them -- the `osd-injected-metadata` element
and the bundle list in `bootstrap.js` -- is not written down anywhere. It is
whatever the Node server happens to emit, and it changes when Dashboards
changes.

So it is pinned rather than guessed: this asks a running OpenSearch Dashboards
for the page it serves, reads the contract out of it, and writes it beside the
version it came from. Moving to a newer Dashboards is running this again and
looking at the diff -- which is the point. A contract nobody wrote down should
at least be one somebody has to change on purpose.

    tools/dashboards_reference.sh
    tools/osd_pin.py --url http://127.0.0.1:5613

What is *not* pinned, because the server knows it: the base path, the
per-request nonce, and the user's own settings, which are live state.
"""
import argparse
import html
import json
import pathlib
import re
import sys
import urllib.request

OUT = pathlib.Path("console")


def fetch(url, path):
    with urllib.request.urlopen(url + path, timeout=30) as answer:
        return answer.read().decode()


def element(page, name):
    """The JSON an `<osd-…  data="…">` element carries."""
    found = re.search(rf'<{name} data="([^"]*)"', page)
    if not found:
        raise SystemExit(f"no <{name}> in the page: the shell is not the shape this reads")
    return json.loads(html.unescape(found.group(1)))


def bundles_of(boot):
    """The bundle URLs the boot script loads, in the order it loads them."""
    block = re.search(r"\[\s*((?:'[^']*',?\s*)+)\]\s*,\s*function", boot)
    if not block:
        raise SystemExit("the boot script does not name its bundles the way this reads")
    return re.findall(r"'([^']+)'", block.group(1))


def capabilities(url):
    """What a caller may do, with the part that depends on the request left out."""
    request = urllib.request.Request(
        url + "/api/core/capabilities",
        data=json.dumps({"applications": []}).encode(),
        headers={"content-type": "application/json", "osd-xsrf": "true"},
    )
    with urllib.request.urlopen(request, timeout=30) as answer:
        found = json.loads(answer.read())
    found.pop("navLinks", None)
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:5613")
    ap.add_argument("--app", default="home", help="any application; the shell is the same")
    args = ap.parse_args()
    url = args.url.rstrip("/")

    page = fetch(url, f"/app/{args.app}")
    boot = fetch(url, "/bootstrap.js")
    meta = element(page, "osd-injected-metadata")
    csp = element(page, "osd-csp")

    version = meta["version"]
    # the user's own settings are live state, not part of the contract
    settings = meta.get("legacyMetadata", {}).get("uiSettings", {})
    settings.pop("user", None)

    pinned = {
        "what": "the contract between the OpenSearch Dashboards front end and the "
                "server behind it, read out of a running one. Regenerate with "
                "tools/osd_pin.py rather than editing.",
        "version": version,
        "buildNumber": meta["buildNumber"],
        "branch": meta.get("branch"),
        "env": meta["env"],
        "csp": {**csp, **meta.get("csp", {})},
        "i18n": meta.get("i18n", {}),
        "vars": meta.get("vars", {}),
        "anonymousStatusPage": meta.get("anonymousStatusPage", True),
        "branding": meta.get("branding", {}),
        "survey": meta.get("survey"),
        "uiPlugins": meta["uiPlugins"],
        "uiSettingDefaults": settings.get("defaults", {}),
        # the boot script's own two lists, which are ordered by a dependency
        # sort the manifests alone do not give back
        "publicPaths": dict(re.findall(r'"([^"]+)":"([^"]+)"',
                                       re.search(r"__osdPublicPath__ = (\{.*?\});", boot,
                                                 re.S).group(1))),
        "bundles": bundles_of(boot),
        "styleSheets": re.findall(r"'(/[^']*\.css)'", boot),
        # The page around the two elements: the fonts, the favicons, the
        # loading markup and the two scripts. It is not behaviour either --
        # it is the same bytes for every request but the metadata in the
        # middle, so it is carried rather than reimplemented. Taken from a
        # server with no base path, and the base path is put back in front of
        # every absolute URL when it is served.
        "shellHead": page[: page.index("<osd-csp")],
        "shellTail": page[page.index("</osd-injected-metadata>") + len("</osd-injected-metadata>") :],
        "startup": fetch(url, "/startup.js"),
        # What a caller may do, as the plugins between them decide. `navLinks`
        # is left out: it is one entry per application the caller asked about,
        # so it is the request's shape rather than the server's.
        "capabilities": capabilities(url),
    }

    OUT.mkdir(exist_ok=True)
    path = OUT / f"osd-{version}.json"
    path.write_text(json.dumps(pinned, indent=2, sort_keys=False) + "\n")
    print(f"  {path}")
    print(f"  {len(pinned['uiPlugins'])} plugins, {len(pinned['bundles'])} bundles, "
          f"{len(pinned['uiSettingDefaults'])} setting defaults, "
          f"{len(pinned['styleSheets'])} stylesheets")
    return 0


if __name__ == "__main__":
    sys.exit(main())
