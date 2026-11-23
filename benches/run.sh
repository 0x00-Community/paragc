#!/bin/bash
# Run the comparison benchmarks: each configuration runs REPS times, keeping the minimum, and prints a Markdown table.
#   ./run.sh                       every library
#   ./run.sh paragc cppgc          only these
# Environment: REPS (default 5), THREADS (thread count of the parallel runs, default nproc), RUSTY_V8_ARCHIVE (the V8 static library for cppgc; downloaded when unset)
set -euo pipefail
cd "$(dirname "$0")"
REPS=${REPS:-5}
THREADS=${THREADS:-$(nproc)}
LIBS=("$@")
[ ${#LIBS[@]} -eq 0 ] && LIBS=(paragc paragc-public cppgc cppgc-par cppgc-detached gc-arena rust-gc dumpster)
# cppgc-par / cppgc-detached are both bench-cppgc: the former turns on every parallel / concurrent marking option, the latter does not attach an isolate (CPPGC_HEAP=detached)
CPPGC_PAR="--parallel-marking --concurrent-marking --cppheap-concurrent-marking --cppheap-incremental-marking --concurrent-marking-max-worker-num=$THREADS"
cargo build --release -q $(printf -- '-p bench-%s ' "${LIBS[@]}" | sed 's/-p bench-cppgc-par//; s/-p bench-cppgc-detached//')

# (workload, threads) supported by each library
configs() {
    case $1 in
        paragc) echo "pause 1" "pause 4" "pause $THREADS" "pause-cm 1" "pause-cm 4" "pause-cm $THREADS" "pause-cs 1" "pause-cs 4" "pause-cs $THREADS" "pause-cms 1" "pause-cms 4" "pause-cms $THREADS" "gcbench 1" "gcbench 4" "gcbench $THREADS" "alloc-mt 1" "alloc-mt 4" "alloc-mt $THREADS" ;;
        paragc-public) echo "pause 1" "pause 4" "pause $THREADS" "gcbench 1" "gcbench 4" "gcbench $THREADS" ;;
        cppgc | cppgc-par | cppgc-detached) echo "pause 1" "pause 4" "pause $THREADS" "gcbench 1" "gcbench 4" "gcbench $THREADS" ;;
        dumpster) echo "pause 1" "gcbench 1" "alloc-mt 1" "alloc-mt 4" "alloc-mt $THREADS" ;;
        *) echo "pause 1" "gcbench 1" ;;
    esac
}

echo "| Library | Workload | Threads | Wall ms (min of $REPS) | CPU ms of that run | CPU ÷ wall |"
echo "|---|---|---:|---:|---:|---:|"
for lib in "${LIBS[@]}"; do
    while read -r workload threads; do
        best=""
        for _ in $(seq "$REPS"); do
            case $lib in
                cppgc-par) line=$(V8_FLAGS="$CPPGC_PAR" timeout 300 ./target/release/bench-cppgc "$workload" "$threads" 2>/dev/null) ;;
                cppgc-detached) line=$(CPPGC_HEAP=detached timeout 300 ./target/release/bench-cppgc "$workload" "$threads" 2>/dev/null) ;;
                *) line=$(timeout 300 "./target/release/bench-$lib" "$workload" "$threads" 2>/dev/null) ;;
            esac
            best=$(printf '%s\n%s\n' "$best" "$line" | grep . | sort -t$'\t' -k4 -g | head -1)
        done
        echo "$best" | awk -F'\t' -v lib="$lib" '{printf "| %s | %s | %s | %.2f | %.2f | %.1f |\n", lib, $2, $3, $4, $5, $5 / $4}'
    done < <(configs "$lib" | tr ' ' '\n' | paste - -)
done

# Fragmentation load (four sizes, random lifetimes, about 80 MB live): time and RSS after collection / at peak, in a table of its own
FRAG_LIBS=()
for lib in "${LIBS[@]}"; do
    case $lib in paragc | cppgc | cppgc-detached) FRAG_LIBS+=("$lib") ;; esac
done
if [ ${#FRAG_LIBS[@]} -gt 0 ]; then
    echo
    echo "| Library | Threads | Wall ms (min of $REPS) | RSS after collection MB | Peak RSS MB |"
    echo "|---|---:|---:|---:|---:|"
    for lib in "${FRAG_LIBS[@]}"; do
        for threads in 1 "$THREADS"; do
            best=""
            for _ in $(seq "$REPS"); do
                case $lib in
                    cppgc-detached) line=$(CPPGC_HEAP=detached timeout 300 ./target/release/bench-cppgc frag "$threads" 2>/dev/null) ;;
                    *) line=$(timeout 300 "./target/release/bench-$lib" frag "$threads" 2>/dev/null) ;;
                esac
                best=$(printf '%s\n%s\n' "$best" "$line" | grep . | sort -t$'\t' -k4 -g | head -1)
            done
            echo "$best" | awk -F'\t' -v lib="$lib" '{printf "| %s | %s | %.2f | %.1f | %.1f |\n", lib, $3, $4, $6, $7}'
        done
    done
fi
