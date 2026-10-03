#!/bin/bash
cd "D:/shichencike/Desktop/hone"
pass=0; fail=0; failed=""
for f in examples/*.hn; do
  base=$(basename "$f" .hn)
  case "$base" in
    server_selftest|spider_demo|ffi_demo|ffi_header|ai_demo) continue;;
    guipro_*) continue;;
    *guide|interactive|*input|stdin*) continue;;
  esac
  i_out=$(./target/debug/hone.exe run "$f" 2>&1)
  v_out=$(./target/debug/hone.exe run --vm "$f" 2>&1)
  if [ "$i_out" = "$v_out" ]; then pass=$((pass+1)); echo "PASS $base"
  else fail=$((fail+1)); failed="$failed $base"; echo "FAIL $base"
    diff <(echo "$i_out") <(echo "$v_out") | head -8
    echo "----"
  fi
done
echo "PASS=$pass FAIL=$fail"
echo "FAILED:$failed"
