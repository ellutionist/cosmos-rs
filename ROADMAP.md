# Cosmos 通用生态路线图

业务由 Lua 应用实现；Cosmos 提供可组合的系统、协议与数据处理能力。站点抓取、CDP 编排、媒体归档规则、验证提醒和业务任务状态不进入通用模块。此路线图不安排恢复已撤销的 CDP 插件。

## 模块归属

| 位置 | Lua 模块 | 边界 |
| --- | --- | --- |
| `cosmos/src/builtin/fs` | `cosmos.fs`、`cosmos.fs.path` | 异步文件/目录、元数据、路径、原子写入、文件锁；所有 Machine 自动安装 |
| `cosmos/src/builtin/time` | `cosmos.time`、`.instant`、`.deadline`、`.datetime` | 真实系统时间、休眠、单调计时、日期时间与时区；所有 Machine 自动安装 |
| `cosmos-network` | 现有 HTTP client/server、`cosmos.network.http.body`、`.url` | 流式收发、文件源、背压、代理与 URL，不实现播放会话或访问策略 |
| `cosmos-encryption` | `cosmos.encryption.digest`、`.hmac`、`.password`、`.random` | 增量摘要、签名验证、密码哈希、安全随机数 |
| `cosmos-encoding` | `cosmos.encoding.xml` | 保留顺序、属性、命名空间与混合文本的 XML 模型及序列化 |
| `cosmos-image` | `cosmos.image` | 图像句柄、编解码、缩放裁剪与像素操作；不包含封面规则 |
| `cosmos-text` | `cosmos.text.regex`、`.unicode`、`.chinese` | 正则、Unicode 与繁简转换；不包含演员/影片名称规则 |
| `cosmos-database` | 现有 MySQL / Redis | 连接资源控制、参数化查询、事务作用域 |
| `cosmos-system` | `cosmos.system.signal`、`.process` | 显式信号订阅和进程信息；子进程待实际使用需求确认 |
| `cosmos-logging` | `cosmos.log` | 结构化日志、级别、输出、轮转与刷新；兼容宿主日志设施 |

FS、datetime 不新增插件 crate 或 CLI feature。其他新增插件独立裁剪，延续 Module / Machine 注入方式。现有 `now_ms/now_us` 始终返回真实 Unix 时间戳，日期处理与单调时钟不改变它们的语义。

## 实施顺序

### 1. builtin 文件系统与时间（迁移核对通过）

- [x] 异步文件打开、分块读写、定位、截断、刷新与同步；共享句柄串行推进。
- [x] 目录迭代、metadata / symlink_metadata、创建删除、复制移动、canonicalize。
- [x] 路径组合/分解、原子写入、不覆盖操作与文件锁。
- [x] Instant / Deadline；UTC、固定偏移、IANA 时区的日期解析、格式化和转换。
- [x] 两种注入 Runtime、二进制/非 UTF-8 路径、并发与取消、VM 销毁、时区及非法输入回归。

文件 I/O 在 Tokio 工作线程执行，Lua 与持有 Lua 引用的任务仍固定在 Machine 的 LocalSet。取消不能撤销已提交的操作系统调用，接口必须说明是否关闭句柄及可能存在的文件副作用。

本阶段已对照 Librarian Rust 后端的导入、存储、归档与恢复流程核对：

- 纳秒 mtime 使用整数字符串往返，支持同一毫秒内的文件变化检测；日期保留对应纳秒精度。
- 组件级路径前缀/相对路径检查、canonicalize、符号链接元数据和 try_exists 可组合原有根目录边界检查。
- 排他创建、禁止覆盖重命名、原子写入、权限保留和跨设备错误分类支持应用编排移动、暂存与回滚。
- 数据读取、文件锁、真实时钟、单调计时及原后端使用的日期格式已覆盖，取消与 VM 销毁有独立回归。

核对和验证使用本地临时文件及两种注入 Runtime，未运行真实 NAS/SMB 联调；不把本阶段完成等同于整个后端迁移完成。HTTP 大文件响应、XML、图片等仍按后续阶段推进。

### 2. HTTP 流式传输（迁移核对通过）

- [x] 客户端和服务端逐块 body，兼容现有字节串接口。
- [x] 文件读取源、offset/length、上传与下载落盘的组合路径。
- [x] 背压、超时、断开和取消后的资源释放；大文件传输内存有界。
- [x] HTTP 标准范围/缓存辅助；应用自行决定鉴权、票据和文件访问规则。

共享读取源只针对实际文件与 HTTP 使用方设计，必要的跨 crate 句柄隐藏内部表示，不提前抽象所有网络协议。接口见 [HTTP 文档](cosmos-network/http.md)，完整验收见 [http_stream.lua](examples/http_stream.lua)。

本阶段已对照 Librarian Rust 后端核对：

| 迁移需求 | 对应能力与验证 |
| --- | --- |
| `storage.rs` 的文件播放、`updates.rs` 的安装包下载 | 同一打开文件生成响应，64 KiB 分块、准确长度、单 Range / 416 / HEAD / Content-Range；MIME、附件名称和访问控制保留给应用 |
| 图片下载与受限外部响应 | 客户端 stream、max_body、原请求截止时间、逐块落盘或显式受限收集；80 MiB 本地上传/下载通过 |
| 流式输入/输出组合 | 文件 offset/length、异步生产函数、请求流直传及延迟回调转发，HTTP/1.1 与 HTTPS HTTP/2 均实际验证 |
| 背压、取消与资源回收 | 容量 1 的发送队列、慢接收者、显式关闭、读取取消、原生短体/尾部字段、Machine 销毁；两种注入 Runtime 通过 |
| 通用边界 | 条件请求与弱/强 ETag；不引入播放会话、下载票据、站点规则或业务路由 |

单 Range 与文件响应已满足本阶段迁移需要；多范围请求回退完整响应，日期 If-Range 因无法保证强验证而回退完整响应。一次性上传遇到要求重放 body 的重定向会明确报错。当前不新增自动解压、HTTP/SOCKS 代理或服务排空；后两者仍按第 5、6 阶段推进，不能把本阶段完成视为整个后端已可迁移。文件读取与取消沿用 builtin FS 语义，尚未进行真实 NAS/SMB 联调。

### 3. 密码与随机数（迁移核对通过）

- [x] 增量 digest / HMAC 与验证，保留现有一次性摘要接口。
- [x] Argon2 密码哈希及验证、安全随机字节与无偏整数。
- [x] 耗时计算在 Rust 工作线程执行，限制并发与输入规模，不发送 Lua 到其他线程。

本阶段扩展现有 Encryption 插件，不新增 crate 或 CLI feature；接口和资源边界见 [Encryption 文档](cosmos-encryption/README.md)，实际用例见 [crypto.lua](examples/crypto.lua)。

| 迁移需求 | 核对结果 |
| --- | --- |
| `auth.rs` 的密码存储/校验 | 默认 Argon2id v19 / m=19456 / t=2 / p=1，标准 PHC 与原生 RustCrypto 双向互操作通过；应用保留密码策略与登录状态 |
| `auth.rs` / `playback.rs` 的令牌 | OS 安全随机字节与无偏精确整数；可组合现有 Hex/Base64，长度和过期策略属于应用 |
| `storage.rs` 的 HMAC 图片票据 | SHA-256 等 HMAC 增量/一次性输出与完整认证码校验，RFC 2202 / 4231 向量通过；票据拼装和过期判断不进入模块 |
| `updates.rs` / 导入与归档摘要 | 保留旧 SHA 同步入口及 require 初始化兼容，新接口在工作线程计算，支持分块文件摘要；8 MiB 二进制文件及错误后状态检查通过 |
| 单线程响应与资源预算 | 本地准备参数，纯 Rust 工作线程计算；最多 2 个计算作业 / 32 个已接纳作业，取消不会提前归还运行额度，两种 Runtime 和 VM 销毁回归通过 |

不可信 PHC 在分配前检查算法、版本、字段与成本上限；密码/派生输出和 Argon2 工作区显式清零，Lua 字符串不承诺擦除。已开始的原生计算不能强制中断，取消后继续受全局预算限制直到结束。现有 RSA/Ed25519 和 MySQL 明文/TLS 认证回归保持通过。此阶段不引入登录策略、JWT、密码加密或密钥派生接口。

### 4. XML 与图片（迁移核对通过）

- [x] XML 文档读取、构造、访问与序列化，不经 JSON 中转。
- [x] XML 大小、深度、节点数、并发预算及取消/VM 销毁回归。
- [x] 图像解码、缩放、裁剪、像素访问和编码；文件读写由 FS 组合。
- [x] 图片像素、内存限制，以及耗时图像任务取消后的资源回收。

XML 本阶段扩展 Encoding 插件，接口见 [XML 文档](cosmos-encoding/xml.md)，实际用法见 [xml.lua](examples/xml.lua)。XML 已单独确认提交；图片本批作为独立 cosmos-image 插件交付。

| XML 迁移需求 | 核对结果 |
| --- | --- |
| `importer/nfo.rs` 的结构读取 | 根元素、本地名/URI、重复子元素、有序属性、递归文本、Unicode 与字符引用；不折叠为 JSON 对象 |
| `archiver/translation_retry.rs` 的局部文本修改 | 原文位置、精确替换/删除、缺失元素插入、空根展开；未修改的注释、属性、空白及其他子树保持原始字节 |
| 混合内容与命名空间 | 文本/CDATA/注释/PI 分别保留顺序；跨文档移动与子树序列化补齐继承绑定，正确处理默认命名空间清除 |
| 文件发布与安全边界 | 组合 builtin FS 读取/原子写回；拒绝 DTD/外部实体、非法编码/声明、过深/过大文档、稀疏与循环输入 |
| 运行模型与资源 | 拥有型文档/节点、纯 Rust 工作线程、2 个计算/16 个已接纳作业；两种 Runtime、取消、VM 销毁、错误后恢复通过 |

已覆盖本阶段原后端所需 XML 基础能力。未转义 `&` 等历史文件的修复策略、具体 NFO 字段与文件备份/回滚由应用组织；不增加业务格式专用 API。严格 XML 1.0/UTF-8 子集不加载 DTD，也不提供 XPath/XSD 或其他编码转换。

图片模块接口见 [Image 文档](cosmos-image/README.md)，用法见 [image.lua](examples/image.lua) 和 [HTTP 组合示例](examples/image_http.lua)。CLI 的 image feature 默认关闭，可单独启用或通过 full 启用；没有文件/网络依赖。

| 图片迁移需求 | 核对结果 |
| --- | --- |
| `archiver/images.rs` 的下载图解码与输出 | PNG/JPEG/WebP 内容识别、解码前尺寸/像素/分配检查；JPEG 显式 quality=95，PNG/WebP 无损编码；下载仍组合 HTTP |
| 封面裁剪与 `storage.rs` 的缩略图 | 严格矩形裁剪、精确尺寸缩放、等比 thumbnail；允许放大时传 upscale=true，可组合原生 thumbnail 行为 |
| `watermark_detector.rs` 的灰度与像素 | nearest/triangle 等五种采样、luma8/rgb8/rgba8 像素平面、按点读取、批量矩形写入；模板与 Sobel/评分规则留在 Lua |
| 原生格式与图像语义 | JPEG 质量、透明背景选项、16 位 PNG 转 RGBA8、可选 EXIF 方向、旋转翻转与原图保持不变都有回归 |
| 资源、并发与发布 | 解码头部与尺寸限制、重采样中间缓冲预检、受限输出、2 个计算/16 个已接纳作业；两种 Runtime、取消/VM 销毁、FS 原子写回与真实本地 HTTP 流水线通过 |

本阶段已覆盖原后端需要的图像基础能力，不移入海报比例、站点选择、徽标模板或水印判定策略。内部使用 RGBA8，不保留原始位深/元数据或动画；WebP 仅无损编码。原生编解码器分配预算属于其支持范围内的限制，不承诺进程 RSS 硬上限；大图像及应用保留的多个版本仍由应用控制。

### 5. 文本、URL 与时间进阶

- [x] 编译型正则、Unicode normalization 与大小写、繁简转换（迁移核对通过）。
- [x] URL 解析、合并、查询参数，与已有 URI 百分号编码区分职责（迁移核对通过）。
- [ ] HTTP / SOCKS 代理能力；直连规则由应用配置。
- [ ] 补齐日期日历运算和按需扩展的格式支持；继续放在 builtin time。

文本部分作为独立 `cosmos-text` 插件交付，接口见 [Text 文档](cosmos-text/README.md)，实际用法见 [text.lua](examples/text.lua)。CLI 的 text 默认关闭，单独启用或通过 full 启用，无其他插件依赖。

| 文本迁移需求 | 核对结果 |
| --- | --- |
| `importer/nfo.rs` 的域名过滤与编号提取 | 编译复用、大小写标志、转义、捕获组及原文范围；条件替换由可挂起 Lua 回调组合，不内置媒体规则 |
| `archiver/translation.rs` 的文本片段清理 | 内联标志、跨行/大小写、惰性量词；原后端实际模式的捕获与字节位置逐项对照 Rust 引擎通过 |
| 名称/分类筛选的 Unicode 处理 | NFC/NFD/NFKC/NFKD、上下文相关小写、大写、Unicode 空白、scalar 计数与截断；执行顺序保留给应用 |
| `actor_names.rs` 等繁简处理 | 沿用 ferrous-opencc 0.4 的内嵌 t2s/s2t 词典并按方向缓存，词组、歧义字和 NUL 原生结果对照通过 |
| 资源与运行模型 | UTF-8/输入输出/捕获/分段预算、2 个计算/16 个已接纳作业；两种 Runtime、取消、VM 销毁与回调挂起通过 |

本批已满足原后端的文本基础能力；Rust regex 不支持环视/反向引用，现有迁移模式不依赖它们。len/truncate 按 Unicode scalar 计数，不提供字形簇或 locale case folding；繁简转换不承诺可逆。已开始的单次原生搜索/OpenCC 转换不能强制中断，仍持有计算预算到结束；OpenCC 另限 1 MiB 输入并保留词组上下文。URL 由下面的独立批次补齐；代理与日历进阶尚未实现，不把文本完成视为第 5 阶段全部完成。

URL 本批扩展 Network，不新增插件或 feature。接口见 [URL 文档](cosmos-network/url.md)，完整用法及真实 HTTP 重定向见 [url.lua](examples/url.lua)。

| URL 迁移需求 | 核对结果 |
| --- | --- |
| `actor_urls.rs`、`operations/tasks.rs` 的组件检查和规范化 | scheme/host/port/userinfo/path/fragment 访问、不可变修改与清除；站点白名单、路径模板和编号校验仍由应用组织 |
| `scraper/model.rs` 的资源地址补全 | 标准 parse/join，支持层级、点段、协议相对、query/fragment 与绝对引用；结果逐项对照 Rust url 通过 |
| `scraper/filters.rs` 的分页/筛选查询 | 有序重复键、空值、query_pairs、整组重建和保留既有查询字节的追加；表单加号规则与 URI 百分号编码区分 |
| `verification.rs` / WebSocket 地址处理 | 标准 origin 比较、主机与端口重写、IDNA 和 IPv4/IPv6；不增加 CDP 或业务 URL 规则 |
| HTTP 集成与资源 | 删除手写重定向拼接；真实多跳点段/query/fragment/协议相对跳转、跨源凭证剥离、拒绝隐式 userinfo、64 KiB/1024 项边界、GC 后持有及错误恢复通过 |

本批已覆盖原后端使用的 URL 基础能力，并在两种 Runtime 和默认/full/无插件 CLI 上验证。解析沿用 WHATWG 规范化和非规范百分号处理，不提供站点验证策略；URL 与组件长度最多 64 KiB，列表最多 1024 项。HTTP 只接受 http/https，不发送 fragment，不把 userinfo 自动变为认证头。代理和日期日历仍待后续用户指令，本批完成后停下供检查。

### 6. 数据库、服务生命周期与日志

- [ ] MySQL 参数绑定/预编译、活跃连接上限、等待队列、获取超时与事务作用域。
- [ ] 显式信号订阅，HTTP 停止接收与排空，应用组织任务停止顺序。
- [ ] 结构化日志、输出轮转、flush；不抢占宿主 Runtime 或全局日志配置。
- [ ] 迁移、路由和请求校验优先作为 Lua 工具层按实际复用需求补充，不引入 ORM 或业务服务框架。

## 每阶段验收

按 module 分批交付，每个模块先逐项核对迁移/功能需求并完成验证，再停下来让用户检查，收到用户提交指令后再提交，不自动连续实现下一模块。

- 实际运行对应 `examples/`，覆盖取消、错误后继续使用及相关回归。
- 工作区测试、外部宏展开、严格 Clippy、格式、feature 裁剪与文档链接检查。
- 注释和文档用中文，错误/日志/断言/CLI 文案用英文，遵守 AGENTS 的可见性与桥接约束。
- 更新本文件、PROGRESS 和 HANDOFF；只有用户明确要求时 commit / push。
