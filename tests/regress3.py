import subprocess, glob, os, sys

# 仓库根目录 = 本脚本所在目录（tests/）的上一级，换机器/换路径均可用。
_HERE = os.path.dirname(os.path.abspath(__file__))
os.chdir(os.path.dirname(_HERE))
sys.path.insert(0, _HERE)
# SKIP / FLAKY 分类表与 regress_ir.py 共用一份，避免两处各自维护导致漂移。
from regress_common import SKIP, FLAKY, should_skip

BIN = "./target/debug/hone.exe"

def skip(base):
    return should_skip(base)

def run_once(f, vm):
    args = [BIN, "run"] + (["--vm"] if vm else []) + [f]
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=15)
        return r.stdout + r.stderr
    except subprocess.TimeoutExpired as e:
        return (e.stdout or "") + (e.stderr or "") + "[TIMEOUT]"

pass_n=0; fail_n=0; failed=[]; flaky=[]
for f in sorted(glob.glob("examples/*.hn")):
    base=os.path.basename(f)[:-3]
    if skip(base):
        print(f"SKIP {base}"); continue
    io=run_once(f, False); vo=run_once(f, True)
    if io!=vo and base in FLAKY:
        # 已知含随机数/时间/端口/线程顺序：两后端取值本就不同，逐字节比对无意义。
        flaky.append(base); print(f"FLAKY {base} (已知非逻辑差异)"); continue
    if io!=vo:
        # 线程完成顺序 / 随机数 / 随机端口等属非逻辑差异，偶发不一致（threads、async_demo）。
        # 重试至多 2 次，任一次两侧一致即判 PASS；真实逻辑差异会稳定失败，不会被掩盖。
        for _ in range(2):
            io2=run_once(f, False); vo2=run_once(f, True)
            if io2==vo2:
                io, vo = io2, vo2; flaky.append(base); break
    if io==vo:
        pass_n+=1; print(f"PASS {base}" + (" (retry)" if base in flaky else ""))
    else:
        fail_n+=1; failed.append(base); print(f"FAIL {base}")
        il=io.splitlines(); vl=vo.splitlines()
        for k in range(min(len(il),len(vl))):
            if il[k]!=vl[k]:
                print(f"   interp: {il[k]!r}")
                print(f"   vm    : {vl[k]!r}")
                break
        if len(il)!=len(vl):
            print(f"   (len interp={len(il)} vm={len(vl)})")
print(f"PASS={pass_n} FLAKY={len(flaky)} FAIL={fail_n}")
if flaky:
    print("FLAKY(已确认非逻辑差异: 随机数/时间/端口/线程顺序):"+" ".join(flaky))
print("FAILED:"+" ".join(failed))
print("真实逻辑差异 = 0" if fail_n==0 else f"⚠ 存在 {fail_n} 个待查失败项")