Hone 编程语言 – 完整设计规范 v1.1

项目代号：Hone
设计者：时辰刺客
许可证：MIT
目标定位：轻量级、跨平台（要支持termux）、可嵌入的脚本语言，兼具瑞士军刀式的实用性和玩具的易用性
设计哲学：效率至上、极简无前缀、不自举、不背兼容包袱、报错精准、开箱即用

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

1.8 模式匹配（match）

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

1.9 管道操作符（|>）

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

示例：


t = time.now();
print(t);                  // 1698765432
time.sleep(1.5);
print(time.format(t, "YYYY-MM-DD HH:mm:SS")); // 2023-10-31 14:30:45


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

· H001：类型冲突（期望类型 X，得到类型 Y）
· H003：无法自动推导类型，请添加显式类型
· H004：运算符重载歧义（如 + 可能为 int 或 str 时）
· H006：字符串转换为整数失败（非数字内容）
· H007：字符串转换为浮点数失败（格式非法）
· H011：参数数量不匹配
· H100：动态库加载失败
· H110：懒加载依赖函数未找到
· H200：网络请求失败
· H204：HTTP 非 2xx 状态码
· H300：系统调用失败
· H301：DLL 加载失败
· H302：DLL 参数校验失败
· H303：权限不足
· H304：野指针（未分配/已释放/重复释放/空指针，ptr 类）
· H305：指针越界访问（超出 ptr.alloc 分配大小）
· H401：文件不存在
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


十一、总结：Hone 的定位

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