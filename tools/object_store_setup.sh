#!/bin/sh
# The three emulators the object-store repositories are checked against, and a
# bucket in each. They speak the protocols the real services speak, which is
# what makes the check worth running: the signing is what is being tested.
set -e
docker rm -f bs-minio bs-azurite bs-gcs >/dev/null 2>&1 || true
docker run -d --name bs-minio -p 9401:9000 \
    -e MINIO_ROOT_USER=boostkey -e MINIO_ROOT_PASSWORD=boostsecret123 \
    minio/minio:latest server /data >/dev/null
docker run -d --name bs-azurite -p 9402:10000 \
    mcr.microsoft.com/azure-storage/azurite:latest azurite-blob --blobHost 0.0.0.0 >/dev/null
docker run -d --name bs-gcs -p 9403:4443 \
    fsouza/fake-gcs-server:latest -scheme http -public-host 127.0.0.1:9403 -backend memory >/dev/null

until curl -s -o /dev/null http://127.0.0.1:9401/minio/health/live; do sleep 1; done
docker run --rm --network host --entrypoint sh minio/mc:latest -c \
    "mc alias set bs http://127.0.0.1:9401 boostkey boostsecret123 >/dev/null && mc mb --ignore-existing bs/snapshots" >/dev/null

sleep 3
python3 - <<'PY'
import base64, hashlib, hmac, urllib.request, urllib.error, datetime
ACC = "devstoreaccount1"
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
date = datetime.datetime.now(datetime.timezone.utc).strftime("%a, %d %b %Y %H:%M:%S GMT")
xms = {"x-ms-date": date, "x-ms-version": "2021-08-06"}
canon = "".join(f"{k}:{v}\n" for k, v in sorted(xms.items()))
# the emulator carries the account in the path, so it appears twice in the
# resource, which is what the rule says and what the emulator checks
to_sign = ("\n".join(["PUT", "", "", "", "", "application/x-www-form-urlencoded", "", "", "", "", "", ""])
           + "\n" + canon + f"/{ACC}/{ACC}/snapshots\nrestype:container")
sig = base64.b64encode(hmac.new(base64.b64decode(KEY), to_sign.encode(), hashlib.sha256).digest()).decode()
headers = dict(xms, Authorization=f"SharedKey {ACC}:{sig}")
try:
    urllib.request.urlopen(urllib.request.Request(
        f"http://127.0.0.1:9402/{ACC}/snapshots?restype=container", method="PUT", data=b"", headers=headers))
except urllib.error.HTTPError as e:
    if e.code != 409:
        raise
PY

curl -s -o /dev/null -X POST "http://127.0.0.1:9403/storage/v1/b?project=test" \
    -H 'content-type: application/json' -d '{"name":"snapshots"}'
echo "minio 9401, azurite 9402, fake-gcs 9403, each with a bucket called snapshots"
