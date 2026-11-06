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

# Not limited by the shutdown alone: the shutdown stops the network before finish can upload the results.
# Not N minutes before the shutdown counted from the call: the shutdown counts from the start of this script, and the setup uses up part of it.
time_before_shutdown() { echo "$(( (MINUTES - $1) * 60 - SECONDS ))s"; }
