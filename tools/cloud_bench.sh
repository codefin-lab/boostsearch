#!/bin/sh
# The bench matrix on hardware a release would be cut on.
#
# The numbers in docs/progress.md are from a developer machine, which is not a
# release gate: a laptop throttles, shares its disk with everything else on it,
# and has a page cache the size of the corpus. This runs the same matrix on a
# machine rented for the length of the run and gives it back afterwards.
#
# Nothing here is run for you. It costs money on somebody's account, and the
# instance type and region are choices about what the number means -- so it
# prints what it would do and stops unless told to go ahead.
#
#   tools/cloud_bench.sh                 say what it would do
#   BENCH_GO=1 tools/cloud_bench.sh      do it
#
# What it needs on the host: docker, and nothing else. Both engines run in
# containers on the same machine so that neither gets a better one.
set -e

REGION=${BENCH_REGION:-ap-southeast-1}
TYPE=${BENCH_TYPE:-c7i.4xlarge}
DISK=${BENCH_DISK:-200}
NAME=${BENCH_NAME:-boostsearch-bench}
# c7i.4xlarge: 16 vCPU, 32 GiB, dedicated bandwidth. A search engine bench on
# a burstable instance measures the burst credits.
HOURLY=${BENCH_HOURLY:-0.85}

cat <<PLAN
This would, in $REGION:

  1. start one $TYPE with a ${DISK}GB gp3 volume, named $NAME
  2. install docker on it
  3. run OpenSearch 3.1.0 and BoostSearch side by side in containers
  4. generate the 200,000-document corpus and run tools/bench_matrix.py
  5. bring the numbers back to bench/cloud-\$(date +%F).json
  6. terminate the instance

It costs about \$$HOURLY an hour and takes roughly 40 minutes, so about
\$$(awk "BEGIN{printf \"%.2f\", $HOURLY * 0.7}").

The instance is terminated on the way out, including if the run fails. It is
not terminated if this script is killed between starting it and reaching the
trap, so check for one named $NAME afterwards either way.
PLAN

if [ "${BENCH_GO:-}" != "1" ]; then
    echo
    echo "Nothing was started. Set BENCH_GO=1 to run it."
    exit 0
fi

ami=$(aws ec2 describe-images --region "$REGION" --owners amazon \
    --filters 'Name=name,Values=al2023-ami-2023*-x86_64' 'Name=state,Values=available' \
    --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
echo "image $ami"

id=$(aws ec2 run-instances --region "$REGION" --image-id "$ami" --instance-type "$TYPE" \
    --block-device-mappings "DeviceName=/dev/xvda,Ebs={VolumeSize=$DISK,VolumeType=gp3}" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$NAME}]" \
    --query 'Instances[0].InstanceId' --output text)
echo "instance $id"
# whatever happens next, the instance is given back
trap 'echo "terminating $id"; aws ec2 terminate-instances --region "$REGION" --instance-ids "$id" >/dev/null' EXIT INT TERM

aws ec2 wait instance-running --region "$REGION" --instance-ids "$id"
echo "running; the rest is driven over SSM, so no key and no open port"

run() {
    cmd=$(aws ssm send-command --region "$REGION" --instance-ids "$id" \
        --document-name AWS-RunShellScript --parameters "commands=[\"$1\"]" \
        --query 'Command.CommandId' --output text)
    aws ssm wait command-executed --region "$REGION" --command-id "$cmd" --instance-id "$id" || true
    aws ssm get-command-invocation --region "$REGION" --command-id "$cmd" --instance-id "$id" \
        --query 'StandardOutputContent' --output text
}

echo "waiting for the agent"
until aws ssm describe-instance-information --region "$REGION" \
        --filters "Key=InstanceIds,Values=$id" --query 'InstanceInformationList[0]' \
        --output text 2>/dev/null | grep -q .; do sleep 10; done

run "dnf install -y docker git python3 && systemctl start docker"
run "docker run -d --name os-bench -p 9201:9200 -e discovery.type=single-node \
     -e DISABLE_SECURITY_PLUGIN=true -e OPENSEARCH_JAVA_OPTS='-Xms8g -Xmx8g' \
     opensearchproject/opensearch:3.1.0"
run "git clone --depth 1 https://github.com/codefin-lab/boostsearch /opt/bs"
run "cd /opt/bs && docker build -t boostsearch . && docker run -d --name bs -p 9202:9200 boostsearch"
run "cd /opt/bs && python3 tools/gen_dataset.py --out /tmp/bench_logs.ndjson"
out=$(run "cd /opt/bs && BENCH_A=http://localhost:9201 BENCH_B=http://localhost:9202 \
     BENCH_A_CONTAINER=os-bench BENCH_OUT=/tmp/matrix.json python3 tools/bench_matrix.py; \
     cat /tmp/matrix.json")

mkdir -p bench
printf '%s' "$out" | sed -n '/^{/,$p' > "bench/cloud-$(date +%F).json"
printf '%s' "$out" | sed -n '1,/^{/p'
echo "written to bench/cloud-$(date +%F).json"
