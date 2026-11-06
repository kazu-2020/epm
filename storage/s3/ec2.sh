#!/bin/bash
apt-get -o DPkg::Lock::Timeout=600 install -y python3-venv
python3 -m venv /root/venv
# Not the boto3 of Ubuntu 24.04: it is older than the IfNoneMatch argument of PutObject.
/root/venv/bin/pip install boto3==1.43.100

TOKEN=$(curl -sX PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
for item in instance-type placement/availability-zone; do
    echo "$item: $(curl -sH "X-aws-ec2-metadata-token: $TOKEN" "http://169.254.169.254/latest/meta-data/$item")"
done > out/machine.txt

timeout "$(time_before_shutdown 10)" /root/venv/bin/python bin/latency.py "$BUCKET" "latency/$RUN" out
