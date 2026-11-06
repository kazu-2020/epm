#!/bin/bash
apt-get -o DPkg::Lock::Timeout=600 install -y hyperfine time numactl
apt-get -o DPkg::Lock::Timeout=600 install -y "linux-tools-$(uname -r)" || true

{
    lscpu
    numactl -H
    free -g
    uname -a
    cat /sys/kernel/mm/transparent_hugepage/enabled
    # Not the full counter list: a VM smaller than a socket may not expose the PMU, and this line shows if it does.
    perf stat -e cycles,instructions true 2>&1 || true
} > out/machine.txt 2>&1

read -ra multiples <<< "$SCALES"
timeout "$(time_before_shutdown 15)" bin/run.sh "${multiples[@]}"
