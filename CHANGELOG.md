# Changelog

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
