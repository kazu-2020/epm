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

cd /root
aws s3 cp --recursive "s3://$BUCKET/bin/$RUN/" bin/
chmod -R +x bin
