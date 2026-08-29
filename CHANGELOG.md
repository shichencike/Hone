# Changelog

## [v0.7.8] - 2026-08-29

### 新增
- **文件二进制读写**：新增内置函数 `read_bytes(path)` → list[int]（读取文件原始字节，每个元素为
  0-255 的 int，二进制安全，不再受 UTF-8 限制）与 `write_bytes(path, bytes)`（将字节列表原样写入，
  元素须为 0-255 的 int，越界/非整数报类型错误）；解释器、AOT 原生编译、LSP 悬停文档同步支持

### 重构
- **hone_lib/pet.hn 桌宠改原生窗口**：不再依赖 PowerShell/WinForms 与 state.json/events.json
  文件轮询，改为运行时 guipro 模块新增的 `guipro.pet_*` 原生 Win32 窗口内置函数——
  `pet_window`（无边框 + 品红键透明 + 置顶 + 不抢焦点）/ `pet_frame`（RGB 文本推帧，
  Rust 端最近邻放大 + 水平翻转）/ `pet_text`（气泡）/ `pet_move` / `pet_pos` /
  `pet_cursor` / `pet_menu`（右键原生弹出菜单，返回选中项）/ `pet_close`
- 交互改走 `guipro.poll` 事件队列：click / dblclick / drag（Rust 端 SetCapture 拖拽窗口）/
  rclick；跟随鼠标改用 `pet_cursor` 直接读光标；帧由磁盘 PPM 缓存改为内存 RGB 文本
  （`pet_frame_rgb` / `pet_frames_rgb`），换装即时重建；外部 PPM 帧目录替换仍支持
- 窗口随 Hone 进程生命周期，Ctrl+C 不再残留 PowerShell 孤儿窗口；示例：
  `examples/pet_demo.hn`

## [v0.7.6] - 2026-08-26

### 新增
- **AOT 原生编译**：`hone build --exe -c <script.hn>` 将 .hn 脚本编译为 C 中间文件，再用系统
  C 编译器（gcc/clang/cc，可用 `CC` 环境变量指定）生成原生可执行文件；生成前先做语法与类型检查
- 编译参数 `-Os -ffunction-sections -fdata-sections` + 链接期死代码消除，未用到的内置函数不占体积，
  常见脚本约 50~70 KB；默认删除中间 .c 文件，`--keep-c` 保留便于检查，输出名用 `[-o <out>]`
- 值语义与解释器一致（列表/字典深拷贝、lambda 按值捕获、索引赋值克隆写回），支持函数/递归/类方法/
  结构体/lambda/异常/推导式/解构/多返回值/match/可选链/复合赋值等全语言特性
- 内置函数支持核心集（print/len/to_str/集合/字符串/文件/时间/随机等）；
  http/crypto/sqlite/guipro 等重内置与 import/load/go 在编译期报「暂不支持」（附定位与建议）

## [v0.7.5] - 2026-08-26

### 工具链增强
- **LSP 语义高亮**：新增 `textDocument/semanticTokens/full`（semantic tokens），复用词法分析器精准 token 流，
  智能分类高亮——关键字 / 类型（`int/float/bool/str`）/ 函数（定义名带 declaration 修饰符、调用、内置函数）/
  变量 / 字符串（含 `f"..."` 插值串与 `"""` 多行串）/ 数字 / 注释（`//` 与跨行 `/* */`）/
  命名空间（`time.` 等模块名）/ 类 / 结构体，输出标准 LSP delta 编码
- 词法失败（如未闭合字符串）时自动退化为仅注释高亮，不影响编辑；无新增依赖

## [v0.7.4] - 2026-08-26

### 新增
- **guipro 进阶控件**：滑块 slider / 表格 table / 树 tree / 画布 canvas，支持读写
  （guipro_table_* / guipro_tree_* / guipro_canvas_*）与事件；示例 examples/guipro_adv_demo.hn
  （滑块/表格/树/画布/托盘/菜单）
- **托盘图标与菜单栏**：guipro_tray_add/tray_tip/tray_remove（Windows 用 Shell_NotifyIcon，
  Linux X11 用 XEmbed 系统托盘协议）、guipro_menu 菜单栏（menu 事件 value=菜单路径）
- **Linux X11 自绘后端**：GTK3 缺失时自动回退 libX11.so.6 动态加载的单窗口自绘后端，
  覆盖全部控件与事件（含托盘 XEmbed）；本机 Windows 亦可 cargo check 编译验证

## [v0.7.3] - 2026-08-25

### 新增
- **hone_lib/guipro.hn 原生图形界面标准库**（gui.hn 的升级版）：原生窗口 + 原生控件，不再依赖浏览器。
  Windows 用 Win32 标准控件（user32/gdi32，零新增依赖、不增体积），Linux 运行时动态加载 GTK3
  （缺失时 msgbox 降级 zenity/xmessage）；控件：button / label / input / select / checkbox / radio + VBox 布局；
  事件：click / change / close / resize，闭包分发 + `guipro_timer` 定时器 + 原生消息框；
  示例：`examples/guipro_demo.hn`（20 秒后自动关闭演示）
- 架构：内置 `guipro.*` 原语（checker 签名表 + builtins 分发）只推送事件队列，闭包分发在 Hone 层
  `guipro_run` 主循环完成（builtins::call 无解释器上下文）；事件注册表函数式传递（Hone 函数内不能改全局变量）

## [v0.7.2] - 2026-08-25

### 新增
- **hone_lib/pet.hn 桌宠标准库**（Windows）：Hone 驱动状态机 + PowerShell 透明置顶窗渲染，
  内置像素猫 8 帧动画（idle 眨眼 / walk 走路镜像 / sleep 睡觉 / happy 开心 / surprise 惊讶），
  说话气泡、点击/拖拽交互、右键菜单（换装/静音/跟随鼠标/隐藏显示台词/退出）、全屏游荡、
  自动入睡、整点报时、CPU/内存播报（wmic）；支持自定义台词 JSON 与外部 PPM 帧目录替换；
  提供阻塞 `pet_run(cfg)` 与非阻塞 `pet_tick(st)` 两种形态；示例：`examples/pet_demo.hn`
- 性能：帧构建用 `ptr.alloc` 缓冲 + 24×24 内联整数判定 + 2× 最近邻放大到 48×48，
  8 帧约 2s（对比推导式 rows 约 10s）；规避 Hone 列表值类型 O(w*h) 逐像素拷贝
- 说明：Hone 无原生窗口 API，窗口渲染/鼠标事件由运行时生成的 `pet_window.ps1`
  （WinForms 透明置顶窗）承担，Hone 与窗口以文件轮询通信（state.json / events.json）；
  仅支持 Windows（PowerShell 5.1+），与 gui.hn 同类，非纯 Hone 实现

## [v0.7.1] - 2026-08-19

### 新增
- **hone_lib/img.hn 图片标准库**（纯 Hone 编写）：像素网格 `{"w","h","rows"}` 表示 +
  创建/绘图（`img_new`、`img_fill`、`img_rect`、`img_line`、`img_circle`、`img_gradient_h/v`、
  `img_checker`、`img_noise`）+ 滤镜（`img_grayscale`、`img_invert`、`img_brightness`、
  `img_contrast`、`img_threshold`）+ 变换（`img_flip_h/v`、`img_rotate90`、`img_crop`、
  `img_scale`）+ PPM P3 读写（`img_to_ppm`、`img_save_ppm`、`img_from_ppm`、`img_load_ppm`）
  与 SVG 输出（`img_to_svg`、`img_save_svg`）；示例：`examples/test_img_lib.hn`
- 说明：Hone 的 str 无法承载二进制，故采用文本格式 PPM P3 与 SVG，不支持 PNG/BMP；
  列表为值类型，绘图/滤镜适合 ≤128×128 的小图

## [v0.7.0] - 2026-08-19

### 工具链增强
- **LSP 增强**：上下文感知补全（`time.` 等模块成员 / 文档变量 / 用户函数）、悬停文档（内置函数签名+说明）、
  跳转定义（`textDocument/definition`）、文档大纲（`documentSymbol`），serverInfo 升至 0.2.0
- **调试器增强**：条件断点 `breakpoint if (expr);`（条件为 bool 才暂停，checker 静态校验）；
  断点提示支持交互命令 `c` 继续 / `q` 退出（正常结束不报错）/ `l` 重列快照 /
  `p <expr>` 即时求值 / `w <name>` 监视变量 / `u <name>` 取消监视
- **prof 剖析增强**：新增**自耗时**（调用栈排除子调用）与**占比**列、
  调用图输出（调用方 → 被调方 → 次数，按次数降序）、总览行（总耗时 / 总调用次数）
- **fmt 格式化增强**：新增 `-c/--check` 检查模式（逐文件报告 OK/NEEDS FMT，有差异退出码非 0，CI 友好），与 `-w` 互斥
- **新增 `hone doc` 工具**：扫描 `fn`/`class` 定义及其上方 `//` 注释，生成 Markdown API 文档（支持多行签名、泛型名）
- **修复**：debug 构建主线程栈溢出——Windows 主线程默认 1MB 栈 + 未优化构建巨大栈帧，
  递归脚本（如 `bench/fib.hn` 的 fib(26)，深度仅 ~20 层）即爆栈；
  改为在 64MB 栈专用线程上执行命令逻辑（跨平台，虚拟内存预留），fib(26) 在 debug 构建下恢复正常

### 文档
- README（版本号 v0.7.0、fmt/doc 用法）、官网（版本徽章/页脚/HONE_VERSION 示例同步 v0.7.0）、
  changelog.html（v0.7.0 条目）

## [v0.6.8] - 2026-08-18

### 新增（新语法）
- **多返回值**：`return a, b, ...;` 一次返回多个值，运行时打包为列表，由解构赋值接收
- **解构赋值**：列表 `a, b = [1, 2]` 按位置绑定；字典 `{a, b} = dict` / 改名 `{a: x, b: y} = dict` 按键取出
- **列表/字典推导式**：`[expr for x in iter (if cond)]` 与 `{key: value for ...}` 一行生成集合
  （字典推导式键必须为 str，动态键用 to_str 转换）
- **可选链 `?.`**：`a?.b` 在 a 为 null 时短路返回 null，可链式 `a?.b?.c`；混合链 `a?.b.c` 与 JS 语义一致
- 新增示例 `examples/new_syntax_demo.hn`（44 项断言全部通过）

### 新增（TLS 根证书回退）
- **系统根证书回退**：内置 Mozilla 根证书校验失败时自动重连，回退到系统根证书
  （Windows ROOT 证书库 / Linux·Termux 系统 CA bundle，Termux 额外扫描 Android cacerts 目录），
  防内置根证书过期导致 https/wss 无法连接
- **用户信任根**：`HONE_CA_BUNDLE` 环境变量指定 PEM 文件（缺省 `~/.hn/ca.pem`），
  可信任私有 CA 的**根证书与中间证书**（自签名 / 内网证书场景）
- **中间证书注入**：用户 CA 文件中的中间证书参与链构建（自定义 rustls ServerCertVerifier），
  服务器即使不随链发送中间证书也能完成验证
- 覆盖 `http_get` / `http_post` / `http.request`、`smtp.send`（隐式 TLS 与 STARTTLS）、`ws.request`（wss://）
- 新增依赖 `rustls-webpki`（链构建）与 winapi `wincrypt`（Windows 证书库枚举）
- 新增端到端测试 `tests/tls_fallback.sh`（本地 CA 链 5 项断言全部通过）

### 文档
- hone.md（1.16-1.19 新语法、3.22 TLS 回退说明）、README、
  官网 docs.html（§7.7 新语法、内置函数参考 TLS 说明）/ tutorial.html（可选链交叉引用）/ changelog.html（v0.6.8 条目）同步更新

## [v0.6.6] - 2026-08-17

### 新增（性能分析）
- **`hone prof <script.hn>`**：以剖析模式运行脚本，统计每个用户函数的
  总耗时 / 调用次数 / 平均耗时（纳秒级计时），按总耗时降序输出控制台表格
- lambda 调用统一归入 `(lambda)` 条目；未调用任何用户函数时提示 `(未调用任何用户函数)`
- 剖析仅在 prof 模式下启用（`Interp.prof = None` 关闭），普通运行零开销
- 示例：`hone prof examples/fib.hn` 找出热点函数后，配合 `hone poop` 评估复杂度优先优化

### 文档
- hone.md（工具链命令）、README（命令清单）、
  官网 docs.html（§15 工具链）/ index.html（快速开始命令表）/ changelog.html（v0.6.6 条目）同步更新

## [v0.6.5] - 2026-08-17

### 新增（模块清单 hone.json）
- **模块清单 `hone.json`**（类似 package.json / Cargo.toml）：项目根目录集中声明
  `name` / `version` / `modules` 依赖（模块名 → 源码 URL 或本地路径）
- `hone get`（不带参数）：读取当前目录 `hone.json` 清单，批量下载全部模块到 ~/.hone/cache/
- `hone get <module> <url>`：下载单个模块并自动写入 / 更新 `hone.json` 清单
  （新建时 name 取目录名、version 默认 0.1.0）
- 模块 URL 非 http/https 开头按本地路径处理，直接读取源码（不联网），便于同仓库共享 hone_lib 等模块；
  清单缺失报 `error[H404]`、JSON 语法错误报 `error[H005]`、modules 为空报 `error[H005]`

### 文档
- hone.md（4.3.1 模块清单教程、工具链命令、进阶章节）、README（命令与导入章节）、
  官网 docs.html（§15 工具链）/ index.html（快速开始命令表）/ changelog.html（v0.6.5 条目）同步更新

## [v0.6.4] - 2026-08-17

### 新增（标准输入函数）
- **`input(prompt?)`**：从标准输入读取一行返回 `str`（去除行尾换行），可选提示文本（须为 `str`，
  如 `input("请输入名字: ")`）；EOF（管道关闭 / Ctrl+Z / Ctrl+D）抛 `error[H306]`，可用 try-catch 捕获降级
- **`read_int(prompt?)` / `read_float(prompt?)`**：读取一行并解析为 `int` / `float`，
  格式非法分别抛 `error[H006]` / `error[H007]`
- 新增错误码 **H306**（标准输入读取失败 / EOF），`hone explain H306` 可查说明
- 新增示例 `examples/input_demo.hn`（input / read_int / read_float / EOF try-catch 演示）、
  `examples/input_default_demo.hn`（输入默认值 / EOF 降级演示）

### 文档
- README（内置功能 + 错误码表）、hone.md（3.1 基础内置函数、3.1.1 输入默认值教程、5.2 错误码）、
  官网 docs.html（§8 内置函数参考 + 输入默认值教程）/ stdlib.html / changelog.html / examples.html（5.15 输入默认值示例）/ index.html 同步更新

## [v0.6.3] - 2026-08-16

### 新增（新语法）
- **continue 关键字**：跳过本次循环剩余语句，直接进入下一次迭代（与 `break` 相对，仅循环体内合法）
- **C 风格 for**：`for (init; cond; step) { ... }` 三段式循环，各段均可省略（`for (;;)` 无限循环配合 `break`）
- **do-while 循环**：`do { ... } while (cond);` 先执行循环体再判断条件（至少执行一次）
- **复合赋值**：`+=` `-=` `*=` `/=` `%=`（等价于 `x = x op y`，str 仅支持 `+=` 拼接）；
  **自增/自减**：`i++` `i--`（后缀返回旧值）、`++i` `--i`（前缀返回新值）
- **三元表达式**：`cond ? a : b`（右结合可嵌套，分支同型返回该类型）
- **空值合并**：`a ?? b`（a 为 null 时取 b，短路求值；null 来自 void 函数调用等占位值）
- **匿名函数 lambda**：`fn(x) { ... }` 作为一等值——可赋值给变量、作为参数传递、作为返回值（闭包工厂）；
  创建时按值捕获当前作用域变量（闭包），经变量名动态调用；参数支持类型注解与默认值
- **函数默认参数**：`fn f(a, b = 10) { ... }` 调用可省略尾部实参；
  默认表达式在调用时求值、可引用其前面的参数（`b = a * 2`）；必选参数必须位于默认参数之前；
  参数个数检查按「必选个数 ~ 总个数」区间；lambda 同样支持；`hone build --dll` 暂不支持（明确报错）
- **三引号原始字符串**：`"""..."""` 多行字符串，不做转义处理、原样保留换行与缩进
- 新增综合示例 `examples/new_syntax_demo.hn`（覆盖全部新语法，含 fmt 往返验证）

### 文档
- hone.md（1.4 控制流、1.11–1.15 新语法章节）、官网 changelog.html 同步更新

## [v0.6.1] - 2026-08-14

### 性能（解释器深度优化）
- 函数定义改为 `Arc<FnDef>` 共享：消除每次调用的 AST 深拷贝（递归 fib(26) 约 3.4s → 1.6s）
- while / for-in 循环作用域复用 + 原地赋值更新：消除每轮迭代的堆分配与 String 分配
  （2000 万次纯循环 71s → 42s；字符串拼接 20 万次 90s → 20s，拼接改为预分配容量、免 O(n²) 整串复制）
- 函数调用环境按参数个数预分配；解释器整体基准提升约 40%–78%，语义完全不变
- 移除 `once_cell` 依赖，改用标准库 `LazyLock`；清理 `FfiSig`/`FfiParam` 死字段（编译警告清零）
- 官网（`官网/`）兼容 Android 8.1（Chrome 61 级 WebView）：flex `gap` 改为 margin 间距

### 新增
- **alias 别名增强**：原名支持点号路径（`alias time.now as tnow;` 之后可 `tnow(...)` 调用），
  别名可叠加（别名再起别名）、可指向内置函数（`alias print as p;`）；
  新增示例 `examples/alias_demo.hn`

### 文档
- hone.md（4.4 别名、6.2 编译优化实测说明）、README（性能基准表 + alias 用法 + 版本号同步）、
  官网 docs.html / examples.html / changelog.html / index.html 同步更新

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
