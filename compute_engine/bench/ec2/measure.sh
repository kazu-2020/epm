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
apt-get -o DPkg::Lock::Timeout=600 install -y hyperfine time numactl
apt-get -o DPkg::Lock::Timeout=600 install -y "linux-tools-$(uname -r)" || true

cd /root
aws s3 cp --recursive "s3://$BUCKET/bin/$RUN/" bin/
chmod -R +x bin
{
    lscpu
    numactl -H
    free -g
    uname -a
    cat /sys/kernel/mm/transparent_hugepage/enabled
    # Not the full counter list: a VM smaller than a socket may not expose the PMU, and this line shows if it does.
    perf stat -e cycles,instructions true 2>&1 || true
} > out/machine.txt 2>&1

# Not limited by the shutdown alone: the shutdown stops the network before finish can upload the results.
# Not MINUTES - 15 from now: the shutdown counts from the start of this script, so the setup above uses up part of it.
read -ra multiples <<< "$SCALES"
timeout "$(( (MINUTES - 15) * 60 - SECONDS ))s" bin/run.sh "${multiples[@]}"
