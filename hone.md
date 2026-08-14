Hone 编程语言 – 完整设计规范 v1.1

项目代号：Hone
设计者：时辰刺客
许可证：MIT
目标定位：轻量级、跨平台（要支持termux）、可嵌入的脚本语言，兼具瑞士军刀式的实用性和玩具的易用性
设计哲学：效率至上、极简无前缀、不自举、不背兼容包袱、报错精准、开箱即用

快速入门（Quick Start）：从零到第一个脚本

新手请先读完本节（约 5 分钟）：安装 → 写第一个脚本 → 运行 → 基础语法速览。
详细语法与工具链见后续章节，本节只串起「能跑起来」的最小闭环。

第 1 步：安装（任选一种）

· 一键脚本（推荐）：
  · Linux / Termux：curl -fsSL https://github.com/shichencike/Hone/releases/latest/download/install.sh | sh
  · Windows（PowerShell 5.1+）：irm https://github.com/shichencike/Hone/releases/latest/download/install.ps1 | iex
· 手动下载：从 GitHub Releases 下载对应平台单文件二进制（Windows x86_64 / Linux x86_64 / Termux aarch64）
· 源码构建：cargo build --release，产物为 target/release/hone（Windows 下为 hone.exe）

验证安装：

    hone --version

第 2 步：写第一个脚本

新建 hello.hn，内容只有一行：

    print("Hello, Hone!");

第 3 步：运行

    hone hello.hn        # 输出 Hello, Hone!
    hone run hello.hn    # 等价（run 是默认命令）

第 4 步：基础语法速览

以下代码把变量、控制流、函数、集合串在一起，可直接保存为 tour.hn 运行：

    // 变量：无前缀声明，类型推导后锁定
    x = 10;               // int
    f = 3.14;             // float
    s = "hello";          // str
    b = true;             // bool

    // 控制流：条件必须是 bool
    if (x > 5) {
        print("x 大于 5");
    } else {
        print("x 不大于 5");
    }
    i = 0;
    while (i < 3) { i = i + 1; }

    // 函数：返回类型由 return 推导
    fn add(a, b) {
        return a + b;
    }
    print(add(2, 3));     // 5

    // 集合与遍历
    nums = [1, 2, 3];
    for v in nums { print(v); }

    // 字符串插值
    name = "Hone";
    print(f"你好, {name}!");

第 5 步：下一步

· 完整语言基础（类型/运算符/match/管道）：见「一、语言基础」
· 内置函数与各模块（sys/time/random/crypto/archive 等）：见「三、内置模块与功能」
· 多文件项目组织、调试、性能优化：见「十一、进阶用法」
· 常见坑（try-catch 变量不可见、append 返回新列表等）：见「十二、FAQ 与已知问题」
· 更多示例：examples/ 目录（hello.hn、fib.hn、control.hn、gui_demo.hn 等）

一、语言基础

1.1 基础信息

· 名称：Hone
· 文件扩展名：.hn
· 核心实现语言：Rust
· 分发形式：单文件可执行程序（< 10 MB）

1.2 语法风格

· 语句结束符：分号 ;
· 代码块：大括号 {}
· 缩进：强制使用 Tab 字符（\t）
· 注释：
  · 单行：// 注释
  · 多行：/* 注释 */

1.3 变量与类型系统

· 类型系统：静态强类型
  · 类型一经推导（x = 10 推导为 int；x = 3.14 推导为 float）或显式声明，终身锁定，不可变更
  · 类型间禁止隐式转换（如 int → float，float → int，int → str）
· 变量声明方式：
  · 自动推导：x = 10; → int；y = 3.14; → float
  · 显式类型（两种等价写法，可混用）：
    · C 风格：int x = 10; float y = 3.14;
    · Rust/TS 风格：x : int = 10; y : float = 3.14;
· 基本类型：
  · int：64位有符号整数
  · float：64位双精度浮点数（IEEE 754）
  · bool：布尔值（true / false）
  · str：UTF-8 字符串
· 字面量规则：
  · 整数：0，42，-1（无小数点）
  · 浮点数：必须包含小数点，如 3.14，-0.5，.2，2.0
  · 布尔：true，false
  · 字符串：双引号括起，如 "hello"，支持 \n，\t，\\，\"

1.4 控制流

· 条件分支：if (条件必须是 bool) { ... } else { ... }
· 循环：while (条件必须是 bool) { ... }
· 条件表达式必须显式返回 bool，禁止隐式转换（如 if (1) 视为非法）

1.5 函数定义

· 语法：fn 函数名(参数1, 参数2) { ... return 返回值; }
· 参数类型可由调用上下文推导，也可显式声明（推荐在复杂场景显式声明）
· 函数返回值类型由 return 语句推导，也可在函数首行用 -> type 声明（可选）
· 若推导失败或无返回语句，默认返回 void（但 Hone 中函数调用可作为表达式，无返回值时返回 null 占位）
· 泛型：fn 函数名[T, U](参数) { ... } —— 在函数名后、参数列表前用 [T, U] 声明类型参数
  · 参数与返回类型注解可引用类型参数（x: T、-> T），同一类型参数处调用点必须传同型
  · 调用时按实参自动推导类型参数（identity(42) → T=int；identity("hi") → T=str），
    同一泛型函数可以不同类型多次调用，互不锁定
  · 编译期擦除：运行期与普通函数零差异，无运行时开销、不产生重复代码
  · 函数名全局唯一，泛型函数与普通函数同名仍报「already defined」
  · 未声明的类型参数（x: T 而 fn 未写 [T]）报 H002；重复类型参数报 H005
  · 类方法同样支持泛型（class 中 fn 方法[T](...)）
· 示例：

fn identity[T](x: T) -> T {
    return x;
}
print(identity(42));          // 42（T=int）
print(identity("hello"));     // hello（T=str）

fn swap[A, B](a: A, b: B) -> B {
    return b;
}
print(swap(1, "two"));        // two（A=int, B=str）

fn pick[T](a: T, b: T, want_first: bool) -> T {
    if (want_first) {
        return a;
    }
    return b;
}
print(pick("a", "b", true));  // a（T=str）
print(pick(10, 20, false));   // 20（T=int）

1.6 命名风格

· 无前缀声明：变量不需要 let、var、const 等修饰符
· 命名空间访问：通过点号 别名.函数名 调用，如 sys.msgbox()
· 不支持 :: 多层路径前缀，所有函数扁平化存在于全局符号表

1.7 结构体（struct）

· 语法：struct 名称 { 字段: 类型, ... };
· 用于声明确定的数据形态（字段名与类型固定），实例用 dict 表示，字段访问 p.字段
· 构造：名称(值1, 值2, ...)，按字段顺序传参；检查阶段校验字段个数与类型（H001/H011）
· 运行时字段访问校验字段存在性（未知字段报 H002），实例可作为 dict 使用（keys/values 等）
· 示例：

struct Point { x: int, y: float };
p = Point(3, 2.5);
print(p.x);   // 3
print(p.y);   // 2.5

1.8 class 类（成员函数不进全局符号表）

· 语法：class 名称 { fn 方法(参数) { ... } ... }
· 作用：将一组相关函数组织成命名空间，通过 类名.方法(...) 调用（如 Math.double(21)）
· 关键特性：成员函数不进入全局符号表——
  · 不能用裸名调用：`double(21)` 报 H002（undefined function），必须写 `Math.double(21)`
  · 不污染全局命名空间：同名全局函数可与类方法共存，互不影响
  · 类内方法互相调用也要用限定名：`return Math.fib(n - 1) + Math.fib(n - 2);`
· 类方法支持参数、返回值（可写 -> type 注解）、递归、return；与普通函数能力一致
· 类名与 struct 名不能重复（报 H005）；类方法同样全局唯一（同类内重名报错）
· 结尾分号可选（class 是块结构，与 struct 不同）
· 示例：

class Math {
    fn double(x) {
        return x * 2;
    }
    fn fib(n) {
        if (n <= 1) {
            return n;
        }
        return Math.fib(n - 1) + Math.fib(n - 2);
    }
    fn greet(name) -> str {
        return "Hello, " + name;
    }
}

print(Math.double(21));   // 42
print(Math.fib(10));      // 55
print(Math.greet("hone"));// Hello, hone

fn double(x) {            // 全局同名函数与类方法共存
    return "global: " + to_str(x);
}
print(double(21));        // global: 21
print(Math.double(21));   // 42（类方法不受影响）

1.9 模式匹配（match）

· 语法：match 表达式 { 模式 => 分支体, ..., _ => 默认值 }
· 模式支持字面量（整数/浮点/布尔/字符串）与 `_` 通配符（匹配任意值，只能出现一次且放最后）
· match 是表达式，返回匹配分支的值；所有分支都不匹配时运行时报错（建议补 `_` 兜底）
· 各分支类型可以不同，返回值为动态类型
· 示例：

s = match 2 {
    1 => "one",
    2 => "two",
    _ => "other",
};
print(s);   // two

1.10 管道操作符（|>）

· 语法：x |> f  等价于 f(x)；x |> f(a, b)  等价于 f(x, a, b)
· 左侧表达式作为第一个参数传入右侧函数调用，可链式：a |> f |> g  等价于 g(f(a))
· 是语法糖，在解析期转换为普通函数调用
· 示例：

print("hello world" |> len);      // 11
print([1, 2, 3] |> len |> to_str); // "3"
print(3 |> max(7) |> to_str);      // "7"

二、工具链与命令

Hone 提供完整的命令行工具链，所有功能集成在单文件 hone（或 hone.exe）中：

命令 功能说明
hone run <script.hn> 执行 Hone 脚本（默认命令，支持 --restart/--resume）
hone fmt [options] <file.hn> 代码格式化（统一 Tab 缩进、运算符空格、大括号位置）
hone fmt -w *.hn 直接覆盖写入源文件
hone test [目录] 递归扫描 *.test.hn 测试文件，运行并汇总 PASS/FAIL（配合 assert 断言）
hone build --dll <script.hn> 将脚本打包成 C ABI 动态库（DLL / SO / DYLIB）
hone build --exe <script.hn> 打包独立可执行文件（解释器 + 脚本自释放，[-o <out>] [--icon <ico>]）
hone build --script <script.hn> 生成仅脚本压缩包 .hzp（不内嵌解释器，[-o <out>]，用 hone run 执行）
hone bind <header.h> 从 C 头文件生成 typed load 签名块（FFI 自动绑定）
hone debug <script.hn> 断点调试模式（支持 breakpoint 关键字）
hone get <module> 远程下载模块依赖并缓存到本地
hone self-update [url] 从 URL 下载最新 hone 二进制并替换当前程序（也可用环境变量 HONE_UPDATE_URL）
hone explain <code> 查看错误码解释与修复建议（如 hone explain H201）
hone lsp 启动语言服务器（代码补全、跳转定义）
hone poop <file.hn> 屎山检测（if 嵌套深度 + 圈复杂度）
hone --help / --version 帮助信息 / 版本信息

进度条策略：

· 仅用于 hone build --dll（编译过程可能有等待）和 hone get（网络下载）
· 使用 Rust 标准库的 print!("\r") 实现轻量进度显示，不引入第三方 TUI 库

三、内置模块与功能

3.1 基础内置函数（直接可用，无需导入）

· print(value)：输出到标准输出，自动换行
· read_file(path) → str：读取文本文件内容
· write_file(path, content)：写入文本文件
· file_exists(path) → bool：检查文件是否存在
· len(value) → int：返回字符串长度（字节数）、列表/字典元素个数
· type_of(value) → str：返回变量类型名称（"int"、"float"、"bool"、"str"、"list"、"dict"、"null"、"error"、"ptr"）
· http_get(url) → str：发送 HTTP GET 请求，返回响应体（支持 http:// 与 https://，TLS 为纯 Rust 实现、内置 Mozilla 根证书）
· http_post(url, body) → str：发送 HTTP POST 请求（body 为字符串，支持 http:// 与 https://）
· json_parse(str) → value：将 JSON 字符串解析为 Hone 值（自动映射为 int/float/bool/str）
· json_stringify(value) → str：将 Hone 值序列化为 JSON 字符串

3.2 sys 模块（系统功能封装）

内置常用系统 API 封装（Windows 优先，其他平台尽力模拟或报错）：

· sys.msgbox(title, message, style)：系统消息弹窗（style 为 "info"/"warn"/"error"）
· sys.run(cmd)：执行系统命令（返回输出字符串）
· sys.get_env(key) → str：获取环境变量
· sys.reg_read(key) → str：读取注册表（Windows 专用）
· sys.reg_write(key, value)：写入注册表（Windows 专用）
· sys.clipboard_set(text)：复制文本到剪贴板
· sys.beep(freq, duration)：播放系统提示音（频率 Hz，持续时间 ms）
· sys.get_screen_size() → (width, height)：获取屏幕尺寸（返回两个 int）

实现方式：Rust std + winapi（仅 Windows），跨平台部分用 std 模拟，总增量 < 500 KB。

3.3 time 模块（时间操作）

· time.now() → int：返回当前 Unix 时间戳（秒，整数）
· time.sleep(seconds)：暂停当前线程执行，seconds 可为整数或浮点数（如 0.5）
· time.format(timestamp, format) → str：将时间戳格式化为字符串，格式占位符：
  · YYYY：四位年份
  · MM：两位月份（01–12）
  · DD：两位日期（01–31）
  · HH：两位小时（00–23）
  · mm：两位分钟（00–59）
  · SS：两位秒数（00–59）
  · WW：ISO 星期几（1=周一 … 7=周日）
· time.parse(str) → int：解析时间戳字符串（YYYY-MM-DD、YYYY-MM-DD[T ]HH:MM:SS，
  可选小数秒与 ±HH[:MM] 时区），失败报错
· time.add(timestamp, seconds) → int：时间戳算术（秒，可为负，溢出报 H010）
· time.diff(a, b) → int：时间差 a - b（秒）
· time.weekday(timestamp) → int：ISO 8601 星期几（1=周一 … 7=周日）

示例：


t = time.now();
print(t);                  // 1698765432
time.sleep(1.5);
print(time.format(t, "YYYY-MM-DD HH:mm:SS")); // 2023-10-31 14:30:45
print(time.weekday(time.parse("2024-08-09T00:00:00Z"))); // 5（周五）
print(time.add(time.parse("2024-01-01"), 86400));         // 次日 00:00 UTC


3.4 random 模块（随机数生成）

· random.int(min, max) → int：返回闭区间 [min, max] 内的随机整数（含两端）
· random.float() → float：返回 [0.0, 1.0) 范围内的随机浮点数（双精度）

示例：


x = random.int(1, 100);
print(x);                   // 42
r = random.float();
print(r);                   // 0.873245


3.5 字符串处理函数

· str_contains(str, substr) → bool：判断 str 是否包含 substr
· str_replace(str, old, new) → str：将 str 中的所有 old 替换为 new
· str_trim(str) → str：去掉 str 首尾的空白字符（空格、Tab、换行）

示例：


s = "  hello world  ";
print(str_trim(s));           // "hello world"
print(str_contains(s, "world")); // true
print(str_replace(s, "world", "Hone")); // "  hello Hone  "


3.6 数学工具函数

· abs(x) → int或float：返回数值的绝对值（类型与输入相同）
· max(a, b) → int或float：返回较大值（要求 a、b 类型相同）
· min(a, b) → int或float：返回较小值（要求 a、b 类型相同）

示例：


print(abs(-5));    // 5
print(max(3.14, 2.71));  // 3.14
print(min(3, 7));  // 3


3.7 类型转换函数

· to_str(value) → str：将 int、float 或 bool 转换为字符串（bool → "true"/"false"，浮点数按默认格式输出）
· to_int(value) → int：将 str（纯数字）或 float 转换为 int（截断小数部分），若 str 包含非数字字符则报错 error[H006]
· to_float(value) → float：将 str（数字格式）或 int 转换为 float，若 str 格式非法则报错 error[H007]

示例：


age = 25;
print("年龄：" + to_str(age));    // "年龄：25"
print(to_int(3.99));            // 3
print(to_float("2.718"));       // 2.718
print(to_int("abc"));           // 抛出 H006
print(to_float("xyz"));         // 抛出 H007


3.8 go 关键字（多线程并发）

· 语法：go 函数名(参数);
· 行为：每次调用启动一个独立的 Rust 线程（std::thread::spawn）
· 每个线程拥有独立的符号表副本，不共享变量
· 只传递值类型（int、float、bool、str 的副本），不传递引用
· 子线程崩溃仅打印错误，不影响主线程

3.9 breakpoint 关键字（断点调试）

· 仅在 hone debug 模式下生效
· 执行到 breakpoint; 时，暂停程序，打印当前作用域所有变量的快照
· 快照格式：[Hone Debug] 断点触发 -> 文件.hn:行号
  --- 变量快照 ---
  变量名 : 类型 = 值
  变量名 : 类型 = 值
· 暂停后等待用户按 Enter 继续，或按 Ctrl+C 退出
· 不提供交互式查询命令（p x 等），全部直接输出

3.10 集合操作与断言

· append(list, value) → list：返回追加了 value 的新列表（列表是值类型，需配合 `l = append(l, x)` 使用）
· clone(value) / copy(value) → value：深度拷贝（递归复制 list/dict，副本的后续修改不影响原值）
· contains(list|str, value) → bool：列表是否包含某值 / 字符串是否包含子串
· index_of(list, value) → int：元素位置（不存在返回 -1）；keys(dict) / values(dict) / has_key(dict, key) 字典操作
· is_int / is_float / is_bool / is_str / is_list / is_dict / is_null(value) → bool：类型判断
· assert(条件[, 消息])：条件为 false 时抛 error[H700]（测试框架 hone test 配合使用）

3.11 args 命令行参数模块

· args.get(key) → str 或 null：获取命令行参数值（--key value 格式）
· args.get(key, type[, default]) → value：按期望类型转换，key 不存在时返回 default（缺省 null）
  · type 支持 int / float / bool / str（可直接写 `args.get("port", int, 8080)`，类型关键字在表达式位置等价于其名称字符串）
  · 转换失败报 H006（int）/ H007（float）/ H001（bool），键缺失且无默认值时返回 null
· args.has(key) → bool：命令行是否包含该参数

3.12 server 本地 HTTP 服务器模块

· server.listen(port) → int：绑定 127.0.0.1 启动后台监听线程，返回实际端口（0=自动分配）
· server.poll() → str：取出排队请求，返回 JSON 数组 [{id,method,path,body}, ...]
· server.respond(id, body[, status]) → bool：发送响应体，可指定 HTTP 状态码（默认 200，如 404/500，范围 100..=599）
· 事件模型：后台线程只做 TCP 收发与请求排队，脚本主线程轮询响应，与解释器单线程模型兼容

3.13 crypto 加密与哈希模块

· crypto.md5(str) → str：MD5 十六进制摘要
· crypto.sha1(str) → str：SHA-1 十六进制摘要
· crypto.sha256(str) → str：SHA-256 十六进制摘要
· crypto.hmac_sha256(key, msg) → str：HMAC-SHA256 十六进制（密钥与消息均为字符串）
· crypto.base64_encode(str) → str：Base64 编码
· crypto.base64_decode(str) → str：Base64 解码（输入非法报 H001）

3.14 archive 压缩与归档模块

· archive.zip_list(path) → list：列出 zip 条目名
· archive.zip_read(path, entry) → str：读取 zip 中指定条目的文本
· archive.zip_extract(path, dir) → int：解压 zip 到目录，返回文件条目数
· archive.zip_create(path, entries) → bool：从 dict {条目名: 内容} 创建 zip
· archive.tgz_list / tgz_read / tgz_extract / tgz_create：同上，针对 tar.gz
· 安全：解压时拒绝绝对路径与 `..` 穿越条目（防 zip-slip）

3.15 ptr 指针类

· ptr.alloc(size) → ptr：分配 size 字节内存（对齐 8），失败返回 0
· ptr.free(p) → bool：释放由 ptr.alloc 分配的内存
· ptr.is_null(p) → bool / ptr.is_valid(p) → bool / ptr.size(p) → int：查询
· ptr.read_int/read_float/read_byte(p, offset) → value：按偏移读取 8 字节整数 / 8 字节 double / 1 字节
· ptr.write_int/ptr.write_float/ptr.write_byte(p, offset, v)：对应写入（write_byte 值域 0..=255）
· 安全模型（防野指针）：分配表跟踪 —— 未分配、已释放（use-after-free）、重复释放（double-free）报 H304，
  越界访问报 H305，空指针读写报 H304；外部 FFI 句柄不在分配表中，free/read/write 拒绝操作

3.16 plugin 插件系统

· plugin.load(path, alias) → bool：运行期加载动态库并注册（之后可用 `alias.函数(...)` 调用，走 C ABI 通道）
· plugin.has(alias) → bool：查询插件是否已注册
· plugin.list() → list：列出已注册插件 [{name, path}, ...]
· plugin.unload(alias) → bool：注销插件
· 与 load 语句的区别：load 是编译期声明 + 静态检查；plugin.* 是运行期动态注册，二者调用链相同

3.17 csv 数据处理

· csv.parse(text) → list：解析 CSV 文本为行列表（每行是 str 列表），支持引号包裹、"" 转义、字段内逗号/换行、CRLF
· csv.parse_dict(text) → list：解析 CSV 文本为 dict 列表（首行为表头，后续行按列名取值）
· csv.stringify(rows) → str：将行列表（list of list / list of dict）序列化为 CSV 文本（自动转义）

示例：


rows = csv.parse("name,age\nAlice,30\n");
for r in rows { print(r); }        // [name, age] / [Alice, 30]
d = csv.parse_dict("name,age\nAlice,30\n");
for x in d { print(x.name); }      // Alice

3.18 glob / temp 系统工具

· glob.match(pattern, path) → bool：判断路径是否匹配 glob 模式
  （* 单层任意、? 单字符、** 跨目录、[abc]/[a-z] 字符类，路径分隔符统一按 /）
· glob.list(pattern) → list：递归列出匹配模式的文件相对路径（排序后返回）
· temp.dir([prefix]) → str：创建唯一临时目录并返回路径（系统临时目录下）
· temp.file([prefix]) → str：创建唯一临时文件并返回路径
· temp.remove(path) → bool：删除临时文件/目录（不存在或失败返回 false）

示例：


print(glob.match("src/**/*.rs", "src/main.rs")); // true
files = glob.list("examples/*.hn");
td = temp.dir("hone-");
write_file(path.join(td, "a.txt"), "x");
print(temp.remove(td));            // true

3.19 zlib / gzip 压缩

· zlib.compress(text) → str：zlib 压缩（RFC 1950），结果以 base64 返回
· zlib.decompress(b64) → str：zlib 解压（输入为 zlib.compress 的 base64 输出）
· zlib.gzip(text) → str：gzip 压缩（含文件头），结果以 base64 返回
· zlib.gunzip(b64) → str：gzip 解压

说明：Hone 的 str 无法直接承载二进制，压缩结果统一 base64 编码；解压输入即压缩输出。

示例：


c = zlib.compress("hello hello");
print(zlib.decompress(c));         // hello hello
g = zlib.gzip("data");
print(zlib.gunzip(g));             // data

3.20 stat 统计 / matrix 矩阵运算

· stat.sum(nums) → int|float：求和（纯 int 返回 int）
· stat.mean(nums) → float：算术平均值
· stat.median(nums) → float：中位数
· stat.variance(nums) → float：总体方差（需 ≥ 2 个元素）
· stat.stddev(nums) → float：总体标准差
· stat.min(nums) / stat.max(nums) → number：最小值 / 最大值
· matrix.identity(n) → list：n×n 单位矩阵
· matrix.transpose(m) → list：转置
· matrix.add(a, b) → list：矩阵相加（形状必须相同）
· matrix.mul(a, b) → list：矩阵乘法（A 列数必须等于 B 行数）
· matrix.scale(m, k) → list：矩阵标量乘法

矩阵以「列表的列表」表示：[[a, b], [c, d]]。

示例：


nums = [1.0, 2.0, 3.0, 4.0, 5.0];
print(stat.mean(nums));            // 3
print(stat.stddev(nums));          // 1.4142...
m = [[1.0, 2.0], [3.0, 4.0]];
print(to_str(matrix.mul(m, m)));   // [[7,10],[15,22]]

3.21 diff 文本对比 / regex 增强

· diff.lines(a, b) → list：逐行 LCS 对比，返回操作列表 [{op, line}]（op 为 "-"/"+"/" "）
· diff.unified(a, b) → str：生成 unified diff 文本（@@ 块头 + -/+ 行）
· regex.find(pattern, text) → list：返回所有非重叠匹配的子串
· regex.groups(pattern, text) → list：返回首个匹配的捕获组（第 0 项为整体匹配，未参与组为 null；无匹配返回空列表）
· regex.split(pattern, text) → list：按正则拆分文本

示例：


ops = diff.lines("a\nb\n", "a\nc\n");
print(ops[0].op);                  // " "
d = diff.unified("x\n", "y\n");
print(to_str(regex.find("a+", "aaa b")));   // ["aaa"]
print(to_str(regex.groups("(\\d+)-(\\d+)", "2024-08"))); // ["2024-08","2024","08"]
print(to_str(regex.split("[,;]", "a,b;c"))); // ["a","b","c"]

3.22 HTTP Client 增强（http.request）

· http_get(url) → str：GET 请求（默认超时 15 秒，UA: hone/0.1.0）
· http_post(url, body) → str：POST 请求（body 为字符串）
· http.request(url, opts) → str：通用请求，opts 为 dict：
  · method：请求方法（默认 "GET"，可 "POST"/"PUT"/"DELETE" 等）
  · headers：自定义请求头 dict（可覆盖 User-Agent / Content-Type）
  · body：请求体字符串（method 为 POST 等时常用）
  · timeout：超时秒数（int/float，默认 15）

示例：


r = http.request("https://api.example.com/data", {
    "method": "POST",
    "headers": {"User-Agent": "my-app/1.0", "Content-Type": "application/json"},
    "body": json_stringify({"q": "hone"}),
    "timeout": 10
});
print(r);

3.23 smtp 发邮件 / ws WebSocket

· smtp.send(host, port, opts) → bool：发送邮件，opts 为 dict：
  · from：发件人（必填）；to：收件人（字符串或字符串列表，必填）
  · subject / body：主题与正文
  · user / password：提供时启用 AUTH LOGIN 认证
  · starttls：是否 STARTTLS 升级（默认 true；port 465 为隐式 TLS，自动跳过 STARTTLS）
· ws.request(url, message[, timeout]) → str：WebSocket 一次性请求-响应
  · 建立连接 + 握手（校验 Sec-WebSocket-Accept），发送一个文本帧，
    读取服务端文本帧直到 close 帧或超时（默认 30 秒），返回拼接文本
  · 支持 ws:// 与 wss://（TLS，纯 Rust rustls）

示例：


smtp.send("smtp.example.com", 587, {
    "from": "a@example.com",
    "to": "b@example.com",
    "subject": "Hi",
    "body": "Hello from Hone",
    "user": "a@example.com",
    "password": "secret"
});
reply = ws.request("wss://echo.websocket.org/", "ping");

3.24 plot 绘图 / yaml 数据格式

· plot.bar(values[, labels]) → str：生成 SVG 柱状图（values 为数值列表，labels 可选）
· plot.line(xs, ys) → str：生成 SVG 折线图（xs/ys 为等长数值列表，含网格线与数据点）
· yaml.parse(text) → value：解析 YAML 子集为 Hone 值（map/list/标量/注释/引号字符串/嵌套缩进）
· yaml.stringify(value) → str：将 Hone 值（dict/list/标量）序列化为 YAML 文本

说明：plot.* 返回 SVG 字符串，用 write_file 保存为 .svg 即可在浏览器查看；
yaml 支持常用配置子集（锚点/别名/多文档/流式 [] {} 不在支持范围）。

示例：


svg = plot.bar([3.0, 1.0, 4.0], ["a", "b", "c"]);
write_file("chart.svg", svg);
cfg = yaml.parse("name: hone\nversion: 1.0\nok: true\n");
print(cfg.name);                 // hone
print(yaml.stringify({"a": 1, "b": "x"}));

四、导入与外部集成

4.1 load 动态库加载

· 语法：load "绝对路径"; 或 load lazy "绝对路径";
· 必须填写完整绝对路径（操作系统原生格式，如 C:\path\to\lib 或 /home/user/lib.so）(如果没有用绝对路径，用名字的话,则是是这个项目的同目录)
· 普通 load：在第一次调用时加载整个库
· load lazy：函数级懒加载，仅加载被实际调用的函数及其依赖链
· 找不到路径或符号时抛出 error[H404] / error[H100]

4.1.1 typed FFI 签名块（v0.4+）

· 语法：load "路径" as 别名 { fn 函数名(参数: 类型, ...) -> 返回类型; ... }
· 签名块显式声明 C ABI 参数与返回类型，调用时按声明精确转换，不再限制为 int64 单通道
· 支持类型：int → int64_t，float → double，bool → C bool，str → const char*，ptr → void*
· 返回类型额外支持 void；参数最多 8 个；回调（fn(...)）与可变参数（...）暂不支持
· 签名块要求 as 别名（作为调用前缀），以 } 结尾无需分号
· 静态检查：签名块声明的函数调用会在检查阶段校验参数个数与类型（H001/H005），
  返回类型也参与类型推导（例如 m.cos(0.0) 的类型为 float）
· 旧约定（全 int64 参数/返回值）不受影响，未声明签名的库函数仍按 int64 通道调用

示例（调用 libm 数学库）：

```
load "libm.so.6" as m {
    fn cos(x: float) -> float;
    fn pow(x: float, y: float) -> float;
}
print(m.cos(0.0));     // 1
print(m.pow(2.0, 10)); // 1024
```

· ptr 返回值可直接传给下一个 ptr 参数（句柄传递），0 可作 NULL；`p == 0` 可判断空指针
· str 参数按 UTF-8 转 C 字符串（含 NUL 字节会报错）；str 返回值由 CStr 读取
· 完整示例：examples/ffi_demo.hn（配合 tests/hone_lib 测试库）

4.1.2 从头文件自动绑定（from "header.h" + hone bind，v0.4+）

· 语法：load "路径" as 别名 from "头文件.h";  从 C 头文件提取函数原型自动生成 FFI 签名
· 用法一（运行时自动绑定）：

```
load "libm.so.6" as m from "/usr/include/math.h";
print(m.cos(0.0));   // 类型来自头文件：cos(double) -> double → float
```

· 用法二（离线生成签名块，粘贴进脚本）：`hone bind <header.h>` 输出 load 签名块到 stdout
· 解析能力（纯 Rust 受限子集，无 libclang 依赖）：
  - 跳过注释、预处理行（#...）、struct/enum/union 定义体与 extern "C" 裸块
  - 类型映射：int/long/short/size_t 等 → int；float/double → float；bool/_Bool → bool；
    char* / const char* → str；其余指针（void*/struct X*/句柄）→ ptr；void → void
  - 简单 typedef 展开（如 sqlite3_int64 → long long int → int）
  - 属性宏跳过（__attribute__((...)) / __declspec / 全大写前缀宏如 SQLITE_API）
· 不支持的原型（回调 fn(...)、变参 ...、数组参数、结构体按值、long double）会以
  unsupported 标记，调用时直接报错（而非 ABI 崩溃）；hone bind 输出中列为注释
· 与签名块可混用：from 头文件签名先注册，签名块中的同名声明覆盖之
· 完整示例：examples/ffi_header.hn（头文件见 tests/hone_lib/hone_lib.h）

4.2 use 命名空间导入

· 语法：use 命名空间;
· 用于调用 Rust 宿主注册的原生函数（如 use std_io;）

4.3 import 远程模块下载

· 语法：import "模块名" from "URL";
· 运行时检测到该声明，从指定 URL 下载模块并缓存到本地（~/.hone/cache/）
· 后续运行直接使用缓存，不重复下载
· 底层基于 TCP/TLS，由 http_get 实现下载（模块源 URL 支持 http:// 与 https://）

4.4 别名（Alias）

· 支持 as 子句：load "path" as lib;
· 支持二次重命名：alias 原名称 as 新名称;
· 原名支持点号路径（模块/类/内置点号函数）：alias time.now as tnow; 之后可 tnow(...) 调用
· 别名可叠加（别名再起别名），可指向内置函数：alias print as p;
· 别名作用域为文件级（当前 .hn 文件全局可见）

4.5 DLL 打包（hone build --dll）

· 将 .hn 脚本打包成标准 C ABI 动态库（DLL / SO / DYLIB）
· 使用 @export 标记要导出的函数
· 内置 TinyCC（约 200 KB）作为 C 编译器，无需用户安装外部工具链
· 生成的动态库自包含，不依赖 Hone 解释器（内嵌微型运行时）
· 类型映射：int → int，float → double，bool → bool，str → const char*
· 错误时返回错误码，并写入错误缓冲区

五、错误处理与报错风格

5.1 报错风格

· 继承 Rust 的精准定位能力
· 格式：error[Hxxx]: 描述信息
  --> 文件名.hn:行号:列号
  行号 | 代码片段
  |    ^^^^ 错误标记
  help: 建议修复方案
· 解析期错误一次性全部报告，不逐条停
· 报错信息为纯英文，保持机器可读性和可搜索性

5.2 错误码体系（部分示例）

· H001：类型冲突（期望类型 X，得到类型 Y；含泛型同一类型参数处实参类型不一致）
· H002：未定义的变量或函数（含：类方法裸名调用——成员函数不进全局符号表；泛型注解引用了未声明的类型参数）
· H003：无法自动推导类型，请添加显式类型
· H004：运算符重载歧义（如 + 可能为 int 或 str 时）
· H005：语法错误（含：类型参数重复声明、类名与 struct 名重复、类成员非 fn 定义）
· H006：字符串转换为整数失败（非数字内容）
· H007：字符串转换为浮点数失败（格式非法）
· H008：条件表达式必须是 bool
· H009：除零错误
· H010：整数溢出
· H011：参数数量不匹配（含 struct 构造、类方法调用）
· H012：递归过深（超过 5000 层）
· H100：动态库加载失败
· H110：懒加载依赖函数未找到
· H200：网络请求失败（http_get / http_post / http.request / smtp.send / ws.request）
· H201：网络超时（http.request 的 timeout、ws.request 超时）
· H202：连接被拒绝
· H203：DNS 解析失败
· H204：HTTP 非 2xx 状态码
· H300：系统调用失败（含文件扫描、临时目录等）
· H301：DLL 加载失败（含 sqlite 库缺失：Linux 安装 libsqlite3，Windows 放置 sqlite3.dll）
· H302：DLL 参数校验失败
· H303：权限不足
· H304：野指针（未分配/已释放/重复释放/空指针，ptr 类）
· H305：指针越界访问（超出 ptr.alloc 分配大小）
· H401：文件不存在
· H402：文件权限不足
· H403：文件被占用/锁定
· H404：文件或库不存在
· H600：用户主动抛出（throw）
· H700：assert 断言失败（测试框架）

5.3 报错原则

· 禁止出现无效行号（如“第 1347 行”指向与错误无关的位置）
· 运行时错误（除零、文件不存在等）也按相同格式输出
· 禁用 panic! 堆栈直接展示给用户，统一封装为 error[Hxxx] 格式

5.4 断点与调试

· 使用 breakpoint; 关键字，仅在 hone debug 模式下生效
· 暂停执行并打印当前作用域所有变量的完整快照
· 等待用户按 Enter 继续或 Ctrl+C 退出，不提供交互式命令

六、性能与打包

6.1 体积目标

· 单文件可执行程序 < 10 MB
· 目标范围：5–8 MB（含 TinyCC 约 200 KB）

6.2 编译优化

Rust Cargo.toml 配置：

toml
# 实测（2026-08-14）：fat LTO + 单 codegen unit 在 Windows 本机编译最快（约 4min）且体积最小（约 3.7MB）；
# thin LTO 的两种组合（cgu=1 / cgu=16）反而更慢（4min47s+）且更大（4.0MB+），故保留此配置。
[profile.release]
lto = true
opt-level = "z"
strip = true
debug = false
panic = "abort"
codegen-units = 1


6.3 依赖控制

· 核心依赖：仅 std + libloading（动态库加载）
· 网络：ureq（轻量 HTTP，约 200 KB）
· JSON：serde_json（约 300 KB）
· 图像：image crate（约 200 KB，用于 PNG 导出）
· 绘图：minifb 或 pixels（可选，用于图形化库）
· 禁止引入 tokio、serde（完整版）等大体积库

6.4 跨平台支持

· 支持目标平台：
  · Windows（x86_64-pc-windows-gnu）
  · Linux（x86_64-unknown-linux-gnu）
  · macOS（x86_64-apple-darwin / aarch64-apple-darwin）
  · Android（Termux，aarch64-linux-android）
· 编译方式：Rust 交叉编译，单一源码树
· 静态链接，无外部运行时依赖

七、版本策略与发布

7.1 版本兼容性

· 不向后兼容：新版本不保证旧版本代码能直接运行
· 不强制升级：旧版本解释器永久可用
· 废弃机制：废弃功能仅标记 @deprecated 并输出警告，不删除
· 文档同步：每个版本发布时提供变更说明和迁移指南

7.2 发布渠道

· 代码仓库：GitHub（shichencike/Hone）
· 官网：https://hone.xo.je
· 许可证：MIT
· 版本命名：v0.1.0、v0.2.0 … 遵循语义化版本
· Release 附件：各平台预编译二进制文件

7.3 发布内容

· 源码（GitHub 仓库）
· 预编译二进制（Windows / Linux / macOS / Android）
· 完整手册（官网 + 仓库内 Markdown）
· 示例代码库
· 可视化编辑器（方案 A：独立网页）

7.4 版本说明示例

Hone v0.1.0 – 初始版本

· 支持基础语法：变量、if/while、函数
· 类型：int、float、bool、str
· 内置函数：print、read_file、http_get、json_parse、time、random、字符串处理、数学、类型转换
· 工具链：hone fmt、hone build --dll
· 调试支持：breakpoint
· 跨平台：Windows / Linux / macOS / Android

八、开发路线图（优先级排序）

阶段 1：核心执行器（当前优先）

· 词法分析器（Lexer）
· 语法解析器（Parser）
· 抽象语法树（AST）
· 虚拟机（VM）或 AST 解释执行
· 符号表与类型检查（含 float 支持）
· 内置函数：print、变量、if/while、函数定义
· 命令行入口：hone run <script.hn>

阶段 2：工具链完善

· hone fmt：代码格式化（Tab 缩进、大括号位置、运算符空格）
· breakpoint：断点调试
· hone build --dll：DLL 打包（集成 TinyCC）
· sys 模块（Windows 常用 API 封装）
· go 关键字（多线程）
· time、random、字符串处理、数学、类型转换函数

阶段 3：扩展与集成

· import 远程模块下载
· load lazy 懒加载机制
· 可视化编辑器（方案 A：独立网页拖拽生成代码）
· 进度条优化（仅用于 build 和 get）

阶段 4：发布与生态

· GitHub 仓库开源（含 LICENSE、README、示例）
· 官网 https://hone.xo.je 搭建（含文档、下载、教程）
· B站视频发布（展示 Hone 的设计与使用）
· 简历整合（Hone 作为完整项目作品）

九、核心设计原则与约束

1. 不自举：Hone 不用于开发自己的编译器，保持设计与实现的灵活性
2. 不兼容旧版本：不背兼容包袱，旧版本永久可用，新版本自由演化
3. 不强制升级：提供迁移工具但绝不强制用户迁移
4. 无前缀声明：变量不需要 let、var、const 等修饰符
5. 强制 Tab 缩进：消除空格与 Tab 的视觉混淆
6. 静态强类型：类型锁定，不隐式转换，报错精准
7. 单文件分发：用户只需下载一个可执行文件
8. 开箱即用：下载 → 解压 → 运行，零配置
9. 多版本共存：多个版本的 hone 可同时存在，互不干扰

十、示例代码（期望运行效果）

Hello World


// 这是 Hone 的第一个示例
print("Hello Hone!");


变量与类型


x = 10;          // 自动推导为 int
y : int = 20;    // 显式类型
z = x + y;
print(z);        // 30

f = 3.14;        // 自动推导为 float
print(to_str(f)); // "3.14"

// 类型锁定示例
x = "Hone";       // error[H001]: 期望 int，得到 str


控制流与函数


fn fib(n) {
    if (n <= 1) {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}

print(fib(10));  // 输出 55


调试与断点

bash
hone debug fib.hn



fn main() {
    x = 10;
    breakpoint;   // 触发调试暂停，打印所有变量快照
    print(x);
}


DLL打包


// math.hn
fn add(a, b) {
    return a + b;
}

@export add;


bash
hone build --dll math.hn


生成 math.dll，可在 C / Python / Rust 中调用。

多线程


fn task(id) {
    print("任务 " + to_str(id) + " 启动");
    i = 0;
    while (i < 1000000) {
        i = i + 1;
    }
    print("任务 " + to_str(id) + " 完成");
}

go task(1);
go task(2);
go task(3);
print("主线程继续执行");


时间与随机


t = time.now();
print(time.format(t, "YYYY-MM-DD HH:mm:SS"));

r = random.int(1, 100);
print("随机数: " + to_str(r));

print("随机小数: " + to_str(random.float()));


字符串处理


s = "  hello world  ";
print(str_trim(s));
print(str_contains(s, "world"));
print(str_replace(s, "world", "Hone"));


十一、进阶用法

11.1 多文件项目组织

Hone 单文件即可运行，但项目变大后建议拆分模块。三种复用方式：

· import 远程/本地模块（.hn 源码复用，最常用）：
  · import "模块名" from "URL";        // 远程：首次下载并缓存到 ~/.hone/cache/<模块名>.hn，之后离线使用
  · import "模块名" from "./路径/x.hn"; // 本地：相对当前工作目录直接读取，不写缓存
  · import "模块名" from "URL" as 别名; // 别名：模块函数前缀替换为别名前缀（见下）
  · hone get <script.hn>               // 预下载脚本中所有 import 声明的模块
· load / load lazy 动态库（C ABI 复用，见 4.1）：
  · load "绝对路径";                    // 调用 C 库函数（需绝对路径，或与脚本同目录的库名）
  · load lazy "绝对路径";               // 函数级懒加载，按需加载依赖链
  · load "路径" as m { fn ...; }       // typed 签名块：精确声明参数/返回类型
· use 命名空间：调用 Rust 宿主注册的原生函数（如 use std_io;）

import 的函数名前缀规则：模块内函数以「模块名_」为前缀注册
（例如 hone_lib/math.hn 中的 clamp → math_clamp，见 examples/test_hone_lib.hn）；
使用 as 别名后，模块名前缀整体替换为别名前缀。

推荐的项目目录结构：

    myproject/
      main.hn            # 入口：只做流程编排与参数解析
      libs/              # 自有模块（import "./libs/xxx.hn"）
      data/              # 数据文件
      vendor/            # 第三方 .hn 模块

注意事项：
· 函数扁平化存在于全局符号表（不支持 :: 多层路径、不支持嵌套遮蔽），
  模块函数靠前缀隔离命名空间，避免与主脚本函数重名（重名报「already defined」）
· import 的模块顶层语句会在加载时执行一次，可在模块顶部放常量初始化
· load 只用于 C 动态库；加载 .hn 源码一律用 import

11.2 调试复杂逻辑

· 断点调试：hone debug <script.hn>，配合 breakpoint; 关键字：
  · 运行到 breakpoint; 暂停，打印当前作用域全部变量快照
  · 按 Enter 继续，Ctrl+C 退出；断点仅在 debug 模式生效
· 条件输出：debug_print(expr); 仅在 hone debug 模式打印，普通运行自动跳过（可留在源码中）
· 临时函数：tmp fn 名称(...) { ... } 编译时自动忽略，适合开发期草稿
· 静态检查：hone 运行前先做类型检查，多数错误（H001 类型、H005 参数个数）在跑之前就报出
· 错误码解释：hone explain H201 查看任意错误码的含义与修复建议
· 复杂度分析：hone poop <file.hn> 输出 if 嵌套深度与圈复杂度，定位「屎山」热点
· 自动化测试：hone test [目录] 递归扫描 *.test.hn，配合 assert(条件[, 消息]) 断言，汇总 PASS/FAIL
· 无人值守：hone run --restart=3 --backoff=1,3,10 --restart-on=H200,H401 <script.hn>
  崩溃自动重启（仅对可重入错误）；hone run --resume 从上次 db 检查点继续（脚本变更后自动失效）

调试思路：
1. 先看类型：类型错误用 hone explain H001 确认期望/实际类型，检查变量是否被锁定
2. 缩小范围：在疑似出错的位置前后加 breakpoint;，观察变量快照
3. 抓运行时错误：用 try { } catch e { } 捕获，打印 e.code / e.message / e.line / e.context
4. 用 assert 固化关键不变量，配合 hone test 防止回归

11.3 性能优化

· 避免循环内反复 append：列表是值类型，append 返回新列表（整体拷贝），
  循环里 l = append(l, x) 是 O(n²)。需要累积时：
  · 先确定规模再一次性构造；或先用 dict 累积（键值对），最后转列表
  · 例：统计词频用 dict（has_key + 自增），不要用列表 append + contains
· 字符串拼接优先 f-string：f"..." 比 + 拼接更简洁；大量拼接场景注意 str 不可变、每次 + 都生成新串
· 减少深拷贝：clone / copy 是递归深拷贝，大集合频繁拷贝有成本；只在需要独立副本时使用
· 热点计算交给 C：性能瓶颈函数可用 load 调用 C 库（FFI），或用 hone build --dll 导出后由 C 调用
· 并行化：go 函数名(参数); 启动独立线程（值传递、不共享变量），
  适合可拆分的独立任务（如批量请求）；线程间通信走文件 / HTTP / db，不要指望共享变量
· 用 hone poop 找出复杂度最高的函数优先优化；优化前先用 hone test 固化行为
· 分发优化：hone build --exe 打包独立可执行文件（启动无解释器依赖）；hone build --script 生成 .hzp 压缩包

十二、FAQ 与已知问题

以下都是新手最容易踩的坑，提前看完能省不少调试时间。

12.1 try/catch 块内声明的变量，块外不可见

    try {
        x = 10;
    } catch e { }
    print(x);    // error：变量 x 未定义

原因：Hone 是块作用域，try 体与 catch 处理器各自独立作用域，
块内声明的变量不会泄漏到块外；catch 绑定的错误变量 e 也只在 catch 块内有效。
解决：需要跨块共享的变量，在 try 之前声明：

    x = 0;
    try {
        x = to_int("42");
    } catch e {
        x = -1;      // 降级值
    }
    print(x);    // 42

12.2 append 返回新列表，不会修改原列表

    nums = [1, 2, 3];
    append(nums, 4);
    print(nums);    // [1, 2, 3] —— 没变！

原因：列表是值类型，append 返回追加后的新列表，原列表不变。
解决：必须把返回值赋回去：

    nums = append(nums, 4);
    print(nums);    // [1, 2, 3, 4]

同理，clone / copy 是深拷贝：对副本的修改不会影响原值。

12.3 类型一经锁定，不可赋其他类型（无隐式转换）

    x = 10;         // 推导为 int
    x = "abc";      // error[H001]：期望 int，得到 str
    f = 3.14;       // float
    f = 3;          // error[H001]：float 不接受 int
    if (1 == 1.0)   // error：int 与 float 比较无隐式转换

原因：静态强类型 + 禁止隐式转换（int↔float 也不行）。
解决：需要转换时显式调用 to_int / to_float / to_str。

12.4 浮点字面量必须带小数点

    2     // int
    2.0   // float
    1 + 2.0   // error：int 与 float 不能混算

原因：字面量 2 是 int，2.0 才是 float；int 与 float 混合运算报错。
解决：混合数值运算前用 to_float / to_int 统一类型。

12.5 if / while 条件必须是 bool

    if (1) { }          // 非法：条件必须是 bool
    if (len(s) > 0) { } // 正确：比较表达式返回 bool

原因：禁止隐式转换，if (1) 不会像 C 一样自动成立。

12.6 函数名全局唯一，且不支持嵌套遮蔽

    fn foo() { ... }
    fn foo() { ... }   // error：already defined（重名报错）

原因：所有函数扁平化存在于全局符号表，不区分作用域层级。
嵌套作用域内的同名定义会被提升而非遮蔽（见 1.6）。
解决：命名加前缀或分类（如 coll_sum、math_clamp），避免重名。

12.7 len(str) 返回的是字节数，不是字符数

    len("hello")   // 5
    len("你好")    // 6 —— 每个汉字占 3 字节（UTF-8），不是 2

原因：str 按 UTF-8 字节存储。
解决：需要按字符处理时，请用 str_contains / str_replace 等字符串函数，不要用下标或 len。

12.8 go 线程不共享变量

    x = 1;
    go task(1);     // 子线程拿到的是 x 的值副本，改它不影响主线程

原因：每个 go 线程拥有独立符号表副本，只传递值类型。
解决：线程间通信走文件 / HTTP / db / server 等外部通道。

12.9 match 务必写 _ 兜底分支

    s = match 5 { 1 => "one", 2 => "two" };   // 运行时报错：无匹配分支

原因：match 所有分支都不匹配时是运行时错误，不是返回 null。
解决：总是补一个 _ => 默认值。

12.10 忘记 `#`：f-string 用 {} 插值，不是 #

    print(f"你好, {name}!");   // 正确
    print(f"你好, #name");     // 错误：# 不是插值语法

原因：Hone 的 f-string 使用 {expr} 插值（见 README 语言速览）。
解决：需要字面大括号时用 {{ }} 转义。

十三、总结：Hone 的定位

Hone 不是一门工业级语言，而是一把设计精巧的瑞士军刀。它的存在意义在于：

1. 好写：语法简洁，无冗余前缀，类型推导智能
2. 好用：内置常用功能（文件、网络、JSON、系统、时间、随机、字符串、数学、类型转换）
3. 好带：单文件 < 10 MB，跨平台
4. 好展示：可作为个人作品写在简历里、展示在 B 站上
5. 好扩展：支持 DLL 打包、懒加载、多线程、调试
6. 不背包袱：不自举、不强制兼容、不预设规模

---

Hone – 为效率而生，为乐趣而造。
🗡️