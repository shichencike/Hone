# Zap 编程语言

轻量级、跨平台、可嵌入的脚本语言。用 Rust 实现，单文件可执行程序，开箱即用。

> 设计规范：`zap..md`（v1.1）
> 当前版本：v0.1.0 —— 核心执行器（阶段 1 + 阶段 2 + 阶段 3 + 阶段 5）

## 构建

```bash
cargo build --release
# 产物：target/release/zap（Windows 下为 zap.exe）
```

## 用法

```bash
zap <script.zp>          # 执行脚本（默认命令）
zap run <script.zp>      # 执行脚本
zap debug <script.zp>    # 断点调试模式（breakpoint / debug_print 生效）
zap run --restart[=N] <script.zp> # 崩溃自动重启（N 为最大重启次数，默认 3）；
                         可配合 --backoff=a,b,c 递增等待间隔，--restart-on=Zxxx 限定可重启错误码
zap run --resume <script.zp> # 恢复上次 db 检查点并启用自动落盘（脚本变更后自动失效）
zap fmt [-w] <file.zp>   # 代码格式化（Tab 缩进/运算符空格/大括号；-w 覆盖写，支持多文件）
zap build --dll <file.zp> # 打包 C ABI 动态库（int/float/bool/str 类型映射，需系统 C 编译器）
zap build --exe <file.zp> # 将脚本与解释器打包为自释放独立可执行文件（[-o <out>] [--icon <ico>]）
zap explain <code>       # 查询错误码含义（如 zap explain Z201）
zap get <module> <url>   # 下载模块依赖并缓存到 ~/.zap/cache/
zap get <script.zp>      # 预下载脚本中所有 import 声明的模块
zap upgrade [-w] <file.zp> # 按映射表自动迁移旧版本语法（-w 覆盖写）
zap lsp                  # 启动语言服务器（补全/诊断，LSP over stdio）
zap poop <file.zp>       # 屎山检测——分析 if 嵌套深度和圈复杂度
zap --help               # 帮助
zap --version            # 版本
```

## 语言速览

```zp
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
user = {"name": "zap", "ver": 1};
print(len(nums));              // 3（元素个数）
print(to_str(user));           // {name: zap, ver: 1}
print(append(nums, 4));        // [1, 2, 3, 4]
print(contains(nums, 2));      // true

// for-in：遍历列表（单变量）或字典（键、值双变量）
for x in nums { print(x); }
for k, v in user { print(k + "=" + to_str(v)); }

// f-string：字符串插值 f"..."，{expr} 内嵌表达式，{{ / }} 转义字面大括号
name = "zap";
greeting = f"你好, {name}! 1+1={1 + 1}";
print(greeting);               // 你好, zap! 1+1=2

// 函数：参数类型可由调用上下文推导
fn fib(n) {
    if (n <= 1) { return n; }
    return fib(n - 1) + fib(n - 2);
}
print(fib(10));    // 55

// 多线程：go 启动独立线程，不共享变量
go task(1);
// 断点调试：zap debug 模式下打印变量快照
breakpoint;

// try/catch/throw：错误捕获与主动抛出
// try { body } catch e { handler }；e 为 error 类型，含 code/message/file/line/col/context 字段
// throw 字符串构造 Z600 用户错误；throw error 值重抛
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

// 调试输出（仅 zap debug 模式生效）
debug_print("当前 x = " + to_str(x));
```

## 内置功能

- 基础：`print` `len` `type_of` `read_file` `write_file` `file_exists`
- 集合：`append(list, x)` `contains(list, x)` `index_of(list, x)`（找不到返回 -1）、
  `keys(dict)` `values(dict)` `has_key(dict, k)`；`to_str` 输出 `[a, b]` / `{k: v}`
- 类型判断：`is_int` `is_float` `is_str` `is_bool` `is_list` `is_dict` `is_null`
- 字符串：`str_contains` `str_replace` `str_trim`
- 数学：`abs` `max` `min`
- 类型转换：`to_str` `to_int` `to_float`
- 模块：`time.now` `time.sleep` `time.format`（UTC）、`time.parse`（解析
  `YYYY-MM-DD[THH:MM:SS]` 时间戳 → Unix 秒）、`random.int` `random.float`、`uuid.new`（UUID v4）
- 网络：`http_get` `http_post`（支持 `http://` 与 `https://`，TLS 为纯 Rust 实现、内置 Mozilla 根证书）、`json_parse` `json_stringify`（标量）
- 系统：`sys.run` `sys.get_env`（跨平台）
- 系统（Windows API，其他平台报 Z999 或降级）：`sys.msgbox` `sys.beep` `sys.clipboard_set`
  `sys.get_screen_size`（返回 `"宽x高"` 字符串，因 Zap 无元组类型）`sys.reg_read` `sys.reg_write`
- **日志**：`log.info(msg)` `log.warn(msg)` `log.error(msg)` `log.debug(msg)`（彩色输出到 stderr）
- **路径**：`path.join(a, b, ...)` `path.dirname(p)` `path.basename(p)`（跨平台路径操作）
- **参数**：`args.get("port")` `args.has("v")`（解析 `--port 8080` / `-v` 等命令行参数）
- **环境变量**：`env.get("PATH")` `env.set("KEY", "val")`（读写环境变量）
- **键值存储**：`db.set("key", "value")` `db.get("key")`（全局内存键值存储）
- **正则**：`regex.match("^\\d+$", "123")` `regex.replace("foo", "foobar", "baz")`（正则匹配与替换）
- **哈希**：`crypto.md5("hello")` `crypto.sha256("hello")`（MD5 / SHA256 十六进制哈希）

## 导入与外部集成

```zp
// import：远程模块下载并缓存到 ~/.zap/cache/（后续运行直接使用缓存）
import "math_mod" from "http://example.com/math_mod.zp";
print(module_add(20, 22));

// import as：以别名导入，函数名前缀替换为别名
import "math_mod" from "http://example.com/math_mod.zp" as m;
print(m_add(20, 22));

// load：动态库加载（C ABI，全 int64 参数/返回值，最多 8 参数）
load "path/to/zap_lib.dll" as m;
print(m.lib_add(1, 2));

// load lazy：懒加载，首次调用时才加载
load lazy "path/to/zap_lib.dll" as lm;
print(lm.lib_fact(5));

// use：命名空间导入（内置函数已全局可用，声明保留）
use std_io;

// alias：函数别名
alias greet as hi;
hi("Zap");
```

- `import` 底层基于 TCP（复用 `http_get`），模块解析后其函数合并进全局符号表，顶层语句在独立作用域执行
- `zap get` 可预先下载模块（`zap get <module> <url>`）或扫描脚本内所有 `import` 声明批量预下载
- `load` 依赖 `libloading`（纯 Rust，无 C 编译）；被调用库需导出 `#[no_mangle] pub extern "C" fn` 形式的
  int64 函数；已加载的库不跨 `go` 线程（懒加载路径与别名可克隆）
- 模块/库函数类型在运行时才能确定，包含 import/load/alias 的程序中静态检查会对未定义函数放行

## 可视化编辑器

浏览器直接打开 `editor/index.html`（单文件 HTML，离线可用）：从左侧代码块面板拖拽
变量/print/if/else/while/函数等代码块到画布，嵌套块内部可继续拖入子块；右侧实时生成
Tab 缩进的 `.zp` 代码，支持复制与下载。初始自带 fib 示例。

示例脚本见 `examples/` 目录（正常示例 + 错误用例 + fmt/sys/dll/load/import 用例）。

## 错误报告格式

```
error[Z001]: type mismatch: variable `x` is locked to `int`, got `str`
  --> examples/err_type.zp:3:1
3 | x = "Zap";
  | ^
help: Zap types are locked after inference; no implicit conversion is allowed
```

| 错误码 | 含义 |
|--------|------|
| Z001 | 类型冲突（期望 X，得到 Y） |
| Z002 | 未定义的变量或函数 |
| Z003 | 无法自动推导类型 |
| Z004 | 运算符重载歧义 |
| Z005 | 语法错误 |
| Z006 | 字符串转整数失败 |
| Z007 | 字符串转浮点数失败 |
| Z008 | 条件表达式必须是 bool |
| Z009 | 除零错误 |
| Z010 | 整数溢出 |
| Z011 | 参数数量不匹配 |
| Z012 | 递归过深 |
| Z600 | 用户主动抛出（throw） |
| Z200 | 网络请求失败 |
| Z300 | 系统调用失败 |
| Z404 | 文件或库不存在 |
| Z999 | 尚未实现 |

## 错误码细分

| 区段 | 错误码 | 含义 |
|------|--------|------|
| 词法/语法 | Z101 | 非法字符 |
| 词法/语法 | Z102 | 字符串未闭合 |
| 词法/语法 | Z103 | 注释未闭合 |
| 词法/语法 | Z104 | 语句缺少分号 |
| 网络 | Z201 | 连接/请求超时 |
| 网络 | Z202 | 连接被拒绝 |
| 网络 | Z203 | DNS 解析失败 |
| 网络 | Z204 | 非 2xx 响应 |
| 系统/DLL | Z301 | DLL 加载失败 |
| 系统/DLL | Z302 | DLL 参数校验失败 |
| 系统/DLL | Z303 | 权限不足 |
| 文件 | Z401 | 文件不存在 |
| 文件 | Z402 | 文件权限不足 |
| 文件 | Z403 | 文件被占用/锁定 |
| 用户抛出 | Z600 | throw 语句抛出的用户错误 |

使用 `zap explain <code>` 查询完整含义与修复建议。

## 设计约束（当前实现状态）

- 静态强类型：类型一经推导即锁定，无隐式转换
- 列表 `[a, b]` 与字典 `{"k": v}` 为动态集合类型（元素类型不锁定），可用
  `append` / `contains` / `index_of` / `keys` / `values` / `has_key` 操作，
  `for-in` 遍历，`f"..."` 字符串插值；变量仍不可直接赋值不同类型的标量
- 函数扁平化存在于全局符号表（不支持嵌套作用域内的函数遮蔽，嵌套定义会被提升）
- 强制 Tab 缩进为 `zap fmt` 的格式化规则，解析器不强制
- 子线程崩溃仅打印错误，不影响主线程
- `@export` + `zap build --dll`：类型映射 int → int64_t、float → double、bool → bool、
  str → const char*（支持数值/布尔/字符串运算、strcmp 比较、str 拼接与返回值 static 缓冲 2048B）；
  导出函数建议显式标注参数与返回类型（无调用点时无法推导）；
  需要系统 C 编译器（gcc/clang，可用 `CC` 环境变量指定），找不到时保留生成的 `.c` 源码并提示手动编译
- `import` / `load` / `load lazy` / `use` / `alias` / `zap get` / `zap upgrade` / `zap lsp` 已实现
  （upgrade 按映射表迁移旧语法；lsp 提供诊断/补全/hover，冒烟测试见 `tests/lsp_smoke.py`）
- `import "mod" from "url" as alias;` 支持以别名导入模块，函数名前缀自动替换
- `try/catch/throw` 错误处理：捕获可恢复错误；catch 绑定的 `error` 类型变量含
  `code`/`message`/`file`/`line`/`col`/`context` 字段；`throw str` 构造 Z600 用户错误，
  `throw error` 重抛
- `zap run --restart=N` / `--backoff=a,b,c` / `--restart-on=Zxxx` 自动重启策略
  （重启仅对可重入错误生效；默认最多 3 次，间隔 1/3/10 秒）
- `zap run --resume` 检查点恢复（`db` 自动落盘，脚本变更后自动失效）
- `zap build --exe` 将解释器与脚本打包为自释放独立可执行文件（`[-o <out>] [--icon <ico>]`）
- `zap explain <code>` 查询错误码含义与修复建议
- `tmp fn` 临时函数在编译时自动忽略，仅开发阶段使用
- `debug_print(expr)` 调试输出仅在 `zap debug` 模式生效，普通模式自动跳过

## 路线图

- ✅ 阶段 1：词法分析器 / 解析器 / AST / 解释器 / 符号表与类型检查 / `zap run`
- 🚧 阶段 2（基本完成）：`zap fmt` ✅、`breakpoint` ✅、`go` 多线程 ✅、
  `sys` 模块 Windows API ✅、`zap build --dll`（int 子集）✅
- 🚧 阶段 3（基本完成）：`import` 远程模块 ✅、`load lazy` 懒加载 ✅、
  `use` / `alias` ✅、可视化编辑器 ✅（editor/index.html）、`zap get` ✅、
  `zap upgrade` ✅、`zap lsp` ✅
- 🚧 阶段 4（部分完成）：官网 ✅（已部署至 InfinityFree 虚拟主机 `ftpupload.net/htdocs`，
  源文件在 `官网/` 目录，FTP 上传验证通过；访问域名请在 InfinityFree 控制面板查看）、
  `--dll` float/str/bool 类型映射 ✅、GitHub 首次提交 ✅；独立域名与推广待做
- 🚧 阶段 5（新增）：内置函数扩展（log/path/args/env/db/regex/crypto）✅、
  `tmp fn` 临时函数 ✅、`debug_print` 调试输出 ✅、`import as` 别名 ✅、
  `zap poop` 屎山检测 ✅、`try/catch/throw` 错误处理 ✅、
  `zap run --restart` / `--backoff` / `--restart-on` 自动重启 ✅、
  `zap run --resume` 检查点恢复 ✅、
  `zap build --exe` 打包独立可执行文件 ✅、`zap explain` 错误码解释 ✅
- 🚧 阶段 6（新增）：集合类型（列表/字典字面量）✅、`for-in` 循环 ✅、
  `f"..."` 字符串插值 ✅、`append`/`contains`/`index_of`/`keys`/`values`/`has_key` ✅、
  `is_int`/`is_float`/`is_str`/`is_bool`/`is_list`/`is_dict`/`is_null` 类型判断 ✅、
  `time.parse` 时间戳解析 ✅、`uuid.new` UUID v4 ✅

## 许可证

MIT
