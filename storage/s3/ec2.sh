#!/bin/bash
set -uxo pipefail
shutdown -h "+$MINUTES"

finish() {
    code=$?
    set +e
    cp /var/log/cloud-init-output.log /root/out/
    aws s3 cp --recursive /root/out "s3://$BUCKET/results/$RUN/"
    echo "$code" | aws s3 cp - "s3://$BUCKET/results/$RUN/exit-code"
    shutdown -h now
}
mkdir -p /root/out
trap finish EXIT
set -e

snap wait system seed.loaded
snap install aws-cli --classic
apt-get -o DPkg::Lock::Timeout=600 update
apt-get -o DPkg::Lock::Timeout=600 install -y python3-venv
python3 -m venv /root/venv
# Not the boto3 of Ubuntu 24.04: it is older than the IfNoneMatch argument of PutObject.
/root/venv/bin/pip install boto3==1.43.100

cd /root
aws s3 cp "s3://$BUCKET/bin/$RUN/latency.py" .
TOKEN=$(curl -sX PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
for item in instance-type placement/availability-zone; do
    echo "$item: $(curl -sH "X-aws-ec2-metadata-token: $TOKEN" "http://169.254.169.254/latest/meta-data/$item")"
done > out/machine.txt

# Not limited by the shutdown alone: the shutdown stops the network before finish can upload the results.
# Not MINUTES - 10 from now: the shutdown counts from boot, and the setup above can take minutes.
timeout "$((MINUTES * 60 - 600 - $(cut -d. -f1 /proc/uptime)))s" /root/venv/bin/python latency.py "$BUCKET" "latency/$RUN" out
