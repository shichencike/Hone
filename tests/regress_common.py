# 回归脚本共享的示例分类表。
#
# 为什么单独抽一个文件：regress3.py（解释器 vs VM）与 regress_ir.py（文本 IR 往返）
# 曾经各自维护一份 SKIP，结果两份逐渐漂移——regress_ir.py 少列了 import 依赖项，
# 于是 test_hone_lib / smoke_spider 等 6 个示例长期被误报为 DIFF（真实逻辑差异 = 0）。
# 分类表是「哪些示例不参与逐字节比对」的唯一事实来源，只应存在一份。

# 按设计无法在另一后端 1:1 复现的示例，直接跳过：
#   1) 原生 GUI / 需交互的示例（guipro_* 与 *input* 由 should_skip 兜底）
#   2) import_demo 需要本地 8800 端口的 HTTP 服务器（无常驻服务时两引擎等价的
#      只是「连接被拒」这一条错误，比对无意义；远程 import 的对齐由
#      tests/tools/probe_align.py 的 IMPORT 组自带 fixture server 覆盖）
# 说明：import（含远程下载）/ load / FFI / byte / bytes / with / type 实例 / 字段赋值
# 已在 VM 落地（T1-T9），原先「VM 报 H999 兜底」的 SKIP 口径已全部移除。
SKIP = {
    "gui_demo", "pet_demo",
    # 需要本地 HTTP 服务器（127.0.0.1:8800）才能跑通全部路径
    "import_demo",
    # https_demo 访问外网（example.com / httpbin.org）：返回内容与长度随网络变化，
    # 两次运行本就可能不同，属非逻辑差异。纳入 SKIP 以保证基线可复现。
    "https_demo",
}

# 含随机数 / 时间 / 端口 / 线程完成顺序的示例：两后端取值本就不同，
# 逐字节比对无意义。单独归类为 FLAKY（非逻辑差异），不计入真实失败。
FLAKY = {
    "time_random", "uuid_demo", "alias_demo", "threads", "server_demo",
    "async_demo", "time_demo", "random_demo",
    # 依赖本机实时状态：墙钟时间 / 进程表 / 随机监听端口
    "test_sched_lib", "test_process_lib", "server_selftest",
}


def should_skip(base: str) -> bool:
    """该示例是否应按设计跳过（不参与逐字节比对）。"""
    if base in SKIP:
        return True
    # GUI 程序化界面 / 需交互式 stdin 的示例：无法在非交互回归中复现。
    if base.startswith("guipro_"):
        return True
    if base.endswith("guide") or "interactive" in base or "input" in base or "stdin" in base:
        return True
    return False
