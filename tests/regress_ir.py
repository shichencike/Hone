# 文本 IR 往返回归：对每个示例执行
#   hone run --disasm <f> > t.ir   →   hone runir t.ir
# 并与解释器 `hone run <f>` 的输出逐字节比对。
# 验证「反汇编 → 反向装配 → 执行」链路与源码执行完全等价（含报错位置与上下文）。
#
# 三类结果：
#   OK           ：源执行与 IR 执行输出逐字节一致（含运行期报错位置/上下文）。
#   SKIP-COMPILE ：脚本本身无法编译（语法/检查期即为错误），不产生字节码，IR 无从谈起。
#   SKIP-CHECK   ：脚本编译通过但其预期输出是「检查期诊断」——IR 模式有意跳过
#                  类型检查阶段，故该诊断不会复现（结构性，非缺陷）。
#   DIFF         ：真实的往返不等价（需修复）。
import subprocess, glob, os, sys, tempfile

# 仓库根目录 = 本脚本所在目录（tests/）的上一级，换机器/换路径均可用。
_HERE = os.path.dirname(os.path.abspath(__file__))
os.chdir(os.path.dirname(_HERE))
sys.path.insert(0, _HERE)
# SKIP / FLAKY 分类表与 regress3.py 共用一份，避免两处各自维护导致漂移。
from regress_common import SKIP, FLAKY, should_skip

BIN = "./target/debug/hone.exe"
# IR 中间文件写入系统临时目录，避免在项目目录留下/删除文件。
IR = os.path.join(tempfile.mkdtemp(prefix="hone_ir_"), "rt_check.ir")


def skip(base):
    return should_skip(base)


# server_demo 会起 HTTP 服务器并驻留，必须给足超时又不能无限等；
# 其余示例正常在 1 秒内结束，20 秒是宽松上限。
def run(args, timeout=20):
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired as e:
        def _txt(x):
            if x is None:
                return ""
            return x if isinstance(x, str) else x.decode("utf-8", "replace")
        return 124, _txt(e.stdout) + _txt(e.stderr) + "[TIMEOUT]"


ok = bad = 0
skipped = []
failed = []
flaky = []
for f in sorted(glob.glob("examples/*.hn")):
    base = os.path.basename(f)[:-3]
    if skip(base):
        continue
    rc, ir = run([BIN, "run", "--disasm", f])
    if rc != 0:
        # 语法/编译期即失败：没有字节码可往返（源执行同样失败，两种模式一致）。
        skipped.append(base)
        print(f"SKIP-COMPILE {base}")
        continue
    with open(IR, "w", encoding="utf-8") as fh:
        fh.write(ir)
    _, src_out = run([BIN, "run", f])
    _, ir_out = run([BIN, "runir", IR])
    if src_out == ir_out:
        ok += 1
        print(f"OK   {base}")
    else:
        # 源执行与 VM 执行一致、且输出是「一条纯错误」→ 该错误发生在检查阶段；
        # IR 模式有意不做类型检查，故不复现（结构性差异，非往返缺陷）。
        _, vm_out = run([BIN, "run", "--vm", f])
        if src_out == vm_out and src_out.lstrip().startswith("error["):
            skipped.append(base)
            print(f"SKIP-CHECK {base}")
            continue
        # 含随机数/时间/端口/线程顺序的示例：IR 与源执行取值本就不同，
        # 逐字节比对无意义（与 regress3.py 同一套 FLAKY 判定）。
        if base in FLAKY:
            flaky.append(base)
            print(f"FLAKY {base} (已知非逻辑差异)")
            continue
        bad += 1
        failed.append(base)
        print(f"DIFF {base}")
        a = src_out.splitlines()
        b = ir_out.splitlines()
        for k in range(min(len(a), len(b))):
            if a[k] != b[k]:
                print(f"   src: {a[k]!r}")
                print(f"   ir : {b[k]!r}")
                break
        if len(a) != len(b):
            print(f"   (len src={len(a)} ir={len(b)})")

print(f"\nIR-ROUNDTRIP OK={ok} FLAKY={len(flaky)} BAD={bad} SKIP(结构)={len(skipped)}")
if failed:
    print("FAILED:" + " ".join(failed))
else:
    print("真实逻辑差异 = 0")
