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
timeout "$(( (MINUTES - 15) * 60 - SECONDS ))s" bash -euxo pipefail <<'MEASURE'
# Not the 4 KB pages that glibc malloc uses by default: their faults and unmaps took about a fifth of the 32-thread time (bench #13).
export GLIBC_TUNABLES=glibc.malloc.hugetlb=1
# Not any vCPUs: with SMT, two threads can share one core, so take one CPU per physical core.
mapfile -t cores < <(lscpu -p=CPU,CORE | awk -F, '!/^#/ && !seen[$2]++ {print $1}')
pin() { echo taskset -c "$(printf '%s\n' "${cores[@]:0:$1}" | paste -sd,)"; }
counts=()
for threads in 1 4 8 16 32 64; do
    if [ "$threads" -le "${#cores[@]}" ]; then counts+=("$threads"); fi
done
most=${counts[-1]}
binaries=(new)
if [ -d bin/baseline ]; then binaries+=(baseline); fi

for threads in $(printf '%s\n' 1 8 "$most" | awk -v most="$most" '$1 <= most' | sort -nu); do
    $(pin "$threads") bin/new/stream "$threads" 1024 >> out/stream.txt
done

for scale in $SCALES; do
    mkdir -p "out/scale-$scale"
    bin/new/generate "data-$scale" "$scale" > "out/scale-$scale/generate.txt" 2>&1
    if [ "$scale" = 1 ]; then
        hyperfine --runs 5 --export-json out/recalculate.json --export-markdown out/recalculate.md 'bin/new/recalculate data-1 column'
        RECALCULATE_HASH=1 /usr/bin/time -v bin/new/recalculate data-1 column 2> out/recalculate-time.txt
        bin/new/edit data-1 100 > out/edit.txt 2> out/edit-log.txt
        for threads in 1 "$most"; do
            # Not the cycles event: cpu-clock works on a VM without the PMU.
            RAYON_NUM_THREADS=$threads $(pin "$threads") perf record -e cpu-clock -g -o "perf-$threads.data" bin/new/recalculate data-1 column > /dev/null 2>&1 &&
                perf report -i "perf-$threads.data" --stdio --no-children --percent-limit 0.5 > "out/perf-$threads.txt" 2>&1 || true
            rm -f "perf-$threads.data"
        done
        plan=()
        for threads in "${counts[@]}"; do plan+=("$threads:3"); done
        plan[0]=1:5
        plan[-1]=$most:5
    else
        plan=(1:1 "$most:3")
    fi
    for round in 1 2 3 4 5; do
        for entry in "${plan[@]}"; do
            threads=${entry%:*}
            if [ "$round" -gt "${entry#*:}" ]; then continue; fi
            for binary in "${binaries[@]}"; do
                result="out/scale-$scale/$binary-threads-$threads-$round.txt"
                # Not stopped by set -e: one failed binary must not throw away the runs of the other.
                RECALCULATE_HASH=1 RAYON_NUM_THREADS=$threads $(pin "$threads") /usr/bin/time -v "bin/$binary/recalculate" "data-$scale" column > "$result" 2>&1 ||
                    echo "exit code $?" >> "$result"
            done
        done
    done
    if [ "$scale" = 1 ]; then
        for binary in "${binaries[@]}"; do
            RECALCULATE_HASH=1 /usr/bin/time -v "bin/$binary/recalculate" data-1 column > "out/scale-1/$binary-smt-1.txt" 2>&1 ||
                echo "exit code $?" >> "out/scale-1/$binary-smt-1.txt"
        done
    fi
    rm -rf "data-$scale"
done
grep -E '^(thp_fault|compact_stall)' /proc/vmstat > out/vmstat.txt
MEASURE
