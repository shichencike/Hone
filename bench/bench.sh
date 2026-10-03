#!/usr/bin/env bash
# Hone 三语言多维度基准运行脚本：hone vs python vs rust（每项 3 次取最快，输出毫秒）。
# 用法：bash bench/bench.sh  （默认用 target/release/hone；可用 HONE=... 覆盖）
set -u
cd "$(dirname "$0")" || exit 1

HONE=${HONE:-../target/release/hone}
PY=${PY:-python}
RUSTC=${RUSTC:-rustc}
OUT=../target/bench
mkdir -p "$OUT"

# .hn 文件名与统一基准名的映射（历史命名）
declare -A HN=([loop20m]=loop [str_concat]=str [dict_lookup]=dict)

BENCHES=(fib loop20m loop2m str_concat pi list_append dict_lookup str_replace sort)

echo "== 编译 Rust 基准（rustc -O）=="
for b in "${BENCHES[@]}"; do
    if ! "$RUSTC" -O "rs/$b.rs" -o "$OUT/$b" 2>/dev/null; then
        echo "rustc 编译失败: $b"
        exit 1
    fi
done
echo "== 编译完成 =="

# run3 <cmd...>：运行 3 次，返回最快耗时（毫秒）
run3() {
    local best=""
    for _ in 1 2 3; do
        local s e ms
        s=$(date +%s%N)
        "$@" > /dev/null 2>&1
        e=$(date +%s%N)
        ms=$(( (e - s) / 1000000 ))
        if [[ -z "$best" || $ms -lt $best ]]; then best=$ms; fi
    done
    echo "$best"
}

printf "%-14s %12s %12s %12s\n" "基准" "hone(ms)" "python(ms)" "rust(ms)"
for b in "${BENCHES[@]}"; do
    hname="${HN[$b]:-$b}"
    h=$(run3 "$HONE" "$hname.hn")
    p=$(run3 "$PY" "py/$b.py")
    r=$(run3 "$OUT/$b")
    printf "%-14s %12s %12s %12s\n" "$b" "$h" "$p" "$r"
done
echo "（数值越小越快；3 次运行取最快。机器与版本信息见 官网/perf.html）"
