#!/bin/bash
cd "D:\shichencike\Desktop\hone"
pass=0; fail=0; failed=""
for f in examples/*.hn; do
  base=$(basename "$f" .hn)
  case "$base" in
    server_selftest|spider_demo|ffi_demo|ffi_header|ai_demo) continue;;
    guipro_*) continue;;
    *guide|interactive|*input|stdin*) continue;;
  esac
  i_out=$(timeout 10 ./target/debug/hone.exe run "$f" 2>&1)
  v_out=$(timeout 10 ./target/debug/hone.exe run --vm "$f" 2>&1)
  if [ "$i_out" = "$v_out" ]; then pass=$((pass+1)); echo "PASS $base" >> /tmp/regress.txt
  else fail=$((fail+1)); failed="$failed $base"; echo "FAIL $base" >> /tmp/regress.txt
    diff <(echo "$i_out") <(echo "$v_out") | head -6 >> /tmp/regress.txt; echo "----" >> /tmp/regress.txt
  fi
done
echo "PASS=$pass FAIL=$fail" >> /tmp/regress.txt
echo "FAILED:$failed" >> /tmp/regress.txt
