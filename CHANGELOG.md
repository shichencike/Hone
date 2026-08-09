# Changelog

## [v0.6.0] - 2026-08-09

### 新增
- **class 类**：`class 名称 { fn 方法(...) { ... } }` 将相关函数组织成命名空间，经 `类.方法(...)` 调用；
  成员函数不进全局符号表（裸名调用报 H002，不污染全局命名空间，同名全局函数可与类方法共存）；
  类内互调用限定名（`Math.fib(n-1)`）；类名与 struct 名不重复；支持参数/返回值/递归/`-> type` 注解
- **泛型**：`fn name[T, U](参数) -> T` 在函数名后声明类型参数，参数/返回注解可引用（`x: T`）；
  调用时按实参自动推导，同一泛型函数可不同类型多次调用、互不锁定；**编译期擦除**——运行期零成本、
  不产生重复代码；类方法同样支持泛型；未声明类型参数报 H002、重复类型参数报 H005
- **标准库大规模扩展**（内置函数，开箱即用）：
  - 数据处理：`csv.parse` / `csv.parse_dict` / `csv.stringify`（RFC 4180）、`yaml.parse` / `yaml.stringify`
  - 系统工具：`glob.match` / `glob.list`（glob 匹配）、`temp.dir` / `temp.file` / `temp.remove`
  - 压缩：`zlib.compress` / `zlib.decompress` / `zlib.gzip` / `zlib.gunzip`（结果 base64 编码）
  - 科学计算：`stat.*`（sum/mean/median/variance/stddev/min/max）、`matrix.*`（identity/transpose/add/mul/scale）
  - 文本处理：`diff.lines` / `diff.unified`（LCS 对比）、`regex.find` / `regex.groups` / `regex.split`
  - 时间增强：`time.add` / `time.diff` / `time.weekday`（ISO 1-7），`time.format` 新增 `WW` 星期占位符
  - 网络：`http.request`（通用请求：method/headers/body/timeout，可自定义 User-Agent）、
    `smtp.send`（发邮件，STARTTLS / 隐式 TLS / AUTH LOGIN）、`ws.request`（WebSocket 请求-响应，支持 wss://）
  - 绘图：`plot.bar` / `plot.line`（生成 SVG 图表文本，可保存为 .svg）
  - 数据库：`sqlite.*`（open/close/exec/query/query_one/escape/last_insert_id/changes，
    运行时通过 libloading 加载系统 libsqlite3，保持零 C 构建依赖）
- 语言特性：点号后允许关键字作为模块成员名（如 `glob.match`、`random.int`、`plugin.load`）
- 新增示例 `examples/stdlib_test.hn`、`examples/class_test.hn`、`examples/generic_test.hn`

### 文档
- README、hone.md、官网 docs.html / stdlib.html / changelog.html：补充 class 类、泛型与全部新标准库模块

## [v0.5.0] - 2026-08-08

### 新增
- 语言特性：`struct` 结构体（确定数据形态，构造校验字段个数与类型）、`match` 模式匹配（字面量 + `_` 通配符）、
  `|>` 管道操作符（可链式，解析期转为普通调用）
- 内置函数扩展：`clone` / `copy` 深度拷贝、`assert` 断言（H700）、`args.get(key, type, default)` 类型转换与默认值、
  `server.respond(id, body, status)` 自定义 HTTP 状态码
- 指针类 `ptr.*`：`ptr.alloc/free/is_null/is_valid/size/read_*/write_*` 内存分配与读写；分配表跟踪防野指针
- 压缩与归档 `archive.*`：zip / tar.gz 的列表、读取、解压与创建（防 zip-slip 穿越）
- 加密与哈希：`crypto.sha1` / `crypto.hmac_sha256` / `crypto.base64_encode` / `crypto.base64_decode`
- 插件系统 `plugin.*`：运行期动态注册，调用走 C ABI 通道
- 测试框架：`hone test [目录]` 递归扫描 `*.test.hn` 运行并汇总 PASS/FAIL
- 打包格式：`hone build --script` 生成仅脚本压缩包 `.hzp`（`hone run` 执行）
- 自动更新：`hone self-update [url]` 下载最新二进制替换当前程序

### 变更
- 移除 `hone upgrade` 命令（旧语法迁移工具不再提供）

## [v0.4.0] - 2026-08-08

### 新增
- typed FFI：`load "lib" as m { fn f(p: ty, ...) -> ret; ... }` 签名块显式声明 C ABI 参数与返回类型，
  支持 `int`（int64_t）/ `float`（double）/ `bool` / `str`（const char*）/ `ptr`（void*）/ `void`
  （返回），调用时按声明精确转换，替代此前全 int64 单通道；参数最多 8 个，支持任意 int/float 混合
  位置（按类别位展开二分分派，Windows / Linux / Termux ABI 一致）
- 静态检查：签名块声明的函数调用在检查阶段校验参数个数与类型（H001/H005），返回类型参与类型推导；
  未声明签名的库函数保持旧 int64 通道调用（完全向后兼容）
- 新类型 `ptr`：FFI 返回值/参数可传递不透明句柄，`p == 0` 判断 NULL；to_str(p) 输出 0x 十六进制
- 头文件自动绑定：`load "lib" as m from "header.h";` 从 C 头文件提取函数原型自动生成签名
  （受限解析器：跳过注释/预处理/struct 定义/extern "C" 块，typedef 简单展开，属性宏跳过；
  类型映射 int/size_t→int、float/double→float、bool→bool、char*→str、其余指针→ptr、void→void；
  回调/变参/数组/结构体按值/long double 标记 unsupported，调用时直接报错而非 ABI 崩溃）
- 新命令 `hone bind <header.h>`：离线生成 typed load 签名块（可直接粘贴进脚本）
- 新增示例 `examples/ffi_demo.hn`（typed FFI 全类型演示）、`examples/ffi_header.hn`（from 头文件
  自动绑定演示）；`tests/hone_lib` 扩展导出 float/str/bool/ptr/void 测试函数并新增 hone_lib.h

### 变更
- 语言更名：Zap → Hone（二进制 `hone`、扩展名 `.hn`、错误码 `Hxxx`、缓存目录 `~/.hone`）
- 新增 GitHub Actions CI（Windows/Linux 构建测试 + Termux aarch64 交叉编译）与 tag 触发自动发布
  （三平台二进制 + 校验和 + 一键安装脚本 → GitHub Releases 附件）
- 新增一键安装脚本 install.sh / install.ps1（sha256 校验）
- 官网部署至 https://hone.xo.je

### 文档
- README、hone.md：load 章节补充签名块语法、类型映射与限制（回调 fn(...) 与可变参数 ... 暂不支持）、
  from 头文件自动绑定与 hone bind 用法

## [v0.3.0] - 2026-08-07

### 新增
- `server.listen(port)` / `server.poll()` / `server.respond(id, body)` 本地 HTTP 服务器内置函数：纯 std::net 实现，Windows / Linux / Termux 跨平台一致，无 C 依赖；后台线程只做 TCP 收发与请求排队，Hone 脚本在主线程轮询响应，与解释器单线程模型完全兼容
- 图形界面库 `hone_lib/gui.hn`（纯 Hone 编写）：浏览器渲染 + 本地服务器双向交互，控件 `gui_button` / `gui_label` / `gui_input` / `gui_select` / `gui_html`，事件回调约定 `on_event(id, value)`，返回值按 JSON 协议更新页面元素
- 新增示例 `examples/gui_demo.hn`（GUI 演示）、`examples/server_demo.hn`（server API 演示）、`examples/server_selftest.hn`（进程内自测）

### 文档
- README：新增"图形界面库"章节，内置函数表补充 server.* 说明

## [v0.2.0] - 2026-08-07

### 新增
- `http_get` / `http_post` 支持 `https://`：TLS 采用 rustls + rustls-rustcrypto（纯 Rust 实现，无 C 依赖），内置 Mozilla 根证书，Windows / Linux / Termux 跨平台行为一致，无需系统依赖
- 新增示例 `examples/https_demo.hn`：展示 https GET、http POST、JSON 解析与错误捕获

### 修复
- 类型检查：`http_post` 的返回类型标注由 `void` 修正为 `str`（与运行时返回响应体一致），此前将返回值传给其他函数会被静态检查误报

### 文档
- README、hone.md：更新网络功能说明（支持 http/https、纯 Rust TLS、内置根证书）
- 官网 docs.html / examples.html：内置函数表标注 https 支持，新增"HTTP 网络请求"示例

### 构建
- `bin/` 三平台二进制重新编译（Windows x86_64 宿主编译；Linux x86_64 与 Termux aarch64 使用 musl 静态交叉编译，不依赖目标机 glibc/bionic）
