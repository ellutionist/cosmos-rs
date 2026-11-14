# HANDOFF — cosmos-rs（交给 Codex）

> 写给接手的 agent：读完这份再动手。约定优先于个人偏好；用户是中文母语、C++ 背景、对分层和分配很敏感。

## 0. TL;DR

- 项目：LuaJIT + Rust 重写版网络框架，路径 `/Users/ellutionist/code/ellutionist/cosmos-rs`。
- WebSocket、桥接分层重构和 VM 生命周期修复已提交为 `c030f5d`。
- 已按用户要求将统一返回值桥接、分层修复和冗余清理纳入本次本地提交；最新验证与踩坑见 `PROGRESS.md`。
- 异步边界采用 `PendingOp::poll_push`，只保留 `PushLua` 转换协议；不采用返回值闭包装箱方案（见 §5）。
- HTTP/WebSocket 回调已改用 `LuaFunction::call<A, R>`；userdata 借用失败只返回 Err，最外层 trampoline 统一抛 Lua 错误。
- 外部嵌入使用 `Machine::new(tokio_handle)`，插件实现 `Module` 并通过 add_module 注入；插件包括 `Network`、`Encoding`、`Encryption`、`Database`，CLI 默认启用四个对应 feature。
- UDP 已作为 `cosmos.network.udp` 接入 Network，支持 IPv4 / IPv6、报文边界、二进制、域名、并发收发、超时/取消与关闭唤醒；不再属于待做。
- `cosmos.database.mysql` 已完成 lua-resty-mysql 适配与按 client 隔离的空闲连接池；真实 MySQL 8.4.5 的明文 TCP / TLS 回归通过。认证异常与大包另有离线协议测试；尚未用真实 MariaDB 验证，不支持服务端预编译语句。
- `cosmos.database.redis` 已适配 lua-resty-redis 0.34，提供 RESP2、pipeline、事务、Pub/Sub 与 client 池。数据库 crate 的 `mysql` / `redis` feature 默认全开；CLI 的 `database` 是两者合集，也可单独选择，依赖随数据库关闭。Redis 8.10.3 的真实回归通过，配置用 COSMOS_REDIS_URL。
- Lua / C 模块路径分别沿用 LuaJIT 的 `LUA_PATH` / `LUA_CPATH` 与原生默认值（`;;` 展开默认路径）；CLI 可重复传入 `--path` / `--cpath` 前置模板，宿主使用 `Machine::prepend_path` / `prepend_cpath`，不修改进程环境变量。
- CLI 的 build.rs 为 macOS / Linux 最终二进制保留并导出 Lua C API；该链接参数不会传播给外部 Machine 宿主。宿主需自行配置，见 README 的 C 扩展节；cosmos 的 Rust ffi 仍私有。
- 交流、注释、文档一律**中文**；**不要自动 commit / push**。

## 1. 环境与结构

```
cosmos-rs/
  cosmos/            # 运行时 + 桥接层：machine / ffi / userdata / light_thread / stack / libs / builtin
  cosmos-macros/     # #[userdata]、#[derive(FromLua/PushLua)]，只生成 Rust 调用/转换组合
  cosmos-network/    # 功能模块：tcp/{stream,tls} / udp / http / websocket + bridge.rs（叶子转换）
  cosmos-encoding/   # Encoding 插件：Hex、Base64、URI、JSON / TOML / YAML
  cosmos-encryption/ # Encryption 插件：SHA、RSA-OAEP、Ed25519
  cosmos-database/   # Database 插件：按 feature 选择 mysql / redis，内嵌 Lua 协议与 client 池
  Cargo.toml         # 虚拟 workspace，默认成员为 cosmos-cli
  cosmos-cli/        # clap 命令行入口，注入 Runtime Handle，默认 network/encoding/encryption/database
  cosmos-network/examples/machine.rs # Rust 嵌入示例，根 examples/machine.lua 验证参数与模块入口
  examples/*.lua     # 全部端到端用例
  cosmos-network/lua/ # 内嵌 Lua 源码（include_str! 随二进制编译）
```

- LuaJIT 2.1（Lua 5.1 C API），Tokio >= 1.49；宿主可注入单线程或多线程 Runtime 的 Handle，
  Machine 的 LocalSet 始终在宿主线程驱动 Lua 协程，Machine 与 Lua 协程均为 `!Send`。
- 依赖：hyper 1 + hyper-util（client/server，h2 走 ALPN）、rustls+ring、fastwebsockets（upgrade feature）。
- 重构前基线：`c030f5d feat: add WebSocket support and refactor Lua bridge layers`；最新提交号通过 `git log -1` 查询。

## 2. 硬性约定（详见 `AGENTS.md`）

1. **功能模块零 FFI**：tcp / udp / tls / http / websocket 里不出现 `cosmos::ffi`、不直接调 `lua_*`、不做裸栈操作。
2. **桥接 = 组合**：基层类型各实现 `PushLua` / `FromLua`，结构体靠 derive，枚举靠 derive，
   多返回值靠元组/枚举字段顺序。**禁止**把命令式栈操作换个文件存放（用户明确说过那是"马甲"，没有意义）。
3. 裸 FFI 仅限 cosmos 运行时/桥接层；network bridge 与宏生成代码均只能使用受控上下文，不能引用 ffi。
4. 不做死抽象；公共层要有明确复用点。
5. 任何行为改动必须实际跑 `examples/` 对应用例（含回归），不能只看编译通过。
6. 只有用户明确要求才 commit / push。

## 3. 工作区现状

- time/thread 内建功能已集中到 `cosmos/src/builtin`，由 `builtin::open` 统一初始化。
- time 已改为 `#[userdata(name = "time")]` 自动注册，入口统一为 `cosmos.time.*`，
  时间函数不再放在 cosmos 根表，examples 与类型化回调测试已同步迁移。
- `cosmos.sync` 已实现：有界 channel、通知、信号量和 mutex，见下文接口与回收语义。
- 模块注册已按 userdata 声明路径自动组装父表；手写 Sync 与 TCP 聚合表已删除，sync 构造改为各类型的 `.new(...)`。
- 上述 sync 与模块注册改动已提交为 `b093478`，未 push。网络文件已按模块层级整理为
  `tcp/mod.rs`、`tcp/stream.rs`、`tcp/tls.rs` 及 `websocket/mod.rs`、`websocket/conn.rs`，
  Rust 引用同步更新，Lua require 路径保持原样；目录整理已提交为 `c4a907e`。

- 本次命名整理提交：`Thread` → `LightThread`，`thread.rs` → `light_thread.rs`；当前统一类型为 `cosmos::LightThread`（实现模块 `light_thread` 为私有），Lua 类型表为 `cosmos.LightThread`。
- Lua 状态指针（含宏生成代码）已从 `L` 改为 `lua_state`，未使用参数为 `_lua_state`。
- 其余 Clippy 诊断已修复，公开裸指针桥接 API 已明确 unsafe 契约；严格 Clippy、构建、15 项测试和 examples 通过。

- 已提交基线：WebSocket、桥接分层重构、VM 生命周期修复与项目约定。
- 本次提交范围：统一 `PushLua`、类型化 Lua 回调、枚举 `FromLua`、userdata fallible 借用、统一错误边界、冗余清理与回归记录。
- 外部演示 crate 已移除，主程序构建不再依赖工作区外的源码目录。
- Machine 管理 VM/LocalSet，Runtime 只注入 Handle；网络作为 Module 插件加载，CLI 已迁移。
  外部演示删除改动一并提交为 `a9ce8a8`。
- HTTP client 已改为实例独占连接池，构造时固定 TLS 配置，调用迁移为 new + 冒号 request。

## 4. 最近完成的工作

### TOML / YAML（2026-11-14）

- Encoding 新增 cosmos.encoding.toml 与 cosmos.encoding.yaml，CLI 沿用 encoding feature，无需新插件。依赖 Rust toml 1.2 与 serde-saphyr 1.3，不使用 C YAML 库；不改 Machine、宏或裸栈接口。
- JSON 原有 Lua 输入/输出抽到私有 value 模块，与新格式共用文档值、容器标记、循环检查、UTF-8、节点/字节/深度预算和元表输出。JSON 继续使用 sonic 验证与编解码，原 encode/decode、lossless_numbers、number 路径不变。新增 formats.lua 只提供容器与数字类型别名和 decode 元表参数适配，初始化幂等。
- TOML 根必须是字符串键表，不支持 null，日期时间使用 toml.datetime userdata 保持类型，text / tostring 读取，new 构造。使用原生 DeTable / DeValue，避免 Serde 的 $__toml_private_datetime 私有键与普通配置冲突；整数限 i64，小数为 f64，支持 inf/nan。
- YAML 支持字符串键映射、数组、null、锚点、别名及合并键。重复键、非字符串键、自定义标签、多文档、循环别名与非有限数返回 nil+err；yes/off 保持字符串。不启用文件包含或环境替换。Serde 对超范围整数可能转 f64，先用 granit 事件读取原始标量并验证 i64/u64 范围，再反序列化，不能只在 visit_f64 后判断是否丢精度。
- toml.number / yaml.number 与 json.number 是同一张 userdata 类型表，对应 require 入口为显式别名。安全整数为 Lua number，其他受支持整数保留精确 userdata；新格式只接受范围内的精确整数 userdata，JSON 精确小数需明确改用 Lua number。YAML 编码 TOML 日期时间时输出字符串。
- 三种格式最多 64 层容器、100 万节点、64 MiB 输入/输出；YAML 再限制锚点/别名各 10000、别名重放事件 100 万，展开后继续计入值预算。TOML 原生解析器保留默认递归限制，转换时再执行公共限制，不启用 unbounded。保留普通线程栈上的深层与万层输入拒绝回归。
- 新增 examples/toml.lua、yaml.lua 与插件/CLI feature 回归，文档采用与现有格式一致的短说明和可运行代码，详细类型与限制放 encoding README。本轮不自动提交或推送；性能测量、配置格式原文保留仍未安排。

### UDP（2026-11-14）

- 单一 Udp userdata 同时提供静态 bind / connect 与实例收发，不新增聚合空壳或 Lua 包装。通过 Network 注册为 cosmos.network.udp，CLI 沿用 network feature；Rust 类型仅 crate 内可见。
- 支持 IPv4 / IPv6、域名、二进制与空报文、来源地址和广播开关；发送最多 65507 字节，接收缺省上限相同。接收使用完整普通 IP 报文缓冲，再检查调用者上限，超限丢弃整份并返回 nil+buffer overflow，不返回截断内容或 TCP 式 partial。IPv6 地址保留 scope ID。
- Udp 是 Rc 共享句柄，异步 self 通过 Clone 转换，RefCell 只在取底层 Rc 或关闭时短暂借用。收发可跨协程并行，超时和取消保留 socket；close 幂等、优先通知所有挂起操作返回 closed，等待者退出后释放最后的底层引用。无等待时 GC 直接关闭，Machine 销毁时取消在途操作并回收端口。
- bind / connect 的初始超时同时覆盖解析/初始化和之后的收发；sendto 在单个期限内解析并发送。connect 不保证对端可达，也不做应用握手；send 用默认目标，sendto 仅用于未连接 socket，并筛选与本地地址族相同的解析结果。settimeout 只影响之后的操作，<=0 清除。
- 将原 TCP 中的 with_deadline / timeout_from_ms 移到私有 deadline 模块，现有 TCP/TLS/socket/HTTP/WebSocket 行为不变，UDP 不依赖 TCP 实现。核心仅补三元组 PushLua，复用二元组组合来返回 data/host/port；没有新增裸 FFI 或开放运行时实现。
- examples/udp.lua 与集成测试覆盖 IPv4 / IPv6、两种 Tokio 宿主、来源过滤、边界/空包/二进制、超限后恢复、非法参数/地址、并发收发、多等待者关闭和取消/GC/VM 销毁。后续按需补多播、socket 高级选项及长时间压力验证；本轮不自动提交或推送。
- examples/udp_server.lua / udp_client.lua 提供独立的双进程回显示例，先启动服务端，再用相同端口运行客户端；根 README 分别展示两端代码，udp.lua 继续保留为完整单进程回归。

### sonic-rs JSON（2026-11-14）

- `cosmos.encoding.json` 随 Encoding 安装，CLI 默认可用，Machine 宿主仍显式注入插件。JSON 功能只组合受控转换，不引入裸 FFI；ClickHouse 原生 TCP 按用户要求留在待做。
- encode / decode 的数据错误返回 nil+err，null 复用 cosmos.null。连续整数键为数组、字符串键为对象、普通空表为对象；array / object 标记和解码共享元表保留容器形状。拒绝稀疏/混合键、循环引用、无效 UTF-8、非有限数和无关 userdata。
- 启用 sonic arbitrary_precision。大整数及溢出/非零下溢浮点保留 RawNumber userdata，普通小数默认 Lua 双精度；lossless_numbers=true 保留所有数字原文，json.number.new 可直接构造，text / tostring 读取原文，编码输出数字字面量。
- 普通测试线程栈上，sonic 的调试版递归 DOM 解析会栈溢出；改为严格 UTF-8 与容器深度预检、sonic 完整惰性验证、容器遍历后构造值。编解码统一最多 64 层、100 万节点、64 MiB 输入/输出，超限不破坏 VM。不得通过增大测试线程栈掩盖问题。
- 核心桥接增加全类型键遍历、调用内表身份、原始元表字段读取、数组写入及结束时设置元表；底层操作仍全部私有，字段借用保活与异常回滚有外部插件回归。重复安装 Encoding 使用 Lua 初始化标记，防止 CLI 与 Database 同时注入时重复包装 decode，丢失参数或容器形状。
- sonic 的 SIMD 后端按编译目标选择，通用路径仍是 sonic，不自动切换 serde_json；不强制 target-cpu=native。新增 examples/json.lua 和独立文档；后续再做真实负载性能测量，不以 sonic 本身的基准宣称当前 Lua 桥接吞吐量。

### Redis 与按数据库划分的 feature（2026-11-14）

- 基于本地 lua-resty-redis 0.34 / 74d0c11 适配到 `cosmos.database.redis`，保留 BSD 许可。协议、pipeline、事务和 Pub/Sub 用 Lua；Rust 只装配插件、解析 URL，共用 mysql / redis 的 URL 叶子转换。无 ngx / resty 注入或 Lua FFI。
- `mysql` / `redis` feature 同时控制代码、注册、内嵌源码和专用依赖，默认全开；只有 Redis 时只安装 Network，不安装 Encoding / Encryption，全关时 Database 注册为空操作。CLI 的 `database` feature 展开为 mysql + redis，依赖声明关闭 crate 默认 feature，避免单库选择被默认值重新开启。
- Redis 连接采用已有 Socket，支持 TCP / Unix / TLS、AUTH 与 ACL、SELECT、三段超时、RESP2 嵌套错误与 null。超过 Lua 精确整数范围的 int64 回复使用字符串；限制请求/回复字节、数组长度、嵌套、pipeline 数量和 Pub/Sub 缓冲，pipeline 共用回复预算。
- 池为 client 级，空闲数量与过期语义和 MySQL 一致，租借先 PING，归还后旧句柄失效。拒绝未完成 pipeline、订阅、MULTI、WATCH、改变会话及失败/取消连接回池。HELLO 3、MONITOR、复制流、分片订阅和改变回复模式的 CLIENT 子命令明确拒绝。
- 兼容 Redis 原生命令命名：`redis.client(opts)` 是池工厂，`conn:client(...)` 是 CLIENT 命令；通用显式调用用 `execute`，保留 `conn:command(...)` 为 Redis COMMAND。订阅按频道集合跟踪，重复订阅不多算，取消订阅按请求数读完 ACK；RESET 清理本地订阅/事务状态，但该连接不再回池。
- Redis 示例在独立前缀下配置 TTL、结束清理，并创建/删除自己的临时 ACL 用户；用户允许使用测试服务器，仍不修改现有用户或清空数据库。默认测试只连本地协议服务端，真实回归显式使用 COSMOS_REDIS_URL 与 --ignored。
- 数据库 README 改为总览和 feature 说明，MySQL / Redis 细节分别在 mysql.md / redis.md；示例为 examples/mysql.lua 与 examples/redis.lua。后续优先保持两种池的语义一致，RESP3、分片订阅及全局/有界活跃池不在本轮范围；不自动提交。

### MySQL 驱动与插件（2026-11-14）

- `cosmos-database` 只公开 `Database`，自动注入 Network / Encoding / Encryption，注册 mysql 的 URL 转换并加载内嵌 Lua。父子模块表继续由声明路径组装，外部宿主无需裸栈或额外 Lua 文件路径。
- MySQL 协议基于本地 lua-resty-mysql 0.29 / 3af1b39 适配，保留 BSD 许可；不注入全局 ngx / resty 兼容层。支持 TCP/Unix、原地 TLS、四类认证、NULL、BIGINT 字符串、多结果集、紧凑数组和 SQL 十六进制字符串引用。
- `mysql.new()` 是独立连接；`mysql.client(options_or_url)` 是显式池，pool_size 只限制空闲数，idle_timeout 控制过期；不实现命名全局池或 backlog。租借先 COM_PING，归还转移到新句柄表；旧句柄失效。禁止未读结果、未完成事务和失败/取消连接回池，调用方自行恢复其他会话设置。
- 真实 MySQL 握手发现旧 TCP 发送用了 LuaStr，短串 UTF-8 有损转换会破坏认证，长串则拒绝非 UTF-8。新增核心 LuaBytes，短串复制、长串锚定，socket 与旧 TcpWriter 统一使用，读写测试改用真正的非 UTF-8 字节。
- 借用回归还暴露 `Option<T>` 重新构造 Value 丢失字段的 scope / temporary，重入 GC 后可能悬垂；组合转换改为传递原始 Value，外部插件用表字段删除与内存覆写验证。
- 协议层验证序号、字段长度和逻辑包上限，支持 16 MiB 分帧与空终止帧，区分 0xfe 行前缀与 EOF；异常即丢弃流，完整 ERR 包保留连接。服务端只做会话临时表测试，不修改业务数据。
- 真实回归通过环境变量 COSMOS_MYSQL_URL 显式启用，默认工作区测试不连接外部数据库。见 examples/mysql.lua 与 cosmos-database/README.md；不要将测试凭据写入代码或文档。

### 适配基础与独立插件（2026-11-14）

- `cosmos.null` 是空指针 lightuserdata，与 `ngx.null` / `cjson.null` 一致，保留表字段与数组位置；桥接使用 `Null` / `ValueKind::Null`，与 `Nil` 区分。
- `cosmos.network.socket` 提供顺序 `connect/send/receive/close`、Unix domain socket 和 `starttls/starttls_server`；TLS 在已有流上升级，回放 BufReader 的预读字节。该接口的 I/O 失败、超时和取消均丢弃连接，旧 split 接口继续保留原有超时语义。
- `tcp/transport.rs` 的 PlainIo 组合 TCP/Unix 与预读前缀，`stream.rs` 的私有表示 Duplex 复用旧读写状态机。TLS 写入完成前须在同一 deadline 内 flush，避免已接收明文但仍未发完密文导致停滞；重复 split 返回 nil+错误。
- `cosmos-encoding` 与 `cosmos-encryption` 仅公开 Module 插件类型，功能通过受控转换与宏注册。同步二进制参数复用新增的 `&[u8]: FromLua`；异步参数仍须拥有数据或独立锚点。
- Encoding 提供 Hex、标准/URL-safe Base64、严格 URI 百分号编解码及 JSON/TOML/YAML 文本转换。Encryption 提供二进制 SHA-1/256/512、SPKI/PKCS#1 公钥的 RSA-OAEP-SHA1/256、标准 Ed25519 与 MariaDB 密码材料派生。算法实现均为 Rust，不引入 OpenSSL。
- 示例见 `examples/null.lua`、`network_socket.lua`、`encoding.lua`、`encryption.lua`；各插件使用说明见各自 README。MySQL 驱动的后续进度见上一节，不重新引入全局 ngx 兼容层。

### 4.1 WebSocket（服务端复用 HTTP 端口）

- 服务端不单独 listen：HTTP handler 对选定 path 返回
  `{ websocket = function(conn, req) ... end }`。
- `cosmos-network/src/http/server.rs`：
  - `upgrade_websocket()`（约 264 行）用保留的请求 `Parts` 调 `fastwebsockets::upgrade::upgrade`，
    返回 101；非 upgrade 请求返回 400；HTTP/2 无 Upgrade 机制，同样走 400。
  - 连接驱动改为 `serve_connection_with_upgrades`；`handle()` 里给 ws 分支重建一份请求 userdata。
- `cosmos-network/src/websocket/mod.rs` 负责客户端入口和连接选项，`websocket/conn.rs` 负责连接对象和服务端帧循环：
  - `Ws.connect(url, opts)`：`ws://` 明文、`wss://` 走 TlsContext（缺省公有根，强制 ALPN `http/1.1`）；
    opts = `{ headers, timeout, ctx, max_message }`，超时覆盖 DNS+TCP+TLS+握手并作为连接初始读写超时。
  - `WsConn`（`cosmos.network.websocket.conn`）：`send`（校验 UTF-8）/`send_binary`/`ping`/`receive`/
    `close`/`state`/`settimeout`；`receive` 返回 `("text"|"binary", data)`，关闭 `nil,"closed"`，
    超时 `nil,"timeout"`（连接可继续用）。ping 自动 pong、close 自动回 close。
  - 帧流放在 `Rc<RefCell<Option<FragmentCollector<...>>>>`：handler 协程结束后服务端强制 close；
    协程被取消时随 userdata 回收关闭。
  - `run_conn()` 通过 `handler.call::<_, ()>((conn, request)).await` 跑类型化 Lua 回调，运行时管理协程锚点。
- 上限：`ServerOptions.ws_max_message` / 客户端 `max_message`，缺省 64 MiB。
- 注意：`FragmentCollector<S>` 包的是**流** `S`，不是 `WebSocket<S>`。

### 4.2 桥接分层与回调

- `cosmos-network/src/bridge.rs` 只保留叶子转换、迭代器的 Rust LuaCallable 适配与类型注册，不含 FFI。
  `Headers` 组合 `StringPairs`，`LuaBytes` 对接 `Bytes` 与 Lua 字符串，TLS 配置组合 fallible userdata 借用。
- cosmos 基层原语包括：`Nil`、`Vec<u8>`、`StringPairs`、`LuaClosure`、元组和标量的转换。
  `()` 是 0 个输出值；`Nil` 是 1 个 nil；读取无返回值的回调可选择 `R = ()` 忽略结果。
- `LuaFunction::call<A: PushLua, R: FromLua>` 在内部创建并锚定协程，完成后读取拥有型结果；
  HTTP 和 WebSocket 都使用该入口。功能模块不再接触 Lua 指针、栈索引或类型编号。
- `#[derive(FromLua)]` 的结构体读取 table；枚举按分支上的 `#[lua(type = "table")]` 等选择，
  分支字段依次读取多个返回值。`#[lua(other)]` 表示无字段的兜底分支，不进行试读或额外装箱。
- HTTP `HandlerReply` 通过 derive 表达 table、二进制字符串、nil/错误串和无返回值；
  TCP `ReceiveArg` 复用同一宏表达数字或模式字符串。
- `#[derive(PushLua)]` 的结构体生成 table，枚举按分支字段顺序压栈；`ReceiveOutcome` 和
  `WaitOutcome` 直接组合 `Nil`、字符串、字节及下标，不手写多值栈操作。
- 已删除 `ReturnKind`、`return_kind`、`read_handler_return` 和对外的协程状态 getter。

### 4.3 VM 生命周期修复（重要）

WebSocket 用例暴露了退出段错误：脚本结束后 LocalSet 里仍挂着 ws handler 协程（持有 Lua userdata 的
`RefMut` 守卫），而 `lua_close` 先于任务回收执行 → 守卫 Drop 时读写已释放内存。

关闭顺序现由 `cosmos/src/machine.rs` 的 Drop 统一管理：

```rust
drop(self.local.take());       // 先回收本地任务（VM 仍存活）
userdata::cleanup(self.lua_state.as_ptr());
lua_close(self.lua_state.as_ptr());
```

执行结束或报错不会关闭 VM，全局状态和后台任务保留到后续执行；Machine 销毁时才关闭。
RegistryRef 为每次 VM 生命周期保留独立存活标记，供 LuaValue、LuaFunction、字符串锚点与轻线程执行状态共用，
关闭一个 VM 不影响另一个 VM，即使主状态地址被复用也不会误操作新 VM。
LightThread 的执行守卫先释放挂起 Future 中的借用，再解除协程锚定并发布完成结果，统一使用所属主状态 unref。

### 4.4 Machine 嵌入与插件

```rust
use cosmos::Machine;
use cosmos_network::Network;

let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(2).enable_all().build().unwrap();
let mut machine = Machine::new(runtime.handle().clone()).unwrap();
machine.add_module(Network).unwrap();
runtime.block_on(machine.exec("cosmos.time.sleep(10)")).unwrap();
```

- 核心 Machine/Module 从 cosmos crate 引入，Network 从 cosmos-network 引入；根目录为虚拟 workspace。
  Machine 只持有 Handle，不创建或关闭 Runtime；宿主负责 Runtime 存活并驱动执行。
- `Module::register(&self, &mut Machine) -> Result<(), String>` 同步注册插件，进入注入的 Runtime 与本地任务上下文，
  不要求插件 Send/Sync。Network 注册全部网络类型并加载编译内嵌的 Lua 定义，替代旧裸指针注册与 LUA_SOURCES。
- `register::<T>()` 注册 userdata；`load(name, source)` 同步加载定义；`spawn(future)` 创建本地后台任务。
  load 使用受保护的临时协程；遇到异步等待会回收协程并返回错误，不在 TLS 通道遗留未取出的 Future。
- `exec(source)`、`exec_with_args(name, source, args)`、`exec_file(path, args)` 均为 async，必须由注入的 Runtime 驱动。
  Runtime 身份不匹配或缺少 Tokio 上下文时返回错误，不执行 Lua；不在 Machine 内嵌套 block_on。
- 多线程 Runtime 的普通 Rust 任务可以在工作线程运行；Lua 与 HTTP handler 通过 LocalSet 留在固定宿主线程，
  不能直接将 Machine 或其执行 Future 交给要求 Send 的 tokio::spawn。
- HTTP 连接池由各 client 实例持有，不同实例独立，避免不同宿主 Runtime 或 VM 复用隐藏的全局池。
- 完整 Rust 插件示例见 cosmos-network/examples/machine.rs，外部调用不需要 Lua 裸指针。

### 4.5 HTTP client 实例与连接池

```lua
local HttpClient = require("cosmos.network.http.client")
local client = HttpClient.new({ ctx = tls_ctx }) -- 缺省使用公有信任根
local response, err = client:request("GET", url, {
    timeout = 3000,
    headers = { ["x-name"] = "value" },
})
```

- 每次 new 创建独立 Hyper 连接池，同一 client 内复用 HTTP/1.1 keep-alive 与 HTTP/2 连接；
  即使多个 client 使用相同 TLS context，它们也不共享池。已删除线程局部 CLIENTS、client_for 及 Runtime/TLS 指针索引。
- ctx 仅在构造时读取，信任根、客户端证书与 ALPN 为配置快照；修改原 context 只影响后来构造的 client。
  请求级 opts.ctx 会明确返回 nil, err，提示移到 HttpClient.new，不会被静默忽略。
- headers/body/timeout/redirects/max_body 仍为请求选项；重定向与整体超时语义不变。
- request 采用拥有型 self，通过 Clone 取得共享池句柄，不跨 await 借用原 Lua userdata；
  多协程可并发使用同一实例，原 userdata 被 GC 后在途请求仍正常完成。
- 最后一个 client/请求句柄释放后，池及空闲连接随之回收；默认 TLS 配置仍可共享，但不保留连接池。
- 连接池配置为空闲超时 30 秒、每 host 最多 32 条空闲连接，并显式安装 TokioTimer 使空闲超时生效。

### 4.6 统一轻线程及内部职责

- 主脚本、同步定义、LuaFunction 回调和后台 Lua 子任务统一使用 LightThread；删除独立的 LuaCoroutine。
- 私有 Execution 持有 Lua 状态指针、挂起操作和 registry 锚点，每步最多恢复一次 Lua；
  它不实现 Future，不独立调度。Control 只管理取消、唯一完成结果与等待者，不持有 Lua 指针。
- run 构造 Future 时就取走 Execution，由执行守卫独占；多次 run 会返回错误，未首次 poll 就 drop 也会取消并释放锚点。
  不跨 lua_resume 或 await 借用共享 RefCell，每次恢复前检查取消，每轮最多 256 次恢复后让出调度。
- Machine 与类型化回调直接驱动 run，不额外创建 Tokio 任务；cosmos.thread.spawn 才提交后台 LocalSet 任务。
  返回值在锚点有效时经 FromLua 读取，再释放执行资源、保存结果并唤醒等待者。
- join 使用拥有型克隆句柄，支持同一任务的并发 join 与 kill；join/wait 不消耗结果，可重复观察成功、错误或取消。
  wait 未选中的任务仍可继续 wait，取消等待者或参数解析失败均不影响其他观察者。
- 不再用 JoinHandle + oneshot 重复保存 Lua 任务结果；Control 按需登记 waker，等待 Future 被取消时注销自己的通知。
  kill 请求取消并唤醒执行者，完成后幂等；不会抢占正在运行的同步 Lua 代码，也不会因句柄 GC 而取消后台任务。
- Machine/CLI 退出语义保持不变：主协程结束则 exec 返回，保留 Machine 可继续执行；Machine drop 先回收 LocalSet 再关闭 VM。
- 分配变化：统一共享状态增加一个 Rc 状态分配，直接执行不增加 Tokio 任务；后台任务去掉 oneshot，等待登记按需分配。
  异步操作仍只装箱 Future 本身，原生返回值继续共用 PushLua，不增加返回值闭包装箱。

### 4.7 Rust 可见性边界

- cosmos 公开宿主入口 Machine/Module、可克隆的 LightThread 句柄，以及 userdata 的受控桥接协议；
  builtin、light_thread、stack、ffi 实现模块均为私有，libs 仅测试构建存在。
- VM 创建/关闭/恢复、registry 操作与 userdata 初始化/清理/裸注册/底层 push 均限 crate 内；
  main_state、MethodTable::new、字符串锚点及拷贝阈值为私有。
- LuaStr / LuaBytes 是私有字段的公开类型，分别提供 FromLua 与 Deref<str> / Deref<[u8]>；外部不能匹配或构造内部拷贝/锚定分支，二进制收发必须使用后者。
- FromLua<'lua> 绑定 Value 的读取生命周期；同步方法可借用字符串及结构体字段，
  LuaFunction::call 与 LightThread::run 要求结果对所有读取生命周期均可转换，拒绝返回临时协程的裸借用。
  同步表字段的借用字符串由 Call 内部 ReadScope 按需 registry 锚定，重入删除字段及 GC 不会使其悬垂；
  普通标量、拥有型字段和直接字符串参数不增加该锚点分配。
- 宏与叶子桥接通过 Value/Output/Call、TableReader/TableWriter 及 LuaCallable/LuaClosure 组合；
  yield_with、PendingOp/PendingFuture、abs_index 和所有原始状态/索引/ABI 均限内部，不再公开导出。
- ffi 模块及全部函数/类型/常量均限 crate 内；luaL_loadstring、lua_pcall 仅测试编译，
  未使用的 luaL_newmetatable 声明已删除。外部不能绕过 Machine 直接管理 VM 生命周期。
- cosmos-network 仅公开 Network；协议实体和跨功能模块转换类型限 crate 内，
  仅本文件使用的选项/响应类型为私有，TCP/TLS 共用的状态、读写半和字段限 tcp 模块。
- cosmos-macros 仅保留 3 个过程宏入口公开；CLI 函数与 Args 均私有。四个 crate 都启用 unreachable_pub。
- 可见性调整不改变 Lua require 路径或方法语义；Rust 外部代码应使用上述公开入口，不依赖实现模块。

### 4.8 受控桥接与 FFI 封闭

- FromLua<'lua> 接收 Value<'lua>，PushLua 接收 Output；原始状态、索引和上下文构造仅在 cosmos 内部可见。
  同步字符串及表字段借用受读取生命周期约束；LuaFunction::call 和轻线程结果要求 for<'lua> FromLua<'lua>，不能泄漏临时协程借用。
- Value/TableReader 统一完成类型选择、单次表遍历、嵌套读取和栈恢复；Output/TableWriter 统一构造字段，
  字段必须输出一个值，构造中断或 Rust 展开时回收未完成表。StringPairs 与 derive 复用这些完整操作，不是逐个转发 C API。
- userdata 宏生成 Rust LuaCallable 标记类型，MethodTable::add 单态化内部 C trampoline；
  LuaClosure 捕获 userdata 状态，与方法调用共用 Call、panic 边界和异步挂起协议，不增加回调元数据分配。
- LuaCallable 是 unsafe trait：适配必须由 Call::returns 或 suspend 生成准确结果，守卫只存活于本次执行。
  同步借用可以直接回传；异步参数仍使用拥有值或 registry 锚点，挂起只装箱 Future 一次，不装箱返回值。
- network bridge 不再使用原始 Lua 类型、栈常量或 C 入口：Bytes 经 &[u8] PushLua 输出，无中间 Vec；
  TCP 迭代器仅组合 LuaClosure 和 Rust 状态读取，TLS 配置通过受控 userdata 借用构造快照。
- 私有 ffi 全部使用 crate 内可见性，且没有公开函数或类型重导出。公开模块仅 userdata，宿主入口保持 Machine/Module/LightThread。
- Rust 自定义叶子转换需迁移到 FromLua<'lua>/Value 和 PushLua/Output；不再依赖 cosmos::ffi 或裸状态参数。
  Lua 模块路径、返回值顺序及 Machine 的执行/退出语义不变。

## 5. 本轮完成：统一 `PushLua`

- 删除 `IntoOutput` trait、derive、全部 impl，以及 `AsyncOutput` 枚举。
- `PendingFuture = Pin<Box<dyn PendingOp>>`；`PendingOp` 对所有输出实现 `PushLua` 的 Future
  提供 blanket impl：Pending 时不动 Lua 栈，Ready 时立即 `value.push_lua(output)`，返回压栈个数。
- Future 本身仍只装箱一次；标量没有新增返回值分配，原先 userdata/table/元组/枚举的
  `Custom` 闭包装箱也已消除。业务值自身与 Lua 栈的内存分配不包含在这个结论里。
- `#[userdata]` 的异步 Rust 调用适配直接返回原生 Rust 值；同步和异步都经 `PushLua`。
- `#[derive(PushLua)]`：struct 仍生成 Lua table；enum 按分支字段顺序生成多返回值。
  TCP `ReceiveOutcome` 和线程 `WaitOutcome` 都采用该机制。
- `()` 仍返回 0 个值，`Nil` 返回 1 个 nil；异步 `Option::None` 从旧实现的 0 个值统一为
  1 个 nil，`select('#', f())` 能观察到这个变化。
- **旧交接结论已纠正**：统一桥接并不必然增加一次 Box；可以在具体 Future 的输出类型
  被擦除前组合完成轮询和压栈，无需 specialization，也无需返回值闭包。

### 分层错误边界与冗余清理

- `try_check` / `try_check_shared` 通过 metatable 比较和 `RefCell` 检查返回 `Result`；
  类型错误和借用冲突均不直接调用 `lua_error`。借用守卫受调用栈/协程锚点保护，不能作为拥有型回调结果。
- 同步/异步方法与 TCP until 共用内部 callable trampoline；builtin 的原始入口也共用内部错误边界；
  参数解析和守卫获取失败先释放 Rust 局部值，再压错误串，释放该串后才调用 `lua_error`。
- `FromLua` 及其 derive 在成功、失败时恢复入口栈高度；相对索引固定和栈恢复由 Value/TableReader 统一处理，不再由宏生成手工操作。
- userdata 注册恢复入口栈高度，`push` 只增加一个值；类型表与模块路径对应的表复用。
- 移除了无调用方的 `with_alpn_server`、`LuaFunction::registry_ref/main/push_to`、`UserData::type_name`，
  旧 throwing check、重复 raise、状态别名与示例注册包装。

### Clippy 与底层安全契约

- PushLua/FromLua 接收受控 Output/Value，仍保留借用、线程亲和与 VM 生命周期的 unsafe 转换契约。
  LuaCallable 为 unsafe trait，必须由 Call 产生准确结果数量或挂起操作；内部 PendingOp、yield、注册及错误边界不对外公开。
- derive 会生成匹配的 unsafe 实现；功能模块继续只描述值，裸指针操作留在运行时/桥接层。
- 验证命令：`cargo clippy --workspace --all-targets -- -D warnings`。
- 两项局部例外带有明确理由：Conn 内联 TLS 流避免额外 Box；回归测试故意保持 RefCell 借用跨 await。

### 内建模块初始化

- Rust 布局：`builtin/mod.rs` 负责启动，`builtin/time.rs` 负责时间函数，
  `builtin/light_thread.rs` 只适配轻线程的 Lua 接口，`builtin/sync/` 负责同步原语；
  统一执行类型位于 `light_thread/`，不再存在独立的 LuaCoroutine。
- Machine 初始化顺序：`luaL_newstate` → `userdata::init` → `luaL_openlibs` → `builtin::open`。
  宿主随后 add_module(Network) 并按需注册自定义类型；CLI 不注册示例类型。
  `builtin::open` 自行注册 LightThread，宿主不再单独调用线程注册函数。
- Lua 时间入口为 `cosmos.time.sleep/now_ms/now_us`，轻线程入口为 `cosmos.thread.spawn/wait`，类型表为 `cosmos.LightThread`。
- `libs.rs` 及其 `run_lua_source` 仅在 cfg(test) 下作为内部同步测试工具编译；正式源码加载走 Machine::load。

### 时间模块：cosmos.time

- `Time` 通过 `#[userdata(name = "time")]` 声明，builtin 只需 register；
  `require("cosmos.time")` 与 `cosmos.time` 指向同一张表。
- `sleep(ms)` 是普通 Rust async 方法，由宏生成参数读取、yield 和返回值桥接；成功返回零个值。
  参数必须是 0..=1e12 的有限毫秒数，接受小数；负数、非有限值、缺省/nil 和非 number 均抛 Lua 参数错误。
- `now_ms()` / `now_us()` 使用 SystemTime 返回当前 Unix 毫秒/微秒整数时间戳（自 1970-01-01 UTC 起），
  随真实系统时间调整；没有进程内起点或缓存。sleep 仍由 Tokio 的定时器驱动。
- `Duration: FromLua` 在 userdata 基层统一完成毫秒转换和校验，time.sleep 与 sync 的可选 Timeout 共用。
  time.rs 不再包含裸 Lua 栈操作、手写 C trampoline 或方法注册表。

### 模块表按声明路径自动组装

- `#[cosmos::userdata(name = "sync.channel")]` 经 register/push 注册时，自动创建并连接
  `cosmos` → `sync` → `channel`，各级都安装 require 工厂，父表字段与 require 返回值身份一致。
- 先注册子类型时，后续父类型注册会补全已有父表，保留子模块、Lua 增补字段和 package.loaded 已缓存的引用；
  父类型先注册时，后续子类型同样直接挂到原表。构造和实例方法仍由 #[userdata] 自动生成。
- builtin::open 复用 cosmos 根表，不重新创建覆盖；sync/mod.rs 只保留类型注册与共用参数转换，
  删除手写 Sync 类型。网络 TCP 父表也自动生成，已删除 lua/modules.lua 与对应内嵌源码条目。
- 同步类型自己的 new 方法放在各自 #[userdata] impl 中，调用为 `sync.channel.new(...)` 等。
  同步宏直接对返回值使用 PushLua 的类型推导，支持常规 Rust `fn new() -> Self` 写法。

### 协程同步：cosmos.sync

`local sync = require("cosmos.sync")` 与 `cosmos.sync` 是同一张表，初始化由 builtin 负责，不新增依赖。
所有等待方法的末参均为可选毫秒超时：缺省/nil 无限等待，0 只 poll 一次立即尝试；
负值、非有限值和超过 1e12 的超时报 Lua 参数错误。操作失败返回 `nil, "timeout"/"closed"`。

| 构造 | 方法与语义 |
|---|---|
| `sync.channel.new(capacity)` | 正整数容量的有界多生产者、多消费者队列；`send(value[, timeout])` 成功返回 true，`recv([timeout])` 成功返回一个原始 Lua 值；`len()` 返回缓冲消息数；`close()` 唤醒所有等待者，拒绝后续发送，允许读完缓冲区 |
| `sync.notify.new()` | `wait([timeout])` 成功返回 true；`notify_one()` 唤醒一个等待者，没有等待者时最多存一个信号；`notify_all()` 唤醒当前全部等待者，不为后续等待者存信号；`close()` 令当前及后续等待返回 closed |
| `sync.semaphore.new(permits)` | 正整数并发配额；`acquire([timeout])` 公平排队，返回持有一个配额的 permit；`available()` 查询当前可用数量；`close()` 唤醒等待者并拒绝后续获取 |
| `sync.mutex.new()` | 非重入互斥锁；`lock([timeout])` 返回锁守卫；`with_lock(fn[, timeout])` 在锁内执行可 yield 的回调，返回其第一个值，无返回值时为 nil；回调报错返回 nil, err |

- channel 通过基层 `LuaValue: FromLua + PushLua` 的 registry 锚点保活值，保持 table、函数、
  userdata 的原始身份，二进制字符串不做编码转换；消息可以为 nil（成功仅返回一个 nil），
  调用方通过第二个错误返回值判断关闭/超时。发送缺省实参会报错。
- semaphore permit 和 mutex 锁守卫共用 `cosmos.sync.permit` 类型，`permit:release()` 幂等；
  GC 也会归还配额。需要及时释放时应显式 release。
- `with_lock` 的超时只覆盖获取锁；Rust 守卫跨回调 Future 持有，正常完成、报错或任务取消均立即归还锁。
  手动 `lock` 模式的守卫由 Lua 持有，持锁协程被取消后需由显式释放或后续 GC 回收，不能依赖即时释放。
- `#[userdata]` 新增拥有型 `self`/`mut self` 接收者，经 `FromLua` 转换；可克隆 userdata 的
  blanket impl 短暂借用后 clone。sync 等待方法使用 Rc/Arc 共享句柄，等待期间不占用原 userdata 借用，
  且原 userdata 被 GC 后仍安全。既有 `&self`/`&mut self` 方法继续保留原借用语义。
- channel 检查队列前先 enable 通知等待者，避免多生产者/消费者漏唤醒；超时/取消不提交消息，
  已分派给被取消等待者的 notify_one 会由 Tokio 转交。

```lua
local sync = require("cosmos.sync")
local mutex = sync.mutex.new()
local value, err = mutex:with_lock(function()
    cosmos.time.sleep(1)
    return "完成"
end, 100)
assert(err == nil, err)
```

### 尚未排期

- ClickHouse 原生 TCP 接入：用户明确放到待做；候选 Rust 原生客户端需评估协议版本、列式类型、流式结果与取消/回池、压缩依赖，不在当前 JSON 工作中启动。
- HTTP/3：用户明确暂缓。
- 压测脚本：`examples/http_bench.lua` / `examples/http_bench_client.lua`。

## 6. 代码地图

| 文件 | 职责 |
|---|---|
| `cosmos/src/ffi.rs` | 全部限 crate 内的 LuaJIT C API 声明，模块不对外公开 |
| `cosmos/src/machine.rs` | Machine/Module、安全嵌入 API、Handle 注入、LocalSet 与 VM 生命周期 |
| `cosmos/src/userdata.rs` | 桥接核心：`UserData`/`PushLua`/`FromLua`、userdata 存放与 GC、类型化 `LuaFunction::call`、fallible userdata 借用、`Nil`/`StringPairs`/`LuaValue`、derive 单元测试 |
| `cosmos/src/light_thread/mod.rs` | 统一 LightThread、唯一执行权、直接/后台调度与执行守卫 |
| `cosmos/src/light_thread/execution.rs` | 私有 Lua 执行状态机、锚点、结果读取、PendingOp 与 TLS yield 协议；不实现 Future |
| `cosmos/src/light_thread/control.rs` | 私有取消请求、最终结果与等待者通知，不接触 Lua 或 Tokio |
| `cosmos/src/builtin/mod.rs` | time/thread 内建功能统一初始化与 LightThread 类型注册 |
| `cosmos/src/builtin/time.rs` | 声明 cosmos.time 模块，sleep、now_ms、now_us 通过宏生成桥接 |
| `cosmos/src/builtin/light_thread.rs` | Lua spawn/wait/join/kill 参数与结果桥接、类型注册，不定义第二个线程实体 |
| `cosmos/src/builtin/sync/` | channel、notify、semaphore、mutex 及统一超时转换；mod.rs 只注册类型，模块表由声明路径自动生成 |
| `cosmos/src/libs.rs` | cfg(test) 下的内部同步 Lua 测试工具，不进入正式构建 |
| `cosmos/src/stack.rs` | 私有栈辅助，不通过 userdata 公开重导出 |
| `cosmos-macros/src/lib.rs` | `#[userdata]`、`FromLua`/`PushLua` derive |
| `cosmos-network/src/bridge.rs` | 只组合受控上下文的叶子转换、until Rust 调用适配及类型注册 |
| `cosmos-network/src/deadline.rs` | 网络协议共用的私有截止时间与毫秒转换 |
| `cosmos-network/src/udp.rs` | UDP 报文、共享句柄、地址与超时/关闭语义 |
| `cosmos-network/src/tcp/mod.rs` | TCP 协议族模块声明：stream 与 tls |
| `cosmos-network/src/tcp/stream.rs` | TCP 状态机、`ReceivePattern`/`ReceiveOutcome`、`receiveuntil` |
| `cosmos-network/src/tcp/tls.rs` | rustls 客户端/服务端、`TlsContext`（证书/信任根/ALPN/mTLS） |
| `cosmos-network/src/tcp/transport.rs` | TCP/Unix 明文传输与 TLS 升级前的预读回放 |
| `cosmos-network/src/socket.rs` | 顺序 socket、监听、TLS 原地升级与失败/取消时关闭 |
| `cosmos-encoding/src/lib.rs` | Encoding 插件及 Hex、Base64、URI 编解码 |
| `cosmos-encoding/src/json.rs` | sonic-rs JSON 与共享精确数字 userdata |
| `cosmos-encoding/src/value.rs` | JSON/TOML/YAML 共用文档值、Lua 容器读写与资源预算 |
| `cosmos-encoding/src/toml.rs` | TOML 原生解析树转换与日期时间 userdata |
| `cosmos-encoding/src/yaml.rs` | YAML 原始标量检查、受限 Serde 转换与别名策略 |
| `cosmos-encoding/lua/json.lua` | JSON 共享元表、容器标记与 decode 参数适配 |
| `cosmos-encoding/lua/formats.lua` | TOML/YAML 容器标记、数字类型别名与 decode 参数适配 |
| `cosmos-encryption/src/lib.rs` | Encryption 插件及摘要、RSA、Ed25519 |
| `cosmos-database/src/lib.rs` | 按 mysql / redis feature 注册 Database 插件及依赖 |
| `cosmos-database/src/options.rs` | 两种数据库的 URL 百分号、主机、布尔值叶子转换 |
| `cosmos-database/src/mysql.rs` | MySQL URL 转为受控连接选项 |
| `cosmos-database/src/redis.rs` | Redis / Rediss URL 转为受控连接选项 |
| `cosmos-database/lua/mysql.lua` | MySQL 文本协议、认证及按 client 隔离的空闲连接池 |
| `cosmos-database/lua/redis.lua` | RESP2、命令生成、pipeline、事务、Pub/Sub 和 client 空闲池 |
| `cosmos-database/tests/protocol.rs` | 认证、异常包和 16 MiB 分帧离线回归 |
| `cosmos-database/tests/redis.rs` | Redis 协议/限额/TLS/Unix 与显式真实服务器回归 |
| `cosmos-database/tests/features.rs` | 分数据库的模块与依赖注册隔离 |
| `cosmos-network/src/http/mod.rs` | `Conn`（明文/TLS 统一流）、`Headers`、`LuaBytes` |
| `cosmos-network/src/http/client.rs` | hyper legacy client、连接池、重定向、整体超时 |
| `cosmos-network/src/http/server.rs` | hyper auto server、请求 DTO、handler 协程、WebSocket 升级入口 |
| `cosmos-network/src/websocket/mod.rs` | ws/wss 客户端入口与连接选项 |
| `cosmos-network/src/websocket/conn.rs` | `WsConn`、帧收发、关闭与服务端升级后的回调执行 |
| `cosmos-cli/src/main.rs` | clap 参数解析、脚本/标准输入、宿主 Runtime 与 Machine 驱动 |
| `cosmos/src/userdata/context.rs` | 受控 Value/Output、表遍历/构造、栈恢复与按需字符串借用锚点 |
| `cosmos/src/userdata/call.rs` | Rust LuaCallable 与捕获状态闭包共用的内部 C ABI/挂起边界 |
| `cosmos/tests/bridge.rs` | 外部插件、derive、异步参数、闭包及错误恢复回归 |
| `cosmos-network/tests/bridge.rs` | TCP 迭代器的二进制、超时与取消后借用释放回归 |
| `cosmos-cli/tests/cli.rs` | 命令行帮助、版本、参数透传、标准输入与退出码回归 |
| `cosmos-network/tests/machine.rs` | 嵌入、插件、线程亲和、VM/Runtime 隔离、错误、关闭顺序与 client 池释放回归 |

## 7. 验证清单

CLI 用法：`cosmos-cli <SCRIPT> [ARG]...`，`-` 从标准输入读取；`--help` / `--version` 须放在脚本路径前。
脚本后的参数原样传给 Lua chunk 的 `...`。参数或文件读取错误退出 2，Lua 编译/执行错误退出 1。

```sh
cargo build

# 单进程
./target/debug/cosmos-cli examples/websocket.lua   # 期望最后一行 all websocket checks passed
./target/debug/cosmos-cli examples/http.lua        # 期望 http ok
./target/debug/cosmos-cli examples/spawn.lua       # OK
./target/debug/cosmos-cli examples/sleep.lua       # OK
./target/debug/cosmos-cli examples/sync.lua        # all sync checks passed
./target/debug/cosmos-cli examples/machine.lua host 参数
cargo run -p cosmos-network --example machine                    # 多线程 Runtime 下的 Rust 嵌入/插件示例

# 双进程
(./target/debug/cosmos-cli examples/tcp_server.lua 19001 &) ; sleep 1
./target/debug/cosmos-cli examples/tcp_client.lua 19001     # OK
(./target/debug/cosmos-cli examples/tls_server.lua 19443 &) ; sleep 1
./target/debug/cosmos-cli examples/tls_client.lua 19443     # OK

cargo test --workspace  # 包含离线 MySQL 回归；真实数据库用例须显式设置环境变量并 --ignored
```

## 8. 坑与注意事项

- **VM 关闭顺序**：先 `drop(local)` 再 `lua_close`（§4.3）。改了会 UAF。
- **async 方法的 self 守卫**：`#[userdata]` 生成的 async 方法会把 `RefMut/Ref` move 进 future，
  跨 await 一直占着借用；同一 userdata 的另一次调用会 `lua_error`。需要并发等待的共享句柄使用拥有型 self，
  经 FromLua clone 后跨 await 持有独立句柄，不持有原实例的借用。
- **FromLua 只返回 Err，不抛 Lua 错误**；借用守卫必须先释放，再由外层 trampoline 调用 lua_error。
- **嵌套转换不能保存字段视图**：Value/TableReader 内部固定索引，visit 的字段视图只用于当前回调；宏和模块不再自行计算栈位置。
- **hyper upgrade**：必须 `serve_connection_with_upgrades` 且保留 `http::request::Parts`
  （`OnUpgrade` 在 extensions 里），否则 101 之后连接不会被接管。
- **h2 不能走 WebSocket upgrade**（无 RFC 8441），当前直接 400。
- **wss 客户端必须强制 ALPN http/1.1**（`with_alpn_client`），否则服务端可能协商 h2 导致握手失败。
- **`()` 是 0 个返回值，`Nil` 是 1 个 nil**；多返回值占位必须用 `Nil`。
- **`server:close()` 只停 accept**；句柄被 GC 不等于 close（与 C++ 一致）。
- 用户不接受"给函数加马甲"式的分层修补；桥接要么用原语组合，要么别做。
