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

### 3. 密码与随机数

- [ ] 增量 digest / HMAC 与验证，保留现有一次性摘要接口。
- [ ] Argon2 密码哈希及验证、安全随机字节与无偏整数。
- [ ] 耗时计算在 Rust 工作线程执行，限制并发与输入规模，不发送 Lua 到其他线程。

### 4. XML 与图片

- [ ] XML 文档读取、构造、访问与序列化，不经 JSON 中转。
- [ ] 图像解码、缩放、裁剪、像素访问和编码；文件读写由 FS 组合。
- [ ] 大小、深度、像素和内存限制，以及耗时任务取消后的资源回收。

### 5. 文本、URL 与时间进阶

- [ ] 编译型正则、Unicode normalization 与大小写、繁简转换。
- [ ] URL 解析、合并、查询参数，与已有 URI 百分号编码区分职责。
- [ ] HTTP / SOCKS 代理能力；直连规则由应用配置。
- [ ] 补齐日期日历运算和按需扩展的格式支持；继续放在 builtin time。

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
