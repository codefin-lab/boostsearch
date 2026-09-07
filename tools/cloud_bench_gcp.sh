#!/bin/sh
# The bench matrix on a machine rented from GCP for the length of the run.
#
# Terraform in tools/cloud_bench/ makes one VM and one bucket; the VM's
# startup script runs OpenSearch 3.1.0 and BoostSearch side by side in
# containers, runs tools/bench_matrix.py and puts the numbers in the bucket.
# This script waits for them, brings them back to bench/, and destroys what
# it made -- on the way out whatever happened.
#
#   tools/cloud_bench_gcp.sh               say what it would do
#   BENCH_GO=1 tools/cloud_bench_gcp.sh    do it
#
# Needs: terraform, gcloud (logged in: `gcloud auth login` and
# `gcloud auth application-default login`), and a project with the Compute
# and Storage APIs on.
set -e
cd "$(dirname "$0")/cloud_bench"

PROJECT=${BENCH_PROJECT:-$(gcloud config get-value project 2>/dev/null)}
ZONE=${BENCH_ZONE:-asia-southeast1-b}
TYPE=${BENCH_TYPE:-c3-standard-16}
REF=${BENCH_REF:-$(git rev-parse HEAD)}
HOURLY=${BENCH_HOURLY:-1.0}

cat <<PLAN
This would, in project $PROJECT, zone $ZONE:

  1. make a bucket and one $TYPE with a 200 GB SSD, from tools/cloud_bench/
  2. on it: docker, OpenSearch 3.1.0 in a container, BoostSearch built from
     $REF
     and run in a container, a 200,000-document corpus, tools/bench_matrix.py
  3. wait for the numbers to land in the bucket, bring them back to
     bench/cloud-gcp-$(date +%F).json
  4. terraform destroy, whatever happened

About \$$HOURLY an hour; the build and the run take about an hour, so about
\$$HOURLY. If this script is killed before it reaches its trap, the VM is
still there: \`cd tools/cloud_bench && terraform destroy\`.
PLAN

if [ "${BENCH_GO:-}" != "1" ]; then
    echo
    echo "Nothing was started. Set BENCH_GO=1 to run it."
    exit 0
fi

export TF_VAR_project="$PROJECT" TF_VAR_zone="$ZONE" TF_VAR_machine_type="$TYPE" TF_VAR_source_ref="$REF"
terraform init -input=false >/dev/null
trap 'echo "giving it back"; terraform destroy -auto-approve -input=false >/dev/null && echo "destroyed"' EXIT INT TERM
terraform apply -auto-approve -input=false
bucket=$(terraform output -raw bucket)
instance=$(terraform output -raw instance)
echo "bucket $bucket, instance $instance; waiting (the serial console says where it is)"

# a run takes about an hour; a driver still waiting after this long is
# waiting for a machine that will never answer, and every minute costs
DEADLINE=${BENCH_DEADLINE:-10800}
started=$(date +%s)
seen=0
while :; do
    if gsutil -q stat "gs://$bucket/matrix.json" 2>/dev/null; then break; fi
    if [ $(( $(date +%s) - started )) -gt "$DEADLINE" ]; then
        echo "no numbers after $DEADLINE seconds; giving up"
        gcloud compute instances get-serial-port-output "$instance" --zone "$ZONE" 2>/dev/null | grep "bench:" | tail -20
        exit 1
    fi
    state=$(gcloud compute instances describe "$instance" --zone "$ZONE" --format='value(status)' 2>/dev/null || echo UNKNOWN)
    case "$state" in
        RUNNING|PROVISIONING|STAGING) ;;
        *) echo "the instance is $state; it will not answer"; exit 1 ;;
    esac
    if gsutil -q stat "gs://$bucket/failed.txt" 2>/dev/null; then
        echo "the run failed:"; gsutil cat "gs://$bucket/failed.txt"
        gcloud compute instances get-serial-port-output "$instance" --zone "$ZONE" 2>/dev/null | grep "bench:" | tail -20
        exit 1
    fi
    lines=$(gcloud compute instances get-serial-port-output "$instance" --zone "$ZONE" 2>/dev/null | grep -c "bench:" || true)
    if [ "$lines" -gt "$seen" ]; then
        gcloud compute instances get-serial-port-output "$instance" --zone "$ZONE" 2>/dev/null | grep "bench:" | tail -n $((lines - seen))
        seen=$lines
    fi
    sleep 30
done

mkdir -p ../../bench
gsutil cp "gs://$bucket/matrix.json" "../../bench/cloud-gcp-$(date +%F).json"
gsutil cat "gs://$bucket/matrix.txt"
echo "written to bench/cloud-gcp-$(date +%F).json"
