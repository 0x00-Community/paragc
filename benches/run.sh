#!/bin/bash
# 跑对比基准：每个配置跑 REPS 次取最小值，打印 Markdown 表。
#   ./run.sh                       全部库
#   ./run.sh paragc cppgc          只跑这几个
# 环境变量：REPS（缺省 5）、THREADS（并行档的线程数，缺省 nproc）、RUSTY_V8_ARCHIVE（cppgc 用的 V8 静态库，不设就下载）
set -euo pipefail
cd "$(dirname "$0")"
REPS=${REPS:-5}
THREADS=${THREADS:-$(nproc)}
LIBS=("$@")
[ ${#LIBS[@]} -eq 0 ] && LIBS=(paragc paragc-public cppgc cppgc-par cppgc-detached gc-arena rust-gc dumpster)
# cppgc-par / cppgc-detached 都是 bench-cppgc：前者打开全部并行 / 并发标记选项，后者不挂 isolate（CPPGC_HEAP=detached）
CPPGC_PAR="--parallel-marking --concurrent-marking --cppheap-concurrent-marking --cppheap-incremental-marking --concurrent-marking-max-worker-num=$THREADS"
cargo build --release -q $(printf -- '-p bench-%s ' "${LIBS[@]}" | sed 's/-p bench-cppgc-par//; s/-p bench-cppgc-detached//')

# 每个库支持的 (工作负载, 线程数)
configs() {
    case $1 in
        paragc) echo "pause 1" "pause 4" "pause $THREADS" "pause-cm 1" "pause-cm 4" "pause-cm $THREADS" "pause-cs 1" "pause-cs 4" "pause-cs $THREADS" "pause-cms 1" "pause-cms 4" "pause-cms $THREADS" "gcbench 1" "gcbench 4" "gcbench $THREADS" "alloc-mt 1" "alloc-mt 4" "alloc-mt $THREADS" ;;
        paragc-public) echo "pause 1" "pause 4" "pause $THREADS" "gcbench 1" "gcbench 4" "gcbench $THREADS" ;;
        cppgc | cppgc-par | cppgc-detached) echo "pause 1" "pause 4" "pause $THREADS" "gcbench 1" "gcbench 4" "gcbench $THREADS" ;;
        dumpster) echo "pause 1" "gcbench 1" "alloc-mt 1" "alloc-mt 4" "alloc-mt $THREADS" ;;
        *) echo "pause 1" "gcbench 1" ;;
    esac
}

echo "| 库 | 工作负载 | 线程 | 墙钟 ms（$REPS 次最小） | 那一次的 CPU ms | CPU ÷ 墙钟 |"
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

# 碎片负载（四种大小、随机寿命，存活约 80 MB）：耗时与回收后 / 峰值 RSS，单独一张表
FRAG_LIBS=()
for lib in "${LIBS[@]}"; do
    case $lib in paragc | cppgc | cppgc-detached) FRAG_LIBS+=("$lib") ;; esac
done
if [ ${#FRAG_LIBS[@]} -gt 0 ]; then
    echo
    echo "| 库 | 线程 | 墙钟 ms（$REPS 次最小） | 回收后 RSS MB | 峰值 RSS MB |"
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
