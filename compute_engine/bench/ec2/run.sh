#!/bin/bash
set -euxo pipefail

# Not the 4 KB pages that glibc malloc uses by default: their faults and unmaps took about a fifth of the 32-thread time (bench #13).
export GLIBC_TUNABLES=glibc.malloc.hugetlb=1
# Not any vCPUs: with SMT, two threads can share one core, so take one CPU per physical core.
mapfile -t cores < <(lscpu -p=CPU,CORE | awk -F, '!/^#/ && !seen[$2]++ {print $1}')
cpus() { printf '%s\n' "${cores[@]:0:$1}" | paste -sd,; }
counts=()
for threads in 1 4 8 16 32 64; do
    if [ "$threads" -le "${#cores[@]}" ]; then counts+=("$threads"); fi
done
most=${counts[-1]}
binaries=(new)
if [ -d bin/baseline ]; then binaries+=(baseline); fi

timed() {
    local result=$1 binary=$2 scale=$3
    shift 3
    # Not stopped by set -e: one failed binary must not throw away the runs of the other.
    RECALCULATE_HASH=1 "$@" /usr/bin/time -v "bin/$binary/recalculate" "data-$scale" column > "$result" 2>&1 ||
        echo "exit code $?" >> "$result"
}

for threads in $(printf '%s\n' 1 8 "$most" | awk -v most="$most" '$1 <= most' | sort -nu); do
    taskset -c "$(cpus "$threads")" bin/new/stream "$threads" 1024 >> out/stream.txt
done

for scale in "$@"; do
    mkdir -p "out/scale-$scale"
    bin/new/generate "data-$scale" "$scale" > "out/scale-$scale/generate.txt" 2>&1
    declare -A rounds=()
    if [ "$scale" = 1 ]; then
        hyperfine --runs 5 --export-json out/recalculate.json --export-markdown out/recalculate.md 'bin/new/recalculate data-1 column'
        timed out/recalculate-time.txt new 1
        bin/new/edit data-1 100 > out/edit.txt 2> out/edit-log.txt
        for threads in 1 "$most"; do
            # Not the cycles event: cpu-clock works on a VM without the PMU.
            RAYON_NUM_THREADS=$threads taskset -c "$(cpus "$threads")" perf record -e cpu-clock -g -o "perf-$threads.data" bin/new/recalculate data-1 column > /dev/null 2>&1 &&
                perf report -i "perf-$threads.data" --stdio --no-children --percent-limit 0.5 > "out/perf-$threads.txt" 2>&1 || true
            rm -f "perf-$threads.data"
        done
        for binary in "${binaries[@]}"; do
            timed "out/scale-1/$binary-smt-1.txt" "$binary" 1
        done
        for threads in "${counts[@]}"; do rounds[$threads]=3; done
        rounds[1]=5
        rounds[$most]=5
    else
        rounds=([1]=1 [$most]=3)
    fi
    for round in $(seq "$(printf '%s\n' "${rounds[@]}" | sort -n | tail -n 1)"); do
        for threads in "${counts[@]}"; do
            if [ "$round" -gt "${rounds[$threads]:-0}" ]; then continue; fi
            for binary in "${binaries[@]}"; do
                RAYON_NUM_THREADS=$threads timed "out/scale-$scale/$binary-threads-$threads-$round.txt" "$binary" "$scale" \
                    taskset -c "$(cpus "$threads")"
            done
        done
    done
    rm -rf "data-$scale"
done
grep -E '^(thp_fault|compact_stall)' /proc/vmstat > out/vmstat.txt
