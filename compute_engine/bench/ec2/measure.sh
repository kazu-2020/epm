#!/bin/bash
set -uxo pipefail
shutdown -h +90

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
apt-get -o DPkg::Lock::Timeout=600 install -y hyperfine time

cd /root
aws s3 cp --recursive "s3://$BUCKET/bin/$RUN/" bin/
chmod +x bin/*
{ lscpu; free -g; uname -a; } > out/machine.txt

# Not limited by the shutdown alone: the shutdown stops the network before finish can upload the results.
timeout 75m bash -euxo pipefail <<'MEASURE'
bin/generate data > out/generate.txt
hyperfine --runs 5 --export-json out/recalculate.json --export-markdown out/recalculate.md 'bin/recalculate data column'
RECALCULATE_HASH=1 /usr/bin/time -v bin/recalculate data column 2> out/recalculate-time.txt
bin/edit data 100 > out/edit.txt 2> out/edit-log.txt
MEASURE
