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

# Not limited by the shutdown alone: the shutdown stops the network before finish can upload the results.
# Not MINUTES - 15 from now: the shutdown counts from the start of this script, so the setup above uses up part of it.
read -ra multiples <<< "$SCALES"
timeout "$(( (MINUTES - 15) * 60 - SECONDS ))s" bin/run.sh "${multiples[@]}"
