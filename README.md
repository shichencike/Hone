# Hone 编程语言

轻量级、跨平台、可嵌入的脚本语言。用 Rust 实现，单文件可执行程序，开箱即用。

> 设计规范：`hone.md`（v1.2）
> 当前版本：v0.7.0（完整工具链，详见 CHANGELOG）

## 构建

```bash
cargo build --release
# 产物：target/release/hone（Windows 下为 hone.exe）
```

## 性能

解释器为 AST 树遍历实现，2026-08-14 深度优化后的代表性基准（Windows x86_64，release 构建）：

| 基准 | 优化前 | 优化后 |
|------|--------|--------|
| 递归 fib(26) | ~3.4s | ~1.6s |
| 纯循环 2000 万次 | 71s | 42s |
| 字符串拼接 20 万次 | 90s | 20s |

关键手段：函数定义 `Arc` 共享（免每次调用的 AST 深拷贝）、循环作用域复用 +
原地赋值更新（免每轮迭代的堆分配）、字符串拼接预分配容量（免 O(n²) 整串复制）。
二进制体积约 3.7MB（strip + fat LTO + opt-level z）。基准脚本见 `bench/` 目录。

## 用法

```bash
hone <script.hn>          # 执行脚本（默认命令）
hone run <script.hn>      # 执行脚本
hone debug <script.hn>    # 断点调试模式（breakpoint / debug_print 生效）
hone run --restart[=N] <script.hn> # 崩溃自动重启（N 为最大重启次数，默认 3）；
                         可配合 --backoff=a,b,c 递增等待间隔，--restart-on=Hxxx 限定可重启错误码
hone run --resume <script.hn> # 恢复上次 db 检查点并启用自动落盘（脚本变更后自动失效）
hone fmt [-w] <file.hn>   # 代码格式化（Tab 缩进/运算符空格/大括号；-w 覆盖写，支持多文件）
hone build --dll <file.hn> # 打包 C ABI 动态库（int/float/bool/str 类型映射，需系统 C 编译器）
hone build --exe <file.hn> # 将脚本与解释器打包为自释放独立可执行文件（[-o <out>] [--icon <ico>]）
hone explain <code>       # 查询错误码含义（如 hone explain H201）
hone get                  # 读取当前目录 hone.json 清单，批量下载全部模块（类似 package.json / Cargo.toml）
hone get <module> <url>   # 下载模块依赖并缓存到 ~/.hn/cache/，并写入/更新 hone.json 清单
hone get <script.hn>      # 预下载脚本中所有 import 声明的模块
hone upgrade [-w] <file.hn> # 按映射表自动迁移旧版本语法（-w 覆盖写）
hone lsp                  # 启动语言服务器（补全/诊断，LSP over stdio）
hone repl                 # 交互式解释器（Python 式：表达式回显/多行续行/.vars 查看变量）
hone prof <file.hn>       # 剖析模式运行脚本，输出函数级热点报告（总耗时/调用次数/平均耗时）
hone poop <file.hn>       # 屎山检测——分析 if 嵌套深度和圈复杂度
hone --help               # 帮助
hone --version            # 版本
```

## 语言速览

```hn
// 变量：无前缀声明，类型推导后锁定，禁止隐式转换
x = 10;            // int
f = 3.14;          // float
s = "hello";       // str
b = true;          // bool
y : int = 20;      // 显式类型（Rust/TS 风格）
int z = 30;        // 显式类型（C 风格）

// 控制流：条件必须是 bool
if (x > 5) { print("大"); } else { print("小"); }
while (i < 10) { i = i + 1; }

// 集合：列表与字典字面量（动态元素类型，可混合）
nums = [1, 2, 3];
user = {"name": "hone", "ver": 1};
print(len(nums));              // 3（元素个数）
print(to_str(user));           // {name: hone, ver: 1}
print(append(nums, 4));        // [1, 2, 3, 4]
print(contains(nums, 2));      // true

// for-in：遍历列表（单变量）或字典（键、值双变量）
for x in nums { print(x); }
for k, v in user { print(k + "=" + to_str(v)); }

// f-string：字符串插值 f"..."，{expr} 内嵌表达式，{{ / }} 转义字面大括号
name = "hone";
greeting = f"你好, {name}! 1+1={1 + 1}";
print(greeting);               // 你好, hone! 1+1=2

// 函数：参数类型可由调用上下文推导
fn fib(n) {
    if (n <= 1) { return n; }
    return fib(n - 1) + fib(n - 2);
}
print(fib(10));    // 55

// 多线程：go 启动独立线程，不共享变量
go task(1);
// 断点调试：hone debug 模式下打印变量快照
breakpoint;

// try/catch/throw：错误捕获与主动抛出
// try { body } catch e { handler }；e 为 error 类型，含 code/message/file/line/col/context 字段
// throw 字符串构造 H600 用户错误；throw error 值重抛
fn risky_div(a, b) -> int {
    return a / b;
}

try {
    risky_div(10, 0);
} catch e {
    print(e.code + ": " + e.message);
    print("  at " + e.file + ":" + to_str(e.line));
}

// throw 主动抛出
throw "custom failure";

// 重抛：内层捕获后再次抛出，由外层兜底
try {
    try {
        throw "inner error";
    } catch e {
        throw e;
    }
} catch e {
    print("outer caught " + e.code);
}

// 临时函数（编译自动忽略，仅开发时使用）
tmp fn helper() { print("debug"); }

// 调试输出（仅 hone debug 模式生效）
debug_print("当前 x = " + to_str(x));

// class 类：成员函数不进全局符号表，经 类.方法(...) 调用
class Math {
    fn double(x) { return x * 2; }
    fn fib(n) {
        if (n <= 1) { return n; }
        return Math.fib(n - 1) + Math.fib(n - 2);
    }
}
print(Math.double(21));   // 42
print(Math.fib(10));      // 55

// 泛型：fn name[T, U](...) 声明类型参数，调用时按实参推导（编译期擦除，运行期零成本）
fn identity[T](x: T) -> T { return x; }
print(identity(42));          // 42（T=int）
print(identity("hello"));     // hello（T=str）

fn pick[T](a: T, b: T, want_first: bool) -> T {
    if (want_first) { return a; }
    return b;
}
print(pick(10, 20, false));   // 20（T=int）

// C 风格 for / do-while / continue
for (i = 0; i < 5; i = i + 1) { print(i); }   // 0 1 2 3 4
j = 0;
do { j = j + 1; } while (j < 3);              // 至少执行一次
for (i = 1; i <= 5; i = i + 1) {
    if (i % 2 == 0) { continue; }             // 跳过偶数
    print(i);                                 // 1 3 5
}

// 复合赋值（str 仅支持 +=）与自增/自减
x = 10; x += 5; x -= 3; x *= 2; x /= 4; x %= 4;  // x = 2
i = 0; i++; print(i);  // 1（后缀返回旧值）
print(++i);            // 2（前缀返回新值）

// 三元表达式（右结合可嵌套）与空值合并（a 为 null 取 b）
label = x >= 0 ? "pos" : "neg";
fn void_fn() {}
v = void_fn();
print(v ?? "default");   // default（void 调用返回 null）

// 函数默认参数：省略尾部实参取默认值，默认表达式可引用前面参数
fn greet(name, greeting = "Hello") { return greeting + ", " + name + "!"; }
print(greet("Hone"));          // Hello, Hone!
print(greet("Hone", "Hi"));    // Hi, Hone!

// 匿名函数（lambda）：一等值，按值捕获外围变量（闭包），经变量名动态调用
double = fn(x) { return x * 2; };
print(double(21));             // 42
base = 10;
adder = fn(y) { return base + y; };
print(adder(5));               // 15（捕获 base）
fn apply(f, v) { return f(v); }
print(apply(fn(n: int) { return n * n; }, 6));   // 36

// 三引号原始字符串：多行、不做转义处理、保留换行与缩进
text = """line1
line2
    indented""";
print(len(text));              // 24
```

## 内置功能

- 基础：`print` `len` `type_of` `read_file` `write_file` `file_exists`
- 标准输入：`input(prompt?)` 读取一行返回 str（EOF 报 H306）、`read_int(prompt?)` 读取并解析为 int（格式错报 H006）、
  `read_float(prompt?)` 读取并解析为 float（格式错报 H007）
- 集合：`append(list, x)` `contains(list, x)` `index_of(list, x)`（找不到返回 -1）、
  `keys(dict)` `values(dict)` `has_key(dict, k)`；`to_str` 输出 `[a, b]` / `{k: v}`
- 类型判断：`is_int` `is_float` `is_str` `is_bool` `is_list` `is_dict` `is_null`
- 字符串：`str_contains` `str_replace` `str_trim`
- 数学：`abs` `max` `min`
- 类型转换：`to_str` `to_int` `to_float`
- 模块：`time.now` `time.sleep` `time.format`（UTC）、`time.parse`（解析
  `YYYY-MM-DD[THH:MM:SS]` 时间戳 → Unix 秒）、`random.int` `random.float`、`uuid.new`（UUID v4）
- 网络：`http_get` `http_post`（支持 `http://` 与 `https://`，TLS 为纯 Rust 实现、内置 Mozilla 根证书）、`json_parse` `json_stringify`（标量）
- 本地服务器：`server.listen(port)` 启动后台监听线程（0=自动分配端口，返回实际端口）、`server.poll()` 取出排队请求（返回 JSON 数组 `[{id,method,path,body}, ...]`）、`server.respond(id, body)` 发送响应体（HTTP 200）——纯 std::net 实现，Windows / Linux / Termux 跨平台一致，无 C 依赖
- 系统：`sys.run` `sys.get_env`（跨平台）
- 系统（Windows API，其他平台报 H999 或降级）：`sys.msgbox` `sys.beep` `sys.clipboard_set`
  `sys.get_screen_size`（返回 `"宽x高"` 字符串，因 Hone 无元组类型）`sys.reg_read` `sys.reg_write`
- **日志**：`log.info(msg)` `log.warn(msg)` `log.error(msg)` `log.debug(msg)`（彩色输出到 stderr）
- **路径**：`path.join(a, b, ...)` `path.dirname(p)` `path.basename(p)`（跨平台路径操作）
- **参数**：`args.get("port")` `args.has("v")`（解析 `--port 8080` / `-v` 等命令行参数）
- **环境变量**：`env.get("PATH")` `env.set("KEY", "val")`（读写环境变量）
- **键值存储**：`db.set("key", "value")` `db.get("key")`（全局内存键值存储）
- **正则**：`regex.match("^\\d+$", "123")` `regex.replace("foo", "foobar", "baz")`（正则匹配与替换）
- **哈希**：`crypto.md5("hello")` `crypto.sha256("hello")`（MD5 / SHA256 十六进制哈希）

## 导入与外部集成

```hn
// import：远程模块下载并缓存到 ~/.hn/cache/（后续运行直接使用缓存）
import "math_mod" from "http://example.com/math_mod.hn";
print(module_add(20, 22));

// import as：以别名导入，函数名前缀替换为别名
import "math_mod" from "http://example.com/math_mod.hn" as m;
print(m_add(20, 22));

// load：动态库加载（C ABI，全 int64 参数/返回值，最多 8 参数）
load "path/to/hone_lib.dll" as m;
print(m.lib_add(1, 2));

// load 签名块：typed FFI，显式声明 C 参数/返回类型（int/float/bool/str/ptr/void）
load "path/to/hone_lib.dll" as m2 {
    fn lib_add_f(a: float, b: float) -> float;
    fn lib_strlen(s: str) -> int;
    fn lib_open(path: str) -> ptr;
}
print(m2.lib_add_f(1.5, 2.25));

// load from 头文件：自动从 C 头文件提取原型生成签名（受限解析器，纯 Rust）
load "path/to/hone_lib.dll" as m3 from "path/to/hone_lib.h";
print(m3.lib_strlen("hello"));   // 类型来自头文件
// 也可用 hone bind <header.h> 离线生成签名块再粘贴进脚本

// load lazy：懒加载，首次调用时才加载
load lazy "path/to/hone_lib.dll" as lm;
print(lm.lib_fact(5));

// use：命名空间导入（内置函数已全局可用，声明保留）
use std_io;

// alias：函数别名（原名支持点号路径，别名可叠加、可指向内置函数）
alias greet as hi;
hi("Hone");

alias random.int as rint;   // 点号路径原名（模块/内置函数）
print(rint(1, 100));

alias print as p;           // 内置函数别名
p("via alias");
```

- `import` 底层基于 TCP（复用 `http_get`），模块解析后其函数合并进全局符号表，顶层语句在独立作用域执行
- `hone get` 可预先下载模块：`hone get <module> <url>`（并写入 hone.json 清单）、
  `hone get <script.hn>`（扫描脚本内所有 `import` 声明批量预下载）、
  `hone get`（读取当前目录 hone.json 清单批量下载全部模块）
- `load` 依赖 `libloading`（纯 Rust，无 C 编译）；被调用库需导出 `#[no_mangle] pub extern "C" fn` 形式的
  int64 函数；已加载的库不跨 `go` 线程（懒加载路径与别名可克隆）
- `load` 签名块（typed FFI）：`load "lib" as m { fn f(a: int, b: str) -> ptr; ... }` 显式声明
  C ABI 类型，支持 int/float/bool/str/ptr/void，调用按声明精确转换，静态检查参数个数与类型；
  未声明签名的库函数仍按 int64 通道调用（见 `examples/ffi_demo.hn`）
- `load "lib" as m from "header.h"`：从 C 头文件自动提取函数原型生成签名（受限解析器，跳过
  注释/预处理/struct 定义，typedef 简单展开；回调/变参/数组/结构体按值标记 unsupported 并报错）；
  `hone bind <header.h>` 离线生成签名块（见 `examples/ffi_header.hn`）
- 模块/库函数类型在运行时才能确定，包含 import/load/alias 的程序中静态检查会对未定义函数放行

## 可视化编辑器

浏览器直接打开 `editor/index.html`（单文件 HTML，离线可用）：从左侧代码块面板拖拽
变量/print/if/else/while/函数等代码块到画布，嵌套块内部可继续拖入子块；右侧实时生成
Tab 缩进的 `.hn` 代码，支持复制与下载。初始自带 fib 示例。

示例脚本见 `examples/` 目录（正常示例 + 错误用例 + fmt/sys/dll/load/import 用例 + server/gui 图形界面用例）。

## 图形界面库（hone_lib/gui.hn）

浏览器渲染 + 本地 HTTP 服务器的双向交互 GUI，纯 Hone 编写，跨平台（Windows / Linux / Termux 均有浏览器）。
依赖 `server.*` 内置函数（v0.3.0+）。运行示例：`hone examples/gui_demo.hn`（自动打开浏览器，Ctrl+C 退出）。

```hn
import "gui" from "./hone_lib/gui.hn";

// 界面事件处理：id 为控件 id，value 为用户输入值（on_event 为约定函数名）
fn on_event(id : str, value) -> str {
    if (id == "btn_hi") {
        return json_stringify({"update": [["lbl_out", "你好，Hone GUI！"]]});
    }
    return "";
}

widgets = [
    gui_button("btn_hi", "打招呼"),
    gui_label("lbl_out", "(事件输出)"),
];
gui_run("Hone GUI 演示", widgets);
```

- 控件：`gui_button(id, label)`、`gui_label(id, text)`、`gui_input(id, label, placeholder)`、`gui_select(id, label, options)`、`gui_html(html)`
- `on_event` 返回值按 JSON 协议解释：`{"update": [[元素id, 新文本], ...]}` 更新元素文本、`{"alert": "消息"}` 弹窗提示、其他文本显示在页面底部状态栏
- 底层 `server.*` API（见 `examples/server_demo.hn`）：`server.listen(port)` 启动后台监听线程（0=自动分配，返回实际端口）、`server.poll()` 取出排队请求（返回 JSON 数组）、`server.respond(id, body)` 发送响应体；后台线程只做 TCP 收发与排队，脚本在主线程轮询响应，与解释器单线程模型兼容；进程内自测：`hone examples/server_selftest.hn`

## 图片库（hone_lib/img.hn）

纯 Hone 编写的图片标准库，像素网格 + 绘图/滤镜/变换 + PPM P3 / SVG 读写。
运行示例：`hone examples/test_img_lib.hn`（生成 `examples/img_test.ppm` 与 `examples/img_test.svg`）。

```hn
import "img" from "./hone_lib/img.hn";

// 图像 = {"w": 宽, "h": 高, "rows": [[[r,g,b],...],...]}
im = img_new(64, 64, 255, 255, 255);        // 白色画布
im = img_rect(im, 8, 8, 40, 40, 255, 0, 0); // 红色实心矩形
im = img_circle(im, 32, 32, 16, 0, 0, 255); // 蓝色实心圆
im = img_grayscale(im);                     // 灰度滤镜
img_save_ppm(im, "out.ppm");                // 保存 PPM P3
img_save_svg(im, "out.svg", 4);             // 保存 SVG（每像素放大 4 倍）
```

- 创建/绘图：`img_new` / `img_fill` / `img_rect` / `img_line` / `img_circle` / `img_gradient_h` / `img_gradient_v` / `img_checker` / `img_noise`
- 滤镜：`img_grayscale` / `img_invert` / `img_brightness` / `img_contrast` / `img_threshold`
- 变换：`img_flip_h` / `img_flip_v` / `img_rotate90` / `img_crop` / `img_scale`
- 读写：`img_to_ppm` / `img_save_ppm` / `img_from_ppm` / `img_load_ppm` / `img_to_svg` / `img_save_svg`
- 说明：Hone 的 str 无法承载二进制，故不支持 PNG/BMP 等二进制格式，采用文本格式 PPM P3（GIMP/ImageMagick 可查看）与 SVG（浏览器直接打开）；列表为值类型、索引赋值整体拷贝，绘图/滤镜适合 ≤128×128 的小图

## 错误报告格式

```
error[H001]: type mismatch: variable `x` is locked to `int`, got `str`
  --> examples/err_type.hn:3:1
3 | x = "Hone";
  | ^
help: Hone types are locked after inference; no implicit conversion is allowed
```

| 错误码 | 含义 |
|--------|------|
| H001 | 类型冲突（期望 X，得到 Y） |
| H002 | 未定义的变量或函数 |
| H003 | 无法自动推导类型 |
| H004 | 运算符重载歧义 |
| H005 | 语法错误 |
| H006 | 字符串转整数失败 |
| H007 | 字符串转浮点数失败 |
| H008 | 条件表达式必须是 bool |
| H009 | 除零错误 |
| H010 | 整数溢出 |
| H011 | 参数数量不匹配 |
| H012 | 递归过深 |
| H600 | 用户主动抛出（throw） |
| H200 | 网络请求失败 |
| H300 | 系统调用失败 |
| H306 | 标准输入读取失败/EOF |
| H404 | 文件或库不存在 |
| H999 | 尚未实现 |

## 错误码细分

| 区段 | 错误码 | 含义 |
|------|--------|------|
| 词法/语法 | H101 | 非法字符 |
| 词法/语法 | H102 | 字符串未闭合 |
| 词法/语法 | H103 | 注释未闭合 |
| 词法/语法 | H104 | 语句缺少分号 |
| 网络 | H201 | 连接/请求超时 |
| 网络 | H202 | 连接被拒绝 |
| 网络 | H203 | DNS 解析失败 |
| 网络 | H204 | 非 2xx 响应 |
| 系统/DLL | H301 | DLL 加载失败 |
| 系统/DLL | H302 | DLL 参数校验失败 |
| 系统/DLL | H303 | 权限不足 |
| 系统/DLL | H306 | 标准输入读取失败/EOF（input / read_int / read_float） |
| 文件 | H401 | 文件不存在 |
| 文件 | H402 | 文件权限不足 |
| 文件 | H403 | 文件被占用/锁定 |
| 用户抛出 | H600 | throw 语句抛出的用户错误 |

使用 `hone explain <code>` 查询完整含义与修复建议。

## 设计约束（当前实现状态）

- 静态强类型：类型一经推导即锁定，无隐式转换
- 列表 `[a, b]` 与字典 `{"k": v}` 为动态集合类型（元素类型不锁定），可用
  `append` / `contains` / `index_of` / `keys` / `values` / `has_key` 操作，
  `for-in` 遍历，`f"..."` 字符串插值；变量仍不可直接赋值不同类型的标量
- 函数扁平化存在于全局符号表（不支持嵌套作用域内的函数遮蔽，嵌套定义会被提升）
- 强制 Tab 缩进为 `hone fmt` 的格式化规则，解析器不强制
- 子线程崩溃仅打印错误，不影响主线程
- `@export` + `hone build --dll`：类型映射 int → int64_t、float → double、bool → bool、
  str → const char*（支持数值/布尔/字符串运算、strcmp 比较、str 拼接与返回值 static 缓冲 2048B）；
  导出函数建议显式标注参数与返回类型（无调用点时无法推导）；
  需要系统 C 编译器（gcc/clang，可用 `CC` 环境变量指定），找不到时保留生成的 `.c` 源码并提示手动编译
- `import` / `load` / `load lazy` / `use` / `alias` / `hone get` / `hone upgrade` / `hone lsp` 已实现
  （upgrade 按映射表迁移旧语法；lsp 提供诊断/补全/hover，冒烟测试见 `tests/lsp_smoke.py`）
- `import "mod" from "url" as alias;` 支持以别名导入模块，函数名前缀自动替换
- `try/catch/throw` 错误处理：捕获可恢复错误；catch 绑定的 `error` 类型变量含
  `code`/`message`/`file`/`line`/`col`/`context` 字段；`throw str` 构造 H600 用户错误，
  `throw error` 重抛
- `hone run --restart=N` / `--backoff=a,b,c` / `--restart-on=Hxxx` 自动重启策略
  （重启仅对可重入错误生效；默认最多 3 次，间隔 1/3/10 秒）
- `hone run --resume` 检查点恢复（`db` 自动落盘，脚本变更后自动失效）
- `hone build --exe` 将解释器与脚本打包为自释放独立可执行文件（`[-o <out>] [--icon <ico>]`）
- `hone explain <code>` 查询错误码含义与修复建议
- `tmp fn` 临时函数在编译时自动忽略，仅开发阶段使用
- `debug_print(expr)` 调试输出仅在 `hone debug` 模式生效，普通模式自动跳过
- 新语法（v0.6.3）：`continue`、C 风格 `for (init; cond; step)`、`do-while`、
  复合赋值 `+= -= *= /= %=`（str 仅支持 `+=`）、自增/自减 `++ --`、
  三元 `cond ? a : b`、空值合并 `a ?? b`、匿名函数 lambda（一等值/闭包，经变量名动态调用）、
  函数默认参数（尾部省略、默认表达式可引用前面参数）、三引号原始字符串 `"""..."""`；
  lambda 与默认参数暂不支持 `hone build --dll`（报错提示改用解释模式）

## 路线图

- ✅ 阶段 1：词法分析器 / 解析器 / AST / 解释器 / 符号表与类型检查 / `hone run`
- 🚧 阶段 2（基本完成）：`hone fmt` ✅、`breakpoint` ✅、`go` 多线程 ✅、
  `sys` 模块 Windows API ✅、`hone build --dll`（int 子集）✅
- 🚧 阶段 3（基本完成）：`import` 远程模块 ✅、`load lazy` 懒加载 ✅、
  `use` / `alias` ✅、可视化编辑器 ✅（editor/index.html）、`hone get` ✅、
  `hone upgrade` ✅、`hone lsp` ✅
- ✅ 阶段 4：官网 ✅（已部署至 https://hone.xo.je，源文件在 `官网/` 目录）、
  `--dll` float/str/bool 类型映射 ✅、GitHub 首次提交 ✅；推广待做
- 🚧 阶段 5（新增）：内置函数扩展（log/path/args/env/db/regex/crypto）✅、
  `tmp fn` 临时函数 ✅、`debug_print` 调试输出 ✅、`import as` 别名 ✅、
  `hone poop` 屎山检测 ✅、`try/catch/throw` 错误处理 ✅、
  `hone run --restart` / `--backoff` / `--restart-on` 自动重启 ✅、
  `hone run --resume` 检查点恢复 ✅、
  `hone build --exe` 打包独立可执行文件 ✅、`hone explain` 错误码解释 ✅
- 🚧 阶段 6（新增）：集合类型（列表/字典字面量）✅、`for-in` 循环 ✅、
  `f"..."` 字符串插值 ✅、`append`/`contains`/`index_of`/`keys`/`values`/`has_key` ✅、
  `is_int`/`is_float`/`is_str`/`is_bool`/`is_list`/`is_dict`/`is_null` 类型判断 ✅、
  `time.parse` 时间戳解析 ✅、`uuid.new` UUID v4 ✅

## 许可证

MIT
