# HANDOFF — cosmos-rs（交给 Codex）

> 写给接手的 agent：读完这份再动手。约定优先于个人偏好；用户是中文母语、C++ 背景、对分层和分配很敏感。

## 0. TL;DR

- 项目：LuaJIT + Rust 重写版网络框架，路径 `/Users/ellutionist/code/ellutionist/cosmos-rs`。
- WebSocket、桥接分层重构和 VM 生命周期修复已提交为 `e004c8e`。
- 已按用户要求将统一返回值桥接、分层修复和冗余清理纳入本次本地提交；最新验证与踩坑见 `PROGRESS.md`。
- 异步边界采用 `PendingOp::poll_push`，只保留 `PushLua` 转换协议；不采用返回值闭包装箱方案（见 §5）。
- HTTP/WebSocket 回调已改用 `LuaFunction::call<A, R>`；userdata 借用失败只返回 Err，最外层 trampoline 统一抛 Lua 错误。
- 交流、注释、文档一律**中文**；**不要自动 commit / push**。

## 1. 环境与结构

```
cosmos-rs/
  cosmos/            # 运行时 + 桥接层：ffi / userdata / coroutine / stack / libs / builtin
  cosmos-macros/     # #[userdata]、#[derive(FromLua/PushLua)]，生成全部 trampoline
  cosmos-network/    # 功能模块：tcp/{stream,tls} / http / websocket + bridge.rs（叶子转换）
  src/main.rs        # 宿主：建 VM、跑脚本、关 VM；demo.rs 是 userdata 示例
  examples/*.lua     # 全部端到端用例
  cosmos-network/lua/ # 内嵌 Lua 源码（include_str! 随二进制编译）
```

- LuaJIT 2.1（Lua 5.1 C API），单线程：tokio `current_thread` + `LocalSet`，Lua 协程 `!Send`。
- 依赖：hyper 1 + hyper-util（client/server，h2 走 ALPN）、rustls+ring、fastwebsockets（upgrade feature）。
- 重构前基线：`e004c8e feat: 新增 WebSocket 支持并重构 Lua 桥接分层`；最新提交号通过 `git log -1` 查询。

## 2. 硬性约定（详见 `AGENTS.md`）

1. **功能模块零 FFI**：tcp / tls / http / websocket 里不出现 `cosmos::ffi`、不直接调 `lua_*`、不做裸栈操作。
2. **桥接 = 组合**：基层类型各实现 `PushLua` / `FromLua`，结构体靠 derive，枚举靠 derive，
   多返回值靠元组/枚举字段顺序。**禁止**把命令式栈操作换个文件存放（用户明确说过那是"马甲"，没有意义）。
3. 允许裸 FFI：cosmos 运行时/桥接层、`cosmos-network/src/bridge.rs` 的叶子转换与 C trampoline、`src/main.rs`（宿主）。
4. 不做死抽象；公共层要有明确复用点。
5. 任何行为改动必须实际跑 `examples/` 对应用例（含回归），不能只看编译通过。
6. 只有用户明确要求才 commit / push。

## 3. 工作区现状

- time/thread 内建功能已集中到 `cosmos/src/builtin`，由 `builtin::open` 统一初始化。
- time 已改为 `#[userdata(name = "time")]` 自动注册，入口统一为 `cosmos.time.*`，
  时间函数不再放在 cosmos 根表，examples 与类型化回调测试已同步迁移。
- `cosmos.sync` 已实现：有界 channel、通知、信号量和 mutex，见下文接口与回收语义。
- 模块注册已按 userdata 声明路径自动组装父表；手写 Sync 与 TCP 聚合表已删除，sync 构造改为各类型的 `.new(...)`。
- 上述 sync 与模块注册改动已提交为 `9a093b4`，未 push。网络文件已按模块层级整理为
  `tcp/mod.rs`、`tcp/stream.rs`、`tcp/tls.rs` 及 `websocket/mod.rs`、`websocket/conn.rs`，
  Rust 引用同步更新，Lua require 路径保持原样；目录整理已提交为 `6c0067a`。

- 本次命名整理提交：`Thread` → `LightThread`，`thread.rs` → `light_thread.rs`；当前 Rust 模块为 `cosmos::builtin::light_thread`，Lua 类型表为 `cosmos.LightThread`。
- Lua 状态指针（含宏生成代码和外部扩展）已从 `L` 改为 `lua_state`，未使用参数为 `_lua_state`。
- 其余 Clippy 诊断已修复，公开裸指针桥接 API 已明确 unsafe 契约；严格 Clippy、构建、15 项测试和 examples 通过。

- 已提交基线：WebSocket、桥接分层重构、VM 生命周期修复与项目约定。
- 本次提交范围：统一 `PushLua`、类型化 Lua 回调、枚举 `FromLua`、userdata fallible 借用、统一错误边界、冗余清理与回归记录。
- 主程序还依赖工作区外的 `../cosmos-ext-demo`；其 `src/lib.rs` 已经用户授权同步修改两行：
  移除旧输出类型的导入，`slow_add` 的 Future 直接返回 `a + b`。后续保存/迁移工程时须一起保留。

## 4. 最近完成的工作

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

- `cosmos-network/src/bridge.rs` 只保留叶子转换、`until` trampoline 与类型注册。
  `Headers` 组合 `LuaStrMap`，`LuaBytes` 对接 `Bytes` 与 Lua 字符串，TLS 配置组合 fallible userdata 借用。
- cosmos 基层原语包括：`Nil`、`Vec<u8>`、`LuaStrMap`、`push_closure`、元组和标量的转换。
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

`src/main.rs` 现在：

```rust
let (lua_state, result) = local.block_on(&rt, run(&code, &script_args));
drop(local);                 // 先回收本地任务（VM 仍存活）
userdata::cleanup(lua_state);
lua_close(lua_state);
```

`run()` 返回 `(*mut lua_State, Result<(), String>)`，内部不再 `lua_close`。所有错误路径也把 lua_state 交回 main。
**不要改回"run 内部关 VM"**。

## 5. 本轮完成：统一 `PushLua`

- 删除 `IntoOutput` trait、derive、全部 impl，以及 `AsyncOutput` 枚举。
- `PendingFuture = Pin<Box<dyn PendingOp>>`；`PendingOp` 对所有输出实现 `PushLua` 的 Future
  提供 blanket impl：Pending 时不动 Lua 栈，Ready 时立即 `value.push_lua(lua_state)`，返回压栈个数。
- Future 本身仍只装箱一次；标量没有新增返回值分配，原先 userdata/table/元组/枚举的
  `Custom` 闭包装箱也已消除。业务值自身与 Lua 栈的内存分配不包含在这个结论里。
- `#[userdata]` 的 async trampoline 直接返回原生 Rust 值；同步和异步都经 `PushLua`。
- `#[derive(PushLua)]`：struct 仍生成 Lua table；enum 按分支字段顺序生成多返回值。
  TCP `ReceiveOutcome` 和线程 `WaitOutcome` 都采用该机制。
- `()` 仍返回 0 个值，`Nil` 返回 1 个 nil；异步 `Option::None` 从旧实现的 0 个值统一为
  1 个 nil，`select('#', f())` 能观察到这个变化。
- **旧交接结论已纠正**：统一桥接并不必然增加一次 Box；可以在具体 Future 的输出类型
  被擦除前组合完成轮询和压栈，无需 specialization，也无需返回值闭包。

### 分层错误边界与冗余清理

- `try_check` / `try_check_shared` 通过 metatable 比较和 `RefCell` 检查返回 `Result`；
  类型错误和借用冲突均不直接调用 `lua_error`。借用守卫受调用栈/协程锚点保护，不能作为拥有型回调结果。
- 同步/异步宏、TCP until、sleep、thread.spawn/wait 共用 `userdata::trampoline`；
  参数解析和守卫获取失败先释放 Rust 局部值，再压错误串，释放该串后才调用 `lua_error`。
- `FromLua` 及其 derive 在成功、失败时恢复入口栈高度；相对索引先通过 `stack::abs_index` 固定。
- userdata 注册恢复入口栈高度，`push` 只增加一个值；类型表与模块路径对应的表复用。
- 移除了无调用方的 `with_alpn_server`、`LuaFunction::registry_ref/main/push_to`、`UserData::type_name`，
  旧 throwing check、重复 raise、状态别名与示例注册包装；Counter 注册直接放在宿主入口。

### Clippy 与底层安全契约

- `PushLua::push_lua`、`PendingOp::poll_push`、`yield_with`、userdata 的注册/压栈/trampoline/cleanup
  均为 unsafe 接口；调用方必须保证状态有效、线程亲和、栈空间与 VM 生命周期。具体约束见各接口的 `# Safety`。
- derive 会生成匹配的 unsafe 实现；功能模块继续只描述值，裸指针操作留在运行时/桥接层。
- 验证命令：`cargo clippy --workspace --all-targets -- -D warnings`。
- 两项局部例外带有明确理由：Conn 内联 TLS 流避免额外 Box；回归测试故意保持 RefCell 借用跨 await。

### 内建模块初始化

- Rust 布局：`builtin/mod.rs` 负责启动，`builtin/time.rs` 负责时间函数，
  `builtin/light_thread.rs` 负责轻线程，`builtin/sync/` 负责同步原语；底层 `LuaCoroutine` 留在 `coroutine.rs`。
- 宿主顺序：`luaL_openlibs` → `userdata::init` → `builtin::open` → 注册外部扩展和网络类型。
  `builtin::open` 自行注册 LightThread，宿主不再单独调用线程注册函数。
- Lua 时间入口为 `cosmos.time.sleep/now_ms/now_us`，轻线程入口为 `cosmos.thread.spawn/wait`，类型表为 `cosmos.LightThread`。
- `libs.rs` 仅保留 `run_lua_source`，不再承担内建模块实现或注册。

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

- UDP / unix socket：优先级待用户确认，勿主动开工。
- HTTP/3：用户明确暂缓。
- 压测脚本：`examples/http_bench.lua` / `examples/http_bench_client.lua`。

## 6. 代码地图

| 文件 | 职责 |
|---|---|
| `cosmos/src/ffi.rs` | LuaJIT C API 声明（唯一 FFI 声明处） |
| `cosmos/src/userdata.rs` | 桥接核心：`UserData`/`PushLua`/`FromLua`、userdata 存放与 GC、类型化 `LuaFunction::call`、fallible userdata 借用、`Nil`/`LuaStrMap`/`LuaValue`、derive 单元测试 |
| `cosmos/src/coroutine.rs` | `LuaCoroutine`（Future）、`PendingOp`/`PendingFuture`、TLS yield 通道、`yield_with` |
| `cosmos/src/builtin/mod.rs` | time/thread 内建功能统一初始化与 LightThread 类型注册 |
| `cosmos/src/builtin/time.rs` | 声明 cosmos.time 模块，sleep、now_ms、now_us 通过宏生成桥接 |
| `cosmos/src/builtin/light_thread.rs` | `LightThread` 句柄、spawn/wait/join/kill |
| `cosmos/src/builtin/sync/` | channel、notify、semaphore、mutex 及统一超时转换；mod.rs 只注册类型，模块表由声明路径自动生成 |
| `cosmos/src/libs.rs` | 内嵌 Lua 源码加载工具 |
| `cosmos/src/stack.rs` | 栈辅助：`err_message`、`abs_index` |
| `cosmos-macros/src/lib.rs` | `#[userdata]`、`FromLua`/`PushLua` derive |
| `cosmos-network/src/bridge.rs` | 本 crate 的叶子转换、`until` trampoline、注册入口 |
| `cosmos-network/src/tcp/mod.rs` | TCP 协议族模块声明：stream 与 tls |
| `cosmos-network/src/tcp/stream.rs` | TCP 状态机、`ReceivePattern`/`ReceiveOutcome`、`receiveuntil` |
| `cosmos-network/src/tcp/tls.rs` | rustls 客户端/服务端、`TlsContext`（证书/信任根/ALPN/mTLS） |
| `cosmos-network/src/http/mod.rs` | `Conn`（明文/TLS 统一流）、`Headers`、`LuaBytes` |
| `cosmos-network/src/http/client.rs` | hyper legacy client、连接池、重定向、整体超时 |
| `cosmos-network/src/http/server.rs` | hyper auto server、请求 DTO、handler 协程、WebSocket 升级入口 |
| `cosmos-network/src/websocket/mod.rs` | ws/wss 客户端入口与连接选项 |
| `cosmos-network/src/websocket/conn.rs` | `WsConn`、帧收发、关闭与服务端升级后的回调执行 |
| `src/main.rs` | 宿主：VM 生命周期、脚本参数、LocalSet 回收顺序 |

## 7. 验证清单

```sh
cargo build

# 单进程
./target/debug/cosmos-rs examples/websocket.lua   # 期望最后一行 all websocket checks passed
./target/debug/cosmos-rs examples/http.lua        # 期望 http ok（32 项）
./target/debug/cosmos-rs examples/userdata.lua    # OK
./target/debug/cosmos-rs examples/spawn.lua       # OK
./target/debug/cosmos-rs examples/ext.lua         # OK
./target/debug/cosmos-rs examples/sleep.lua       # OK
./target/debug/cosmos-rs examples/sync.lua        # all sync checks passed
# examples/error.lua 期望 exit=1（故意调用 sleep(-1)）

# 双进程
(./target/debug/cosmos-rs examples/tcp_server.lua 19001 &) ; sleep 1
./target/debug/cosmos-rs examples/tcp_client.lua 19001     # OK
(./target/debug/cosmos-rs examples/tls_server.lua 19443 &) ; sleep 1
./target/debug/cosmos-rs examples/tls_client.lua 19443     # OK

cargo test --workspace  # 18 passed（derive、回调、栈恢复、借用/取消、LuaValue、拥有型接收者、模块注册顺序与身份）
```

## 8. 坑与注意事项

- **VM 关闭顺序**：先 `drop(local)` 再 `lua_close`（§4.3）。改了会 UAF。
- **async 方法的 self 守卫**：`#[userdata]` 生成的 async 方法会把 `RefMut/Ref` move 进 future，
  跨 await 一直占着借用；同一 userdata 的另一次调用会 `lua_error`。需要并发等待的共享句柄使用拥有型 self，
  经 FromLua clone 后跨 await 持有独立句柄，不持有原实例的借用。
- **FromLua 只返回 Err，不抛 Lua 错误**；借用守卫必须先释放，再由外层 trampoline 调用 lua_error。
- **FromLua 嵌套取值要用绝对索引**：`lua_next` 遍历后再用 `-1` 会错位（`value_idx = lua_gettop(lua_state)`）。
- **hyper upgrade**：必须 `serve_connection_with_upgrades` 且保留 `http::request::Parts`
  （`OnUpgrade` 在 extensions 里），否则 101 之后连接不会被接管。
- **h2 不能走 WebSocket upgrade**（无 RFC 8441），当前直接 400。
- **wss 客户端必须强制 ALPN http/1.1**（`with_alpn_client`），否则服务端可能协商 h2 导致握手失败。
- **`()` 是 0 个返回值，`Nil` 是 1 个 nil**；多返回值占位必须用 `Nil`。
- **`server:close()` 只停 accept**；句柄被 GC 不等于 close（与 C++ 一致）。
- 用户不接受"给函数加马甲"式的分层修补；桥接要么用原语组合，要么别做。
