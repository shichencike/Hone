# 回归脚本共享的示例分类表。
#
# 为什么单独抽一个文件：regress3.py（解释器 vs VM）与 regress_ir.py（文本 IR 往返）
# 曾经各自维护一份 SKIP，结果两份逐渐漂移——regress_ir.py 少列了 import 依赖项，
# 于是 test_hone_lib / smoke_spider 等 6 个示例长期被误报为 DIFF（真实逻辑差异 = 0）。
# 分类表是「哪些示例不参与逐字节比对」的唯一事实来源，只应存在一份。

# 按设计无法在另一后端 1:1 复现的示例，直接跳过：
#   1) load / import 模块（VM 与 IR 均未覆盖，见 vm.rs:780 与 Hone虚拟机VM开发说明.md §10）
#   2) 原生 GUI / HTTP / FFI 示例、pet_demo 需交互或定时器会超时
# 说明：overload_demo（运算符重载 __op）已在 VM 实现，纳入回归比对。
SKIP = {
    "server_selftest", "spider_demo", "ffi_demo", "ffi_header", "ai_demo",
    "import_demo", "load_demo", "load_lazy", "pbar_demo",
    "pet_demo", "gui_demo",
    # 依赖 import 模块（后端未覆盖 import/load），属「按设计不支持」而非缺陷：
    "test_hone_lib", "test_img_lib", "test_music_lib", "test_sched_lib",
    "test_process_lib", "test_hone_lib_smoke",
    # smoke_spider 同样 import spider.hn，与 spider_demo 同类。
    "smoke_spider",
    # byte/bytes 与 type 实例 / with / 字段赋值是解释器特性，VM 按设计报 H999 兜底，
    # 无法 1:1 复现，故不参与逐字节比对（与 goto 在 AOT 的处理同一模式）。
    "byte_demo", "with_type_demo",
    # https_demo 访问外网（example.com / httpbin.org）：返回内容与长度随网络变化，
    # 两次运行本就可能不同，属非逻辑差异。纳入 SKIP 以保证基线可复现。
    "https_demo",
}

# 含随机数 / 时间 / 端口 / 线程完成顺序的示例：两后端取值本就不同，
# 逐字节比对无意义。单独归类为 FLAKY（非逻辑差异），不计入真实失败。
FLAKY = {
    "time_random", "uuid_demo", "alias_demo", "threads", "server_demo",
    "async_demo", "time_demo", "random_demo",
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
