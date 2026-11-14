# 进度记录

## 2026-11-13 WebSocket 模块 + 桥接分层重构

### 核心逻辑

- **服务端 WebSocket 复用 HTTP 端口**：HTTP handler 对选定 path 返回
  `{ websocket = function(conn, req) ... end }`；`http/server.rs` 用保留的
  `http::request::Parts`（含 `OnUpgrade`）调 `fastwebsockets::upgrade::upgrade` 回 101，
  连接驱动改为 `serve_connection_with_upgrades`；升级后的帧循环在独立本地任务里跑
  Lua 协程（`websocket::run_conn`）。
- **客户端**：`Ws.connect(url, opts)` 支持 `ws://` / `wss://`（TlsContext，强制 ALPN
  `http/1.1`）；`WsConn` 提供 send/send_binary/ping/receive/close/state/settimeout；
  `receive` 返回 `("text"|"binary", data)`，关闭 `nil,"closed"`，超时 `nil,"timeout"`。
- **桥接分层**：功能模块零 FFI；`cosmos-network/src/bridge.rs` 只放叶子转换、
  `until` 的 C trampoline 与注册。cosmos 新增基层原语：`Nil`、`Vec<u8>` 的
  PushLua/FromLua、`LuaStrMap`、`push_closure`、`LuaFunction::call_in_new_thread`、
  `stack::ReturnKind`/`return_kind`、元组 `IntoOutput`、`&str` 的 `PushLua`。
- **枚举返回值组合化**：新增 `#[derive(cosmos::IntoOutput)]`，`ReceiveOutcome`
  改为按分支字段顺序压栈，删除手写 `match + lua_push*`。
- **VM 生命周期**：`main.rs` 先 `drop(local)` 回收 LocalSet 挂起任务，再
  `cleanup + lua_close`；`run()` 返回 `(L, Result)`，不再内部关 VM。

### 踩坑

- **lua_close 与 LocalSet 顺序**：脚本结束后仍挂着的 ws handler 协程持有 Lua userdata
  的 `RefMut` 守卫，先关 VM 会在守卫 Drop 时读写已释放内存（SIGSEGV）；
  必须先把本地任务回收掉。
- **hyper upgrade**：必须 `serve_connection_with_upgrades` 且保留 `Parts`；
  HTTP/2 没有 Upgrade 机制，命中 ws path 直接 400。
- **fastwebsockets 类型**：`FragmentCollector<S>` 包的是底层流 `S`，不是 `WebSocket<S>`。
- **wss**：客户端必须强制 ALPN `http/1.1`，否则可能协商到 h2 导致握手失败。
- **多返回值占位**：`()` 是 0 个返回值，`Nil` 才是 1 个 nil；`nil, err, partial`
  这类形态要靠字段顺序表达。
- **FromLua 嵌套取值**：遍历 table 后必须用绝对索引（`lua_gettop`），不能继续用 `-1`。

### 下一步

- 统一返回值桥接已完成，最终方案与验证见下节。
- UDP / unix socket：优先级待用户确认。
- HTTP/3：用户明确暂缓。


## 2026-11-13 统一同步与异步返回值桥接

### 核心逻辑

- 删除 `AsyncOutput`、`IntoOutput` trait、derive 及全部实现，同步与异步返回值统一使用 `PushLua`。
- `PendingFuture` 改为 `Pin<Box<dyn PendingOp>>`。所有输出实现 `PushLua` 的 Future 自动实现
  `PendingOp`：Pending 时不动栈；Ready 时在当次轮询中压入原生返回值，给调度器返回压栈个数。
- 只保留原有的一次 Future 装箱；标量没有增加返回值分配，原先复合返回值的 `Custom` 闭包装箱已移除。
  业务值自身、网络缓冲及 Lua 对象的分配不在这个结论之内。
- `#[userdata]` 的 async trampoline 直接返回原生 Rust 值；`#[derive(PushLua)]` 支持
  struct → table、enum → 按字段声明顺序压栈。TCP `ReceiveOutcome` 与线程 `WaitOutcome` 均已迁移。
- `thread.wait` 的成功/失败返回值通过枚举组合，不再手写结果闭包与多值栈操作。
- 主程序的工作区外路径依赖 `../cosmos-ext-demo/src/lib.rs` 已同步迁移两行：移除旧类型导入，
  `slow_add` 的 Future 直接返回整数。该文件不属于当前 Git 仓库，保存工程时须一起保留。
- 已同步更新 `AGENTS.md` 与 `HANDOFF.md`；保留用户新增的进度记录约定。本轮未 commit / push。

### 踩坑与语义

- 旧交接中的“统一桥接必须新增一次返回值 Box”不成立：把轮询和压栈组合后再擦除 Future 的类型，
  就能直接消费具体返回值，无需 specialization 或额外闭包。
- `()` 仍返回 0 个值，`Nil` 返回 1 个 nil；异步 `Option::None` 从旧实现的 0 个值统一为
  1 个 nil，可以通过 `select('#', f())` 区分。
- 枚举命名字段生成独立临时绑定，避免名为 `L` 的字段遮蔽 Lua 状态参数；回归同时覆盖泛型、
  无字段/元组/命名字段分支及包含 NUL、0xFF 的二进制数据。
- 保留单线程与 `!Send` 模型；回归验证挂起期间借用受保护、完成后释放借用、取消时释放捕获值且不压栈。
- 自动化验证中，Lua `print` 写入管道会缓冲；若等待“listening on”作为启动信号，会等到服务端
  3 秒 accept 超时退出才读到输出，随后客户端报连接拒绝。修正测试启动等待后，原始双进程用例通过，
  无需修改网络实现或用例超时。

### 验证

- `cargo build`：通过。
- `cargo test --workspace`：11 项单元测试通过（原有 7 项，新增 4 项）。
- 单进程 `examples/websocket.lua`、`http.lua`、`userdata.lua`、`spawn.lua`、`ext.lua`、`sleep.lua`：通过。
- `examples/error.lua`：按预期退出码为 1。
- `COSMOS_ECHO_PORT=19002 ./target/debug/cosmos-rs examples/tcp.lua 19002`：通过，覆盖读取模式、
  超时、partial、receiveuntil 与缓冲上限。
- `COSMOS_TLS_ECHO_PORT=19444 ./target/debug/cosmos-rs examples/tls.lua 19444`：通过。
- TCP 双进程 `tcp_server.lua` / `tcp_client.lua`（19001）：双方通过。
- TLS 双进程 `tls_server.lua` / `tls_client.lua`（19443）：双方通过，覆盖 mTLS 与 reload。
- Rust 源码（含外部扩展）已无旧返回值类型/trait 的引用；功能模块没有新增裸 Lua 栈操作。

### 下一步

- 本轮迁移已完成；后续功能由用户安排。
- UDP / unix socket 优先级待确认；HTTP/3 继续暂缓。


## 2026-11-13 修复分层边界与错误后的借用泄漏

### 核心逻辑

- **类型化回调**：新增 `LuaFunction::call<A: PushLua, R: FromLua>`，在运行时内部创建协程、
  等待并读取拥有型返回值，整个过程由 registry 锚点保活。HTTP 与 WebSocket 均已迁移，
  功能模块不再接触 Lua 状态指针、栈索引和类型编号。
- **枚举读取组合**：`#[derive(FromLua)]` 支持分支 `#[lua(type = "table")]` 等类型选择，
  按字段声明顺序读取多返回值；`#[lua(other)]` 是无字段兜底。按类型直接分流，不试读分支、
  不为返回值装箱。HTTP `HandlerReply` 与 TCP `ReceiveArg` 是两个实际使用方。
- **userdata fallible 原语**：`try_check` / `try_check_shared` 比较 metatable 后检查 RefCell，
  类型或借用失败都返回 `Err`。TLS `ClientCfg` / `ServerCfg` 的 FromLua 只组合这些原语与配置快照。
- **统一错误出口**：同步与异步宏、until、sleep、thread.spawn/wait 统一经 `userdata::trampoline`。
  参数读取、借用获取及错误传播都先走 Rust 的正常返回，局部资源释放后再抛 Lua 错误；
  压入 Lua 的错误字符串也在 `lua_error` 前显式释放。
- **栈恢复**：结构体/枚举 FromLua 在失败和成功时都恢复入口栈高度；嵌套与负索引用
  `stack::abs_index` 固定。修复首次 metatable 构建少弹出一项，保持 push 的单值契约。
- **清理冗余**：删除 `ReturnKind` / `return_kind` / `read_handler_return`、`LuaCoroutine::state`、
  `LuaFunction::registry_ref/main/push_to/call_in_new_thread`、`with_alpn_server`、`UserData::type_name`、
  旧 throwing check、重复 raise、状态类型重导出和示例注册包装；Counter 注册移到宿主。
- 已同步更新 `HANDOFF.md`；保留此前统一 PushLua 的未提交改动，本轮未 commit / push。

### 踩坑与兼容

- 原故障已复现并修复：`pcall(listener:reload({}))` 后 listener 永久处于独占借用状态；
  `pcall(acc:tick("bad"))` 后 acc 无法 get。根因都是 lua_error 的 longjmp 跳过 Rust 析构。
- 借用守卫没有实现拥有型 `FromLua`：它们只能在调用栈或协程锚点保护期间使用，
  否则类型化回调返回守卫后释放协程锚点，会失去对 Lua userdata 的保活。
- 保持 HTTP 的既有返回语义：table、二进制字符串、nil、无返回值、nil+错误串；
  nil 后第二个值只有是字符串才表示错误，其他值仍忽略，由嵌套枚举兜底表达。
- userdata 单元测试需打开标准库并 init VM，类型注册会访问 package.preload。
  新增栈高度断言暴露了首次注册遗留 metatable 的旧问题，已修复而非放宽断言。
- VM 关闭顺序、LocalSet 单线程模型及每次异步操作一次 Future 装箱保持不变。

### 验证

- `cargo build`：通过；`cargo test --workspace`：15 项单元测试通过。
- 新增单元覆盖：枚举按类型读取及多返回值、负索引与失败后的栈恢复、userdata 类型/借用错误、
  类型化回调跨 yield 的参数和结果保活。
- 原始临时复现脚本：非法 reload 后 close 成功；非法 tick 后 get 成功。
- `examples/ext.lua` 新增独占/共享异步参数错误后的读写与合法异步调用恢复检查：通过。
- `examples/tls_server.lua` 新增非法 ctx 后合法 reload 检查，后续 mTLS/reload 双进程用例：通过。
- `examples/http.lua` 新增回调字符串、空返回、nil、nil+非字符串、nil+错误串、非法类型/字段及
  错误后后续请求检查；其余 HTTP/HTTPS/h2 与 WebSocket 回归：通过。
- 其余 userdata、spawn、sleep、ext、TCP/TLS 单进程及双进程用例全部通过；error.lua 按预期退出 1。
- 分层扫描确认：网络功能模块没有 Lua 指针、裸栈操作或手写 FromLua 实现；旧包装与冗余接口已无引用。

### 下一步

- 本轮问题已修复，等待用户安排后续任务。
- UDP / unix socket 优先级仍待确认，HTTP/3 继续暂缓。


## 2026-11-13 本地提交

- 按用户要求，将统一 PushLua、三处分层问题修复、冗余清理、回归用例及文档一起提交到本地 Git。
- 提交前 `git diff --check` 通过；构建、15 项单元测试与 examples 回归沿用上一轮已完成的验证结果。
- 外部路径依赖 `../cosmos-ext-demo` 不属于本仓库，其同步迁移的两行改动不在本次 Git 提交内。
- 下一步等待用户安排；未执行 push。


## 2026-11-13 Thread 与文件重命名为 LightThread

### 核心逻辑

- 将句柄类型 `Thread` 改为 `LightThread`，文件 `thread.rs` 改为 `light_thread.rs`；
  Rust 路径为 `cosmos::light_thread::LightThread`，同步更新宿主注册与模块构造引用。
- 使用 `#[cosmos::userdata(name = "LightThread")]` 显式指定 Lua 类型表 `cosmos.LightThread`。
- 同步更新 AGENTS.md 的运行时模块列表和 HANDOFF.md 的代码地图。
- 本轮只做类型/文件重命名；`LuaCoroutine`、原有执行与句柄分层、`cosmos.thread.spawn/wait` 保持原样。

### 验证与下一步

- `cargo build`、`cargo test --workspace`（15 项）通过。
- 实际验证 `require("cosmos.LightThread")` 与 spawn 返回值的 metatable 一致，join 正常。
- 实际运行 `examples/spawn.lua`、`ext.lua`、`http.lua`、`websocket.lua` 全部通过。
- 未 commit / push，等待用户安排。


## 2026-11-13 Lua 状态指针统一使用 Rust 命名

### 核心逻辑

- 按用户最新要求，将运行时、桥接层、宿主、FFI 参数及外部扩展示例中的状态指针
  `L` 改为 `lua_state`，未使用的 `_L` 改为 `_lua_state`。
- userdata trampoline、FromLua/PushLua derive 同步生成 `lua_state`；内部锚定对象的同名字段、
  相关注释和交接文档一并更新。
- 撤销上一轮未完成方案新增的 non_snake_case 豁免；宏原有的两处 trampoline 函数名豁免保留，
  它们用于生成函数名中包含 Rust 类型名的场景，不用于屏蔽状态变量警告。
- 重命名区分 Rust 标识符与字符串/字符字面量，保留 TCP `"L"` / `"*L"` 等协议参数。
  枚举回归字段也改为 `lua_state`，继续验证与宏参数同名时的绑定隔离。
- 保留此前 LightThread 重命名与原有实现分层；没有改变执行流程或新增依赖。

### 验证与下一步

- `cargo build`、`cargo test --workspace`（15 项）通过。
- Clippy 完整工作区命名检查通过：检查命令仅临时使用
  `-A clippy::not_unsafe_ptr_arg_deref -D non_snake_case` 绕过原有裸指针 API 错误，让所有 crate
  完成诊断；non_snake_case 数量为 0。该参数没有写入项目配置。
- 标准 Clippy 原有的 `not_unsafe_ptr_arg_deref` 错误不是本轮命名修改的范围，保留原规则。
- 实际运行 websocket、http、userdata、spawn、ext、sleep、TCP/TLS 单进程与双进程回归；
  error.lua 按预期退出 1。
- 已更新 AGENTS.md 的状态指针命名约定与 HANDOFF.md，本轮未 commit / push。


## 2026-11-13 提交命名整理

- 按用户要求，先将 LightThread 类型/文件重命名、lua_state 变量命名及相关文档提交到本地 Git。
- 提交前差异检查通过；构建、15 项单元测试和 examples 回归沿用上一轮已完成的结果。
- 外部扩展示例的同步改动不属于本仓库；本次未 push。下一步检查并修复剩余 Clippy 诊断。


## 2026-11-13 修复剩余 Clippy 诊断

### 核心逻辑

- 已先按用户要求提交命名整理：`738dfeb`（LightThread 与 lua_state），未 push。
- 将操作公开 Lua 裸指针的 `PushLua::push_lua`、`PendingOp::poll_push`、`yield_with`、
  userdata 注册/压栈/trampoline/cleanup 标为 unsafe；derive 同步生成 unsafe 方法。
  调用方在原有运行时/桥接层履行安全契约，功能模块仍只构造普通值。
- 为公开 unsafe 接口补齐标准 `# Safety` 章节，明确 VM 存活、线程亲和、栈空间、索引/锚点及
  关闭顺序要求；外部扩展示例的注册入口同步补充说明。
- MethodTable 增加 Default；宏参数处理改用 enumerate；合并嵌套判断，移除重复导入、
  不必要的生命周期及同类型转换。receive(n) 的 n 已是 usize，移除无效的 max(0) 和转换。

### 有意保留的设计

- HTTP Conn 内联保存 TLS 流，避免每条连接新增 Box；仅在该枚举上说明并允许 large_enum_variant。
- 一项回归专门验证 userdata 借用跨 await 保活及完成后释放，因此仅在该测试上说明并允许
  await_holding_refcell_ref。生产 API 的错误与安全契约均实际修复，没有全局关闭 lint。
- 桥接 API 的 Rust 调用签名发生变化：手写 PushLua 实现需声明 unsafe fn，调用这些底层接口需
  在满足契约的 unsafe 边界内；Lua API、返回值、单线程模型与分配策略保持原样。

### 验证与下一步

- `cargo clippy --workspace --all-targets -- -D warnings` 通过，没有使用临时 -A 参数。
- `cargo build`、`cargo test --workspace`（15 项）通过。
- 实际运行 websocket、http、userdata、spawn、ext、sleep、TCP/TLS 单进程与双进程回归全部通过；
  error.lua 按预期退出 1。
- 已更新 HANDOFF.md。本轮 Clippy 修复尚未 commit / push，等待用户安排。


## 2026-11-13 将 time/thread 收拢为 builtin 模块

### 核心逻辑

- 新建 `cosmos/src/builtin/mod.rs` 与 `time.rs`，将 `light_thread.rs` 移入 builtin 目录。
  原有 LuaCoroutine 驱动和 LightThread 句柄实现保持分离，未引入新包装或依赖。
- 时间功能从 libs.rs 拆出，保留 sleep/now_ms/now_us 的实现和行为；libs.rs 仅保留 Lua 源码加载工具。
- `builtin::open` 统一创建 cosmos 表、安装时间函数与 thread 表、注册 LightThread 类型。
  宿主先执行 userdata::init，再调用 builtin::open，无需另行注册线程类型。
- LightThread 的 Rust 路径变为 `cosmos::builtin::light_thread::LightThread`；模块内部的 open/register_types
  收紧为 pub(super)，统一初始化入口对外公开。现有 Lua API 和 cosmos.LightThread 类型表保持原样。
- 更新宿主、类型化回调测试、AGENTS.md 的运行时模块列表以及 HANDOFF.md 代码地图。
- 保留上一轮未提交的 Clippy 修复，本轮未 commit / push。

### 验证与下一步

- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo build`、
  `cargo test --workspace`（15 项）通过。
- 实际验证时间函数、thread 入口以及 require("cosmos.LightThread") 对应 spawn 返回类型，join 正常。
- websocket/http/userdata/spawn/ext/sleep、TCP/TLS 单进程及双进程用例全部通过；
  error.lua 按预期退出 1。
- 当前没有旧 open_cosmos 或根模块 light_thread 的 Rust 引用；下一步等待用户安排。

## 2026-11-13 提交 builtin 整理与 Clippy 修复

- 按用户要求，将 builtin 目录整理、Clippy 修复及配套文档一起提交到本地 Git；不 push。
- 提交前差异检查通过，沿用上一轮严格 Clippy、构建、15 项测试及 examples 的完整验证结果。
- 外部目录 `../cosmos-ext-demo` 的配套修改不在本次仓库提交范围内。
- 后续 builtin 优先考虑与协程调度直接相关的同步原语；本轮只评估方向，不新增模块。

## 2026-11-13 新增 cosmos.sync 内建模块

### 核心逻辑

- 新建 `cosmos/src/builtin/sync/`，实现有界多生产者、多消费者 channel、通知、信号量和非重入 mutex。
  由 builtin 统一安装，`cosmos.sync` 与 `require("cosmos.sync")` 共用类型表，没有增加依赖。
- channel 使用 VecDeque 保存 FIFO 消息，满时等待可写通知；close 唤醒全部等待者，拒绝发送，保留缓冲消息供读取。
  notify_one 最多暂存一次信号，notify_all 仅广播给现有等待者，通知对象也支持 close。
- semaphore 使用 Tokio 公平信号量，mutex 复用单配额信号量；二者共用 permit userdata，release 幂等，GC 归还配额。
- mutex 增加 with_lock：获取锁后通过 LuaFunction::call 执行可 yield 的回调，Rust 守卫跨 Future 持有，
  完成、错误、取消均及时释放锁；返回回调的第一个值，无返回值为 nil。超时只覆盖获取锁。
  手动 lock 的守卫属于 Lua，需显式 release 或 GC；取消持锁协程不承诺即时释放该模式的锁。
- 所有等待接口统一可选毫秒超时；缺省/nil 无限等待，0 只 poll 一次；操作错误返回 nil, timeout/closed。
  容量与配额严格校验为正整数，拒绝截断小数；超时校验拒绝负值、非有限值和过大值。

### 桥接与踩坑

- 新增基层 LuaValue 的 FromLua/PushLua，以 registry 锚点保活任意 Lua 值，保留 table、函数和 userdata 身份，
  消费、发送失败或取消时释放锚点。nil 的 LUA_REFNIL 单独处理，避免误读 registry 的负下标。
- #[userdata] 支持拥有型 self 接收者；可克隆 userdata 的 FromLua 仅在转换时借用并 clone。
  sync 的 Rc/Arc 共享句柄可并发等待，且不依赖原 Lua userdata 在挂起期间存活；既有借用接收者语义不变。
- 新增 push_type 组合 userdata 类型表，register 复用该入口，模块不手写方法 trampoline 或返回值压栈。
- channel 必须在检查队列前 enable 通知 Future，防止多个发送者/接收者漏唤醒；用例覆盖已通知等待者被取消后的转交。
- Tokio 的零时长 timeout 仍可能让出一次调度机会，因此零超时改为只 poll 一次，保证即时尝试语义。

### 验证与下一步

- 新增 `examples/sync.lua`，215 项检查通过：并发等待、背压、关闭、超时、取消、Lua 值/守卫 GC、
  mutex 错误与取消释放、零超时及参数校验，并覆盖 VM 退出时仍有等待任务的回收。
- `cargo test --workspace` 共 17 项通过，新增 LuaValue 身份/nil/栈高度和拥有型接收者跨 yield 不占借用的回归。
- 构建、严格 Clippy 通过；websocket/http/userdata/spawn/ext/sleep、TCP/TLS 单进程及双进程回归通过，
  error.lua 按预期退出 1。更新 AGENTS.md 与 HANDOFF.md 的模块入口、接口和回收语义。
- 本轮未 commit / push；下一步按实际使用需求扩展同步接口。

## 2026-11-13 按 userdata 声明自动组装模块表

### 核心逻辑

- userdata 注册按声明的点分路径自动创建父模块，类型表同时挂到父表字段和 require 入口。
  例如 sync.channel 自动生成 cosmos.sync 表及其 channel 字段，不再重复手写模块层级。
- 删除 Rust Sync 聚合类型，构造函数移回各类型的 #[userdata] impl，接口统一为
  sync.channel.new、sync.notify.new、sync.semaphore.new、sync.mutex.new；Lua sync 表继续存在。
- builtin::open 复用自动创建的 cosmos 根表；删除被通用注册替代的 push_type 包装。
- 自动组装也覆盖网络父模块，删除 cosmos-network/lua/modules.lua 的 TCP 聚合表与对应内嵌源码条目。

### 踩坑与验证

- 子模块可能先于父类型注册；父类型必须补全原有表，不能替换已被 require 缓存的表或丢失子模块。
  push_module 逐级复用表并恢复临时栈，新增回归覆盖父/子两种注册顺序、重复注册、Lua 增补字段与类型表身份。
- new 返回 Self 时，旧同步 trampoline 在 impl 外显式引用 Self 导致编译失败；改为直接推导返回值的 PushLua，
  同时消除有无返回值的重复分支，构造函数可使用常规 Rust 写法。
- 构建、严格 Clippy 和 18 项单元测试通过；sync.lua 的 215 项检查及新增模块身份断言通过。
- websocket/http/userdata/spawn/ext/sleep、TCP/TLS 单进程和双进程回归通过，error.lua 按预期退出 1。
- 本轮未 commit / push。下一步继续沿用声明路径注册模块，新增类型只维护自身构造/方法与类型注册清单。

## 2026-11-13 提交同步原语与模块自动注册

- 按用户要求，先将 cosmos.sync、按声明自动组装模块表、配套测试和文档提交到本地 Git，不 push。
- 提交前差异检查通过；沿用上一轮构建、严格 Clippy、18 项单元测试及 examples 回归结果。
- 下一步将网络 TCP/TLS 文件按 cosmos.network.tcp 的模块层级整理，原 tcp.rs 改为 stream.rs。

## 2026-11-13 按 TCP 模块层级整理网络文件

- 已先提交同步原语与自动模块注册，提交号为 `9a093b4`，未 push。
- 将 `cosmos-network/src/tcp.rs` 移为 `tcp/stream.rs`，将原 `tls.rs` 移为 `tcp/tls.rs`，
  新建 `tcp/mod.rs` 声明 stream 与 tls，移除根层的 tls 模块声明。
- 桥接、HTTP、WebSocket 的引用改为 crate::tcp::stream / crate::tcp::tls，TCP 子模块间使用 super 路径。
  没有新增转发导出、兼容别名或包装层；Lua 的 cosmos.network.tcp.stream / tls 路径和协议行为保持原样。
- 逐文件核对迁移内容，仅调整模块说明和引用路径；没有残留旧 Rust 模块路径。同步更新 HANDOFF.md 代码地图。
- `cargo build`、严格 Clippy、18 项单元测试通过；websocket/http/userdata/spawn/ext/sleep、
  TCP/TLS 单进程及双进程 examples 回归通过，error.lua 按预期退出 1。
- 目录整理改动尚未 commit / push；下一步按实际模块扩展继续维护对应目录层级。

## 2026-11-13 按 WebSocket 模块层级拆分文件

- 将原 websocket.rs 拆为 websocket/mod.rs 与 websocket/conn.rs，对应 network.websocket 和其 conn 子模块。
  mod.rs 保留 Ws.connect、连接选项及 ws/wss 握手，conn.rs 保存 WsConn、帧收发、关闭和升级后的回调执行。
- 桥接层直接按类型所在模块导入；HTTP 升级完成后调用 websocket::conn::run_conn，并更新相关注释和 HANDOFF.md。
- 客户端和服务端共用 WsConn::new 初始化自动 pong/close、消息上限和共享帧流；字段保持私有，
  移除只有构造函数调用的 new_stream 包装。Lua 模块路径、收发行为、超时和资源所有权保持原样。
- `cargo build`、严格 Clippy、18 项单元测试通过；实际运行 examples/websocket.lua 与 examples/http.lua，
  ws/wss、升级、文本/二进制、关闭/超时、HTTP/HTTPS 和 h2 回归全部通过。
- 本轮未 commit / push，保留上一轮尚未提交的 TCP 目录整理。下一步按实际模块扩展继续维护目录与声明路径的一致性。

## 2026-11-13 提交网络模块目录整理

- 按用户要求，将 TCP stream/TLS 与 WebSocket 的目录整理、Rust 引用更新及配套文档一并提交到本地 Git，不 push。
- 提交前差异检查通过；沿用已完成的构建、严格 Clippy、18 项单元测试以及 TCP/TLS、HTTP、WebSocket 等 examples 验证结果。
- 下一步按实际模块扩展继续维护文件层级与 Lua 声明路径的一致性。

## 2026-11-13 按声明式模块注册方式重写 time

### 核心逻辑

- 用 Time + #[userdata(name = "time")] 实现时间模块，builtin 统一 register，父表与 require 入口自动生成。
  删除 time.rs 的裸 FFI、手写 C trampoline 和方法注册函数；模块只保留普通 Rust 同步/异步方法。
- Lua 入口统一为 cosmos.time.sleep/now_ms/now_us（也可 require("cosmos.time")），迁移所有 examples、
  压测脚本和类型化回调测试中的旧根表调用。sleep 仍返回零个值，两个时钟仍共用首次读取时初始化的单调起点。
- 新增基层 Duration: FromLua，统一读取有限、非负、至多 1e12 的毫秒数，time.sleep 与 sync.Timeout 共用。
  参数转换在挂起前完成，非法参数仍抛 Lua 错误；同时按 trait 规范严格拒绝非 number，移除旧 lua_tonumber 的隐式转换。

### 验证与下一步

- 扩充 examples/sleep.lua，实际验证 require 表身份、非法/缺省参数、零与小数毫秒、零返回值、
  单调时钟单位与共用起点、实际休眠时长，以及上限时长等待的取消。
- cargo build、18 项单元测试及严格 Clippy 通过；sync.lua 215 项检查通过，error.lua 按预期退出 1。
- websocket/http/userdata/spawn/ext/sleep、TCP/TLS 单进程及双进程 examples 回归通过。
- HTTP 压测服务端和客户端使用短窗口实际运行，完成 1215 个请求、0 错误，验证迁移后的毫秒/微秒计时与服务端休眠。
- 已更新 AGENTS.md 与 HANDOFF.md；本轮未 commit / push。下一步继续按声明式注册维护内建模块。

## 2026-11-13 修正 now 为真实系统时间

- 根据用户明确的语义要求，now_ms / now_us 改用 SystemTime 读取当前 Unix 毫秒/微秒整数时间戳，
  删除 OnceLock/Instant 的首次调用起点；系统时间调整会反映到返回值。sleep 仍使用 Tokio 定时器。
- 上一轮沿用旧单调计时语义不符合 now 的含义，现已修正；同步更新 HANDOFF.md 与时间模块注释。
- examples/sleep.lua 改用 os.time 校验时间戳范围和单位，移除单调时钟起点与不倒退断言。
  另用宿主 Python 的 time.time_ns 在执行脚本前后夹取时间，确认两个返回值均落在当前系统时间范围内。
- 构建、18 项单元测试及严格 Clippy 通过，sleep/ext/http/sync 实际用例通过，sync 仍为 215 项检查。
- 本轮未 commit / push；后续时间接口继续明确时间戳单位与时钟语义。

## 2026-11-13 提交时间模块重写

- 按用户要求，提交 time 声明式注册、真实 Unix 毫秒/微秒时间戳、共用 Duration 转换及配套示例和文档，不 push。
- 提交前差异检查通过；沿用已完成的构建、严格 Clippy、18 项测试、examples 回归及宿主系统时间核对结果。
- 后续时间接口继续明确时间戳单位与时钟语义。

## 2026-11-13 删除外部扩展示例

- 按用户要求，移除 cosmos-ext-demo 的 Cargo 路径依赖、锁文件条目、宿主注册和 examples/ext.lua。
  已删除工作区旁的 ../cosmos-ext-demo 源码目录及其构建目录，并清理本项目中该包的编译缓存。
- 删除 HANDOFF.md 中过期的外部路径依赖说明与示例运行命令；此前进度条目作为历史记录保留。
  当前代码、依赖、示例和交接说明中均无该演示 crate 或其 Accumulator/slow_add 接口的引用。
- cargo build、cargo test --workspace --locked（18 项）及严格 Clippy 通过；Cargo metadata 确认已无外部演示依赖。
- 实际验证 userdata/spawn/sleep/sync/http/websocket 用例全部通过；运行时确认演示类型的 require 入口和 slow_add 已移除。
- 本轮未 commit / push；后续示例与回归继续使用工作区内的运行时和网络模块。

## 2026-11-13 封装 Machine 并支持插件与 Runtime Handle 注入

### 核心逻辑

- 新建 cosmos::Machine 与 Module trait，根库直接重导出 Machine/Module/Network，外部可以通过 cosmos_rs 调用。
  Machine 独占 Lua VM 和 LocalSet，只保存宿主注入的 Tokio Handle，不创建或关闭 Runtime。
- Machine::new 初始化标准库和 builtin；add_module 在注入的 Runtime/LocalSet 上下文同步注册插件，
  支持 register::<T>、load 定义和 spawn 本地任务，不要求插件 Send/Sync，也不暴露 Lua 裸指针。
- 网络库实现 Network 插件，类型注册及内嵌 Lua 源码加载由插件负责，删除原裸指针注册入口和 LUA_SOURCES。
- exec、exec_with_args、exec_file 为 async，宿主在注入的 Runtime 上驱动，代码/参数使用长度感知的 Lua API。
  脚本之间保留全局状态与后台任务；缺少 Tokio 上下文或 Runtime ID 不匹配时返回错误，不执行 Lua。
- CLI 改为 Machine 的调用方，保留文件读取失败退出 2、Lua 失败退出 1、脚本参数和 TCP/TLS 测试服务。
  加载失败与脚本结束均通过 Machine 回收资源；Drop 固定先取消 LocalSet 任务，再 cleanup/lua_close。

### 线程与生命周期处理

- Handle 注入支持 current_thread/multi_thread Runtime；Lua 协程、userdata 和 HTTP 回调始终由 LocalSet 在宿主线程推进，
  Machine 与执行 Future 保持 !Send。宿主管理 Runtime 的存活，不在 Machine 内嵌套 Handle::block_on。
- 同步 load 在临时协程中轮询定义，遇到异步等待会返回错误并回收，避免 lua_pcall 主状态 yield 后遗留 TLS Future。
- 原全局 VM_ALIVE 不能支持多个 Machine。新增共用 RegistryRef，按每次 VM 生命周期持有独立 Rc<Cell<bool>>，
  LuaValue、LuaFunction、StrAnchor、LuaCoroutine 共同使用，关闭一个 VM 不会影响另一个 VM。
  引用统一通过主状态 unref；旧回调在 VM 关闭后返回错误，即使新 VM 复用地址也不会误访问。
- LuaCoroutine 按字段顺序先释放挂起 Future 的借用守卫，再解除协程锚定；取消执行后 userdata 可再次正常调用。
- HTTP 客户端池缓存键增加 Runtime ID，避免在同一宿主线程上切换 Runtime 后复用已暂停的旧连接驱动。
  Tokio 的稳定 Runtime ID 需要 >=1.49，更新核心依赖下限，锁文件仍使用原有 1.54.1。

### 验证与下一步

- 新增 examples/machine.rs 和 machine.lua，实际运行多线程 Runtime + Network + 自定义插件、跨脚本状态与文件参数。
- 新增 6 项嵌入回归，覆盖两种 Runtime 的线程亲和、插件上下文、本地后台任务、VM 隔离、引用释放、
  Runtime 身份检查、编译/运行/模块加载错误恢复、执行取消、VM 关闭顺序、旧回调及 HTTP 池隔离。
- cargo test --workspace --locked 的 24 项测试及 Machine !Send 编译失败用例通过；all-targets 构建和严格 Clippy 通过。
- 实际运行 userdata/spawn/sleep/sync/http/websocket、TCP/TLS 单进程及双进程、CLI machine.lua 和 Rust 嵌入示例全部通过；
  error.lua 按预期退出 1。更新 AGENTS.md 与 HANDOFF.md 的嵌入 API、线程模型和资源回收说明。
- 保留上一轮外部演示删除改动；本轮未 commit / push。后续插件统一通过 Module 注入，由宿主提供 Runtime Handle。

## 2026-11-13 提交 Machine 与插件注入

- 按用户要求，提交 Machine、Module、Handle 注入、隔离与生命周期修复、嵌入示例及配套文档，包含此前外部演示清理。
- 提交前差异检查通过，沿用已完成的 24 项测试、Machine !Send 检查、严格 Clippy 及 examples 回归结果，不 push。
- 下一步评估 HTTP 连接池由 client 实例持有的接口与生命周期。

## 2026-11-13 HTTP 连接池改为 per-client

### 核心逻辑

- HttpClient 从静态入口改为持有 Hyper 连接池的可克隆 userdata，HttpClient.new({ ctx = ... }) 每次创建独立池，
  client:request(method, url, opts) 复用该实例的连接；删除线程局部 CLIENTS、client_for 和 Runtime/TLS 指针索引。
- ctx 在构造时读取为 TLS 配置快照，后续修改原 TlsContext 只影响新建 client；headers/body/timeout/redirects/max_body
  保留在请求选项。请求级 ctx 显式报错，提示移到构造函数，避免配置被静默忽略。
- request 使用拥有型 self，经 FromLua 克隆共享 Hyper 句柄，不跨 await 占用原 Lua userdata 借用，
  同一 client 的并发请求与 HTTP/2 多路复用保持可用；原 userdata 被 GC 后在途请求仍可完成。
- 池由 client/在途句柄持有，最后一个句柄释放后回收空闲连接；TLS 默认配置可以共享，连接池不共享。
  移除 connector 中不可能出现的无 TLS 配置分支；补齐 TokioTimer，使原本配置的 30 秒空闲超时实际生效。
- 迁移 HTTP/WebSocket/压测示例及 Machine 嵌入测试，更新 HANDOFF.md 的接口和生命周期说明。

### 验证与下一步

- 扩充 examples/http.lua：验证同实例 keep-alive 复用、不同实例及同 TLS context 的池隔离、TLS 快照、
  非法请求级 ctx、userdata GC 后在途请求完成；既有重定向、超时、HTTP/HTTPS、h2 并发和大 body 回归通过。
- 新增服务端保持 keep-alive、等待客户端 EOF 的回归，确认 GC 回收 client 会释放空闲连接，且不依赖 Machine 关闭。
- all-targets 构建、cargo test --workspace --locked（25 项及 Machine !Send 编译失败用例）、严格 Clippy 通过。
- 实际运行 HTTP 与 WebSocket examples、Rust Machine 示例通过；压测脚本 4 个并发 worker 共享同一 client，
  短窗口完成 2954 次请求、0 错误。
- 本轮未 commit / push。后续 HTTP 调用统一显式创建并复用 client 实例。

## 2026-11-13 提交 per-client HTTP 连接池

- 按用户要求，将实例连接池、TLS 配置快照、调用迁移、生命周期回归及配套文档提交到本地 Git，不 push。
- 提交前差异检查通过，沿用已完成的构建、25 项测试、严格 Clippy、HTTP/WebSocket 回归及压测脚本验证结果。
- 后续 HTTP 调用统一显式创建并复用 client 实例。

## 2026-11-13 新增 cosmos-cli 并清理根包

### 核心逻辑

- 新增 cosmos-cli 工作区成员，使用 clap derive 解析脚本路径、脚本参数、帮助和版本；
  CLI 创建单线程 Tokio Runtime，通过 Handle 初始化 Machine 并注入 Network，不接触裸 Lua 栈。
- 支持文件脚本和 `-` 标准输入；参数通过 Lua chunk 的 `...` 传入，参数/输入错误退出 2，
  Lua 编译或执行错误退出 1。保留 Counter 及 TCP/TLS echo 环境变量入口，原 examples 继续可用。
- 根 Cargo.toml 改为虚拟 workspace，默认成员为 cosmos-cli；删除根 src/ 及冗余 cosmos_rs 重导出包，
  核心类型从 cosmos 引入、网络插件从 cosmos-network 引入。
- demo 与 echo 服务迁到 cosmos-cli/src；Rust 嵌入示例和 Machine 集成测试迁到 cosmos-network，
  更新 AGENTS.md、HANDOFF.md 和 Lua 用例中的运行命令。

### 踩坑、验证与下一步

- SCRIPT 与 ARG 分别声明时，紧跟脚本路径的 --help 会被 clap 当成工具选项；
  改用从 SCRIPT 开始的 trailing_var_arg 连续参数组，脚本之后的 --help、--version、-- 和空字符串均原样透传。
  路径保留 OsString 以支持非 UTF-8 文件名，Lua 参数显式校验 UTF-8。
- 新增 4 项 CLI 进程回归；cargo build --workspace --all-targets --locked、
  cargo test --workspace --locked（29 项及 Machine !Send 编译失败用例）、严格 Clippy 全部通过。
- 新二进制实际运行 userdata/spawn/sleep/sync/http/websocket、TCP/TLS 单进程及双进程全部通过，
  error.lua 按预期退出 1；CLI machine.lua 与迁移后的 Rust 嵌入示例通过。
- HTTP 压测脚本以 4 个 worker 在短窗口完成数千次请求、0 错误；默认 cargo run、帮助/版本及 workspace 元数据验证通过。
- 本轮未 commit / push。后续以 cosmos-cli 作为可执行入口，验证核心及宏时使用 cargo test --workspace。

## 2026-11-13 清除 CLI 中的测试服务与示例注册

- 按用户要求删除 CLI 的两个 echo 端口参数、对应环境变量处理和整个 echo.rs，
  清除仅由测试服务使用的 rustls、tokio-rustls、rustls-pemfile 直接依赖及 clap env 功能。
- 移除 CLI 的 demo 模块和 Counter 默认注册。Counter 迁至 cosmos/examples/userdata.rs，
  仅由独立 Rust 示例宿主显式注册，再运行根 examples/userdata.lua；正式 CLI 只解析参数并驱动 Machine/Network。
- 踩坑：迁移宿主入口时误将旧测试辅助逻辑一同放进 CLI；测试服务和示例类型应留在测试或 examples，
  不作为正式工具的隐藏功能。更新 AGENTS.md、HANDOFF.md 与 userdata 用例运行方式。
- all-targets 构建、29 项测试及 Machine !Send 编译失败用例、严格 Clippy 通过；
  CLI 回归确认 Counter 不存在，手工确认旧端口选项被拒绝、旧环境变量不再生效。
- 实际运行独立 userdata 示例、HTTP/WebSocket/Machine Lua 用例及 TCP/TLS 双进程用例全部通过。
- 本轮未 commit / push。后续 CLI 保持通用执行入口，自定义 userdata 通过宿主显式注册。

## 2026-11-13 删除剩余早期原型测试遗留

- 按用户要求彻底删除 Counter Rust 演示和 examples/userdata.lua，不再通过迁移目录保留原型类型。
- 删除依赖旧内置 echo 服务的 examples/tcp.lua、examples/tls.lua，以及仅故意触发错误的 examples/error.lua；
  TCP/TLS 保留使用当前 API 的双进程用例，CLI 错误退出路径继续由进程集成测试覆盖。
- 清除 CLI 测试和运行时/宏注释中的 Counter 遗留；宏文档改为当前 network.http.client 声明路径，
  CLI 测试改为检查未注入测试或示例命名空间。同步删除 AGENTS.md、HANDOFF.md 中失效的文件与命令引用。
- 全仓检查正式入口、builtin 初始化、Network 注册、环境变量、内嵌资源和测试条件编译：
  未发现剩余的测试服务自动启动、示例类型默认注册或证书嵌入正式代码；网络模块保持零裸 Lua 栈操作。
  保留当前 API 的单元/集成回归、Machine 嵌入示例、HTTP 压测脚本及仍供 TLS/HTTPS/WSS 用例使用的证书。
- all-targets 构建、29 项测试及 Machine !Send 编译失败用例、严格 Clippy 与差异检查通过；
  实际运行 http/websocket/spawn/sleep/sync/machine、TCP/TLS 双进程及 Rust Machine 示例全部通过。
- 本轮未 commit / push。后续功能验证使用现有独立用例和 cargo test，不在正式入口增加测试开关或演示类型。

## 2026-11-13 提交独立 CLI 与原型清理

- 按用户要求，将 cosmos-cli、虚拟 workspace、嵌入示例/测试迁移、原型服务与演示清理及配套文档提交到本地 Git。
- 提交前差异检查通过，沿用已完成的 all-targets 构建、29 项测试、Machine !Send 检查、严格 Clippy 和 examples 回归结果。
- Machine 执行语义保持不变：主协程结束后 exec 返回；宿主保留 Machine 可继续执行，CLI 则销毁 Machine 并退出。
- 不 push。后续以独立 CLI 为执行入口，测试辅助逻辑不进入正式工具。

## 2026-11-13 统一 LightThread 并细分内部职责

### 核心逻辑

- 删除独立的 LuaCoroutine 与 coroutine.rs，主脚本、同步定义、类型化 LuaFunction 回调及 Lua 后台任务
  统一使用 cosmos::LightThread；Lua cosmos.thread 与 cosmos.LightThread 路径保持不变。
- 新建 light_thread/：mod.rs 组织唯一执行权、直接/后台调度及执行守卫；execution.rs 的私有 Execution
  仅持有 Lua 状态、挂起操作和 registry 锚点，不实现 Future；control.rs 仅管理取消、最终结果和等待者通知。
- run 构造 Future 时立即取走 Execution，执行期间独占状态机，不跨 lua_resume 或 await 借用共享 RefCell。
  Machine 与回调直接 await 同一执行入口，不增加 Tokio 任务；spawn 才交给 LocalSet。
- 回调结果在锚点有效时通过 FromLua 读取；结束/取消时先释放挂起操作中的借用，再解除锚定并通知等待者。
  尚未首次轮询的执行 Future 被 drop 也会通过同一守卫发布取消并释放执行资源。
- 删除 Lua 任务的 JoinHandle + oneshot 双份完成结果，Control 保存唯一结果并按需登记 waker，
  等待者取消时注销通知。join 使用拥有型克隆句柄，支持多个协程同时等待及等待期间 kill。
- join/wait 均可重复观察已完成任务的成功、错误或取消；wait 不再拿走其他任务的接收端，
  未选中的任务、重复参数和参数转换失败后的有效任务都可继续等待。
- builtin/light_thread.rs 只保留 Lua 参数/结果适配与注册；宏及 network bridge 的 yield 路径同步更新。
  AGENTS.md、HANDOFF.md 更新统一模型、分层、生命周期与验证说明。

### 踩坑与验证

- 取消检查必须位于每次 lua_resume 之间：Execution 每步最多恢复一次，由执行守卫检查取消并控制
  每轮最多 256 次恢复，防止自我取消后继续执行，也避免立即就绪的操作链饿死其他任务。
- 新增 3 项 Rust 生命周期/通知回归，覆盖唯一执行权、未 poll 即取消时锚点与参数 GC、
  挂起借用先于完成通知释放、多个等待者唤醒及取消等待不偷通知；保留原 PendingOp 与类型化回调测试。
- 扩充 examples/spawn.lua，实际验证并发 join + kill、重复 wait/join、取消等待者、非法参数、
  重复句柄、自我取消、丢弃句柄后的后台执行，以及立即就绪操作的调度公平性。
- all-targets 构建、cargo test --workspace --locked（32 项测试及 Machine/LightThread 的 2 项 !Send
  编译失败用例）、严格 Clippy、差异检查通过；http/websocket/spawn/sleep/sync/machine、
  TCP/TLS 双进程与 Rust Machine 嵌入示例均实际运行通过。
- HTTP 压测脚本在 4 worker 的短窗口完成 2874 次请求、0 错误；此结果只验证调用链与并发可用性，
  不作为性能对比。统一共享状态会增加一个 Rc 状态分配，后台任务去掉 oneshot，等待登记按需分配。
- Machine/CLI 退出语义不变；本轮未 commit / push。后续协程执行统一通过 LightThread，内部组成部分不对外暴露为独立实体。

## 2026-11-13 提交 LightThread 合并与内部分层

- 按用户要求，将统一 LightThread、Execution/Control 分层、取消/等待语义、回归用例和配套文档提交到本地 Git。
- 提交前差异检查通过，沿用已完成的构建、32 项测试、2 项 !Send 检查、严格 Clippy 与 examples 回归结果，不 push。
- 下一步逐个检查 workspace crate 的可见性，重点收窄 cosmos 内部类型、函数与模块，同时保留宏展开和插件所需的跨 crate 桥接接口。

## 2026-11-13 收窄各 crate 的 Rust 可见性

### 核心逻辑

- 前一轮 LightThread 合并已按用户要求提交为 3bd7ea1，不 push；本轮可见性修改继续留在工作区，未自动提交。
- cosmos 的 builtin/light_thread/stack 实现模块改为私有；libs 仅 cfg(test) 编译，正式加载入口保持 Machine::load。
  保留 Machine/Module、LightThread 句柄、userdata 桥接和最小 ffi 公开面。
- VM 创建/关闭/恢复、registry 操作、userdata 初始化/清理/裸注册/push 收为 pub(crate)；
  main_state、MethodTable::new 与字符串拷贝阈值为私有。LuaStr 改为公开但字段私有的类型，
  内部存储枚举与 StrAnchor 不再公开，FromLua 和 Deref<str> 行为保持不变。
- 宏与叶子桥接所需的 yield_with、PendingOp/PendingFuture、abs_index 通过 userdata 统一公开导出；
  更新宏与 network bridge 的路径，避免为了外部宏展开而公开整个运行时/栈模块。
- ffi 仅保留实际供宏/叶子转换使用的 9 个函数及必要类型/常量公开；其他声明限 crate 内，
  luaL_loadstring/lua_pcall 仅测试编译，删除未使用的 luaL_newmetatable 声明。
- cosmos-network 的协议实体与共享转换类型限 crate 内，本文件使用的 options/response 类型私有，
  TCP/TLS 共用的状态、读写半、缓冲字段及 size_limit 限 tcp 模块，Headers::get/version_num 私有。
  Rust 外部入口仍仅为 Network，Lua require 路径与方法不变。
- cosmos-macros 仅保留必需的 3 个过程宏入口公开；CLI 的函数与 Args 原本已私有。
  四个 crate 均启用 unreachable_pub，防止借私有父模块掩盖过宽的 pub 声明。

### 踩坑、验证与下一步

- 过程宏在调用方 crate 中展开，桥接函数不能直接全部改为 pub(crate)；逐项核对生成代码和跨 crate 使用方后，
  保留最小公开桥接。LuaStrMap 字段等确实由外部叶子转换组合使用的值接口保留公开。
- all-targets 构建、32 项测试、Machine/LightThread 的 2 项 !Send 检查及严格 Clippy 全部通过。
- 独立 rustc 消费者实际编译并运行 Module 注入、FromLua/PushLua derive、异步 userdata、
  8/4096/4097/8192 字节参数与 GC；15 项内部模块、VM 操作、锚点、阈值、私有字段/构造访问均按预期编译失败。
- 实际运行 http/websocket/spawn/sleep/sync/machine、TCP/TLS 双进程与 Rust Machine 示例全部通过。
- 用生成文档核对公开面：cosmos 仅公开 ffi/userdata 模块及宿主入口，network 仅公开 Network。
  修复检查发现的属性宏链接歧义和泛型 HTML 格式警告，严格 rustdoc（-D warnings）与差异检查通过。
- 更新 AGENTS.md/HANDOFF.md 的可见性约定、桥接入口和源码地图；后续新增接口按实际调用范围选择 private/pub(super)/pub(crate)，
  仅宿主 API 和外部桥接使用 pub，不自动 commit / push。

## 2026-11-13 明确 AGENTS.md 的可见性约束

- 按用户要求，将此前的简要可见性说明整理为独立的核心约束章节，明确默认私有、最近共同祖先范围、
  字段与表示封装、各 crate 公开边界，以及宏展开所需桥接的例外和验证要求。
- 补充不能借私有父模块或无必要的公开重导出掩盖过宽声明；移除工程约定中的重复说明。
- 本轮仅修改文档，差异检查通过，不重复运行代码测试，未 commit / push。后续实现遵守该章节。

## 2026-11-13 提交可见性收窄与工程约束

- 按用户要求，先提交各 crate 的可见性收窄、LuaStr 表示封装、宏桥接导出、文档及 AGENTS.md 可见性核心约束。
- 提交前差异检查通过，沿用已完成的构建、32 项测试、2 项 !Send 检查、严格 Clippy/rustdoc、外部插件和 15 项访问拒绝验证，不 push。
- 下一步按已确认方案改造受控读写/调用上下文，迁移宏及 network bridge，最后将 ffi 完全设为内部模块。

## 2026-11-13 封闭 FFI 并迁移受控桥接协议

### 核心逻辑

- 按用户要求先提交此前可见性及约束改动，提交为 af92e5f；本轮 FFI 改造未自动提交。
- ffi 模块以及全部声明改为 crate 内可见，不再重导出 C API、状态类型、栈常量或 ABI。
  外部仅通过 userdata 的 Value/Output/Call、表读写器和类型转换协议组合功能。
- FromLua<'lua> 接收 Value<'lua>，PushLua 接收 Output；TableReader 统一字符串键遍历、绝对索引、
  嵌套转换与错误/panic 时栈恢复，TableWriter 与 LuaStrMap 共用字段构造，拒绝零/多返回值作为单个表字段。
- 过程宏只生成字段/分支转换及 Rust LuaCallable 适配；MethodTable 按适配类型单态化 cosmos 内部 C trampoline，
  不在使用方生成 extern C、裸指针或 lua_* 调用，也不分配回调元数据。Call 统一参数、借用、返回和异步挂起。
- LuaClosure 组合 userdata 状态与同一 LuaCallable 协议，TCP until 不再手写 upvalue 索引、C 闭包或 yield。
  network bridge 只组合值转换、配置快照、Rust 调用适配和类型注册，整个 network crate 不再依赖 ffi。
- 新增 &[u8] 基层 PushLua，String/&str/Vec<u8> 与网络 Bytes 输出复用它，不为 Bytes 先构造中间 Vec。
  每个异步操作仍只装箱一次 Future，不引入返回值闭包装箱。

### 生命周期与踩坑

- 旧的无生命周期 FromLua 允许安全的 LuaFunction::call 泛型返回未锚定的 &str；改为读取生命周期约束，
  回调与运行入口要求结果对所有读取生命周期均可转换，裸借用结果不能逃逸，拥有型数据/registry 引用继续支持。
- 同步参数仍支持字符串及带借用字段的结构体。表字段可能被重入回调删除，不能只依赖原表存活；
  Call 内部 ReadScope 仅为借用字段字符串按需保存 registry 锚点，直到输出完成才释放。
  普通标量、拥有型字段和直接字符串参数不增加该锚点分配。
- Value/Output/Call 构造器及原始字段均私有，保持线程亲和；userdata 借用仍须由调用帧、捕获状态或独立锚点保活。
  LuaCallable 是 unsafe trait，明确要求准确输出数量、挂起及守卫生命周期，宏自动履行该契约。
- 泛型 derive 合并读取生命周期与字段约束；已有高阶 FromLua 约束不重复添加，避免 Rust trait 选择歧义。
  空结构体、零参数异步方法及调用方 Result 别名纳入跨 crate 验证。

### 验证与下一步

- all-targets 构建、36 项测试、5 项编译失败检查（线程约束、私有 ffi、借用不能逃逸）、严格 Clippy/rustdoc 通过。
- 新增表构造/遍历中途 panic 的栈恢复、NUL 键/值及非字符串键回归；外部插件实际覆盖 derive、
  同步借用、异步长字符串、捕获状态闭包、二进制输出、空结构体、错误恢复及重入删除字段后 GC。
- 网络跨 crate 回归覆盖 TCP 迭代器的 8 KiB 二进制数据、超时、取消挂起调用后再次使用及关闭 partial 返回。
- 实际运行 http/websocket/spawn/sleep/sync/machine、TCP/TLS 双进程及 Rust Machine 示例全部通过；
  HTTP 短窗口压测 4 worker 完成 2947 次请求、0 错误，仅作为功能与并发验证。
- 扫描确认 network、macros、CLI 与外部桥接测试没有裸 Lua API/状态类型/C ABI；
  文档公开模块只剩 userdata，9 项外部访问私有 ffi、上下文构造器/原始字段及裸桥接函数均被编译器拒绝。
- AGENTS.md 与 HANDOFF.md 更新更严格的 FFI 边界、上下文签名、生命周期和源码地图。Lua 接口保持原样；
  Rust 自定义转换实现需要使用新上下文及 FromLua 生命周期。本轮未 commit / push，后续桥接能力继续在 cosmos 内逐层组合。

## 2026-11-13 封闭 FFI 并引入受控桥接上下文

### 核心逻辑

- 前序可见性与 AGENTS.md 约束已按用户要求提交为 af92e5f；本轮 FFI 封装未自动 commit / push。
- FromLua<'lua> 改为读取不暴露状态/索引的 Value<'lua>，PushLua 改为写入 Output；
  同步字符串及表字段借用由生命周期约束，回调/执行结果要求高阶 FromLua，阻止临时协程借用逃逸。
- Value/TableReader 封装类型选择、绝对索引、表遍历与失败恢复；Output/TableWriter 统一构造表字段，
  LuaStrMap 与 derive 共用这些转换操作，保持非字符串键跳过、未知字段忽略、默认值及多返回值顺序。
- userdata 宏只生成 Rust LuaCallable 标记类型和参数/结果组合；MethodTable::add 在内部单态化 C trampoline，
  LuaClosure 的捕获状态调用与宏方法共用 Call、错误边界和挂起协议，无额外回调元数据分配。
- LuaCallable 为 unsafe trait，必须通过 Call 生成准确结果或挂起操作；Future 仍只装箱一次，返回值不额外装箱。
  同步结果可借用输入；异步输入必须拥有数据或锚点，取消时先释放借用再解除协程锚点。
- network bridge 全面迁移为上下文组合：Bytes 通过新增 &[u8] 基层 PushLua 输出，不增加中间 Vec；
  TCP 迭代器只提供 Rust 状态调用，TLS 参数只构造受控借用后的配置值，不再出现 C 函数、栈常量或裸状态。
- ffi 模块、全部声明/类型/常量限制在 cosmos 内；移除公开 yield、PendingOp、abs_index、原始 trampoline 等入口，
  文档确认公开模块仅 userdata。更新 AGENTS.md 与 HANDOFF.md 的边界、接口迁移及源码地图。

### 踩坑与验证

- C ABI 不能通过换路径重导出来；本轮把完整的参数转换、错误处理、输出与挂起收回通用调用桥接，
  仅让宏描述字段、分支和方法组合。方法和捕获闭包确实复用同一内部 trampoline。
- 表构造必须检查字段单值输出，并在未完成、错误或 panic 时恢复栈；同步借用回传不能借用整个 Call 的可变引用。
  FromLua derive 同时处理生命周期/泛型约束，避免调用方 Result 别名和空字段/空参数场景影响宏展开。
- 新增外部插件、带状态异步闭包、二进制字节、长字符串/GC、同步借用回传与表字段借用回归；
  新增表上下文错误/panic 栈恢复和 NUL 字符串 map 回归，以及 TCP 迭代器超时、取消、部分数据/EOF 回归。
- all-targets 构建、36 项测试、5 项编译失败检查（FFI 私有、借用逃逸/回调结果约束和两项 !Send）、
  严格 Clippy/rustdoc 与差异检查通过；12 项独立外部底层访问尝试被编译器拒绝。
- 实际运行 http/websocket/spawn/sleep/sync/machine、TCP/TLS 双进程及 Rust Machine 嵌入示例通过；
  4 worker 短窗口 HTTP 压测完成 3040 次请求、0 错误，仅作为并发调用链验证，不作性能对比。
- 回答用户的 C .so 问题时，在仓库外用当前 LuaJIT 头文件编译临时 C 模块，macOS 上 require 加载、
  含 NUL 字符串回显及参数错误处理均通过。本轮未新增 C 依赖、专用注入接口或符号导出配置，其他平台未验证。
- 后续功能模块与宏展开只依赖受控上下文；Rust FFI 可见性与 C 动态模块的 ABI/符号导出分别处理，不自动提交。

## 2026-11-14 按实际容器语义重命名 StringPairs

- LuaStrMap 实际封装 Vec<(String, String)>，是可包含重复键的字符串键值对列表；改名为 StringPairs，
  同步核心转换实现、network Headers 组合、回归与交接文档，不保留冗余别名。
- 注释区分 Rust 列表与 Lua table 的语义：输出时后项覆盖同名键，读取顺序取决于 Lua 遍历。
  本轮仅修改名称与说明，数据结构和转换行为不变。
- cargo test --workspace --locked 的 36 项测试及 5 项编译失败检查通过，差异检查通过。
  未 commit / push；后续采用 StringPairs 表达该列表转换原语。

## 2026-11-14 提交受控桥接与 StringPairs 重命名

- 按用户要求，将私有 FFI、受控读写/调用上下文、宏与 network bridge 迁移、借用生命周期及锚点、回归与文档一并提交，包含 StringPairs 重命名。
- 提交前差异与遗留引用检查通过，沿用已完成的 36 项测试、5 项编译失败检查、严格 Clippy/rustdoc、外部边界与 examples 验证结果，不 push。
- 后续外部模块继续通过受控上下文组合值，字符串键值对列表统一使用 StringPairs。

## 2026-11-14 补充项目使用说明

- 新增根目录 README.md，简要介绍构建、CLI 脚本与参数、标准输入、HTTP 服务、Rust Machine 嵌入和现有示例入口，以使用为主。
- 明确 CLI 在主协程结束后退出、常驻服务需保持主协程运行；区分 Machine 默认内建模块与显式注入的 Network，说明 Runtime Handle 的使用要求。
- cargo build --locked 通过；从 README 提取 Lua 与 Rust 代码，在临时目录验证参数、标准输入、HTTP 响应和独立宿主项目，全部通过；文档相对链接均有效。HTTP、WebSocket、TCP 双进程与 Rust Machine 嵌入示例实际运行通过，差异检查无空白问题。
- 本轮只修改文档，不自动提交；后续新增使用方式时同步维护 README 和可运行示例。

## 2026-11-14 将 CLI 网络支持改为默认 feature

- cosmos-cli 新增默认启用的 network feature，以 dep:cosmos-network 控制可选依赖；导入与插件注入统一按 feature 编译，关闭后仅保留内建模块。
- 同步 README 构建说明与 HANDOFF 入口说明。现有 CLI 参数测试原先无条件创建网络客户端，改为独立验证 feature 对注册结果的影响；依赖网络的脚本测试仅在启用时运行。
- 默认构建及 cargo test --workspace --locked 通过（37 项测试、5 项编译失败检查）；无默认 feature 构建、CLI 的 4 项集成测试及严格 Clippy 通过，依赖树确认不包含 cosmos-network；格式与差异检查通过。
- 实际运行默认构建的 HTTP、WebSocket 示例，以及无网络构建的 sleep、spawn、sync 示例全部通过；禁用网络时 require 网络模块按预期失败。
- 未自动提交；后续 CLI 可选插件沿用显式 feature 与条件注入方式。

## 2026-11-14 按项目定位精简并拆分 README

- 主 README 改为项目用途、特点、CLI 和 Machine 两个使用入口；删除展开的 Lua 服务示例、重复依赖配置和 cargo run 命令，示例直接链接现有文件。
- 新增 cosmos-network/README.md，集中说明网络能力、CLI feature、宿主插件注入及各协议的示例链接；根文档仅保留插件入口。
- 两份 README 的相对链接与命令检查通过；提取精简后的 Machine 代码，在临时宿主中实际执行 Lua 文件通过；差异检查通过。
- 本轮只调整文档；沿用上一轮网络 feature 与 examples 验证结果，后续协议使用说明维护在 network README，主文档保持简洁。不自动提交。

## 2026-11-14 补充 README 异步运行模型

- 主 README 增加简短的异步模型说明：每个受管理的 Lua 协程绑定 Rust Future，由 Tokio 驱动；异步操作挂起、就绪后返回结果并恢复 Lua。
- 说明 LocalSet 下的单线程协作式调度及多线程 Runtime 中的线程亲和性；不将每个 Lua 协程描述为必须独立 spawn 的 Tokio task，主脚本与回调也可由调用方直接 await 对应 Future。
- 对照 LightThread::run、Execution::poll_step 与 Machine::exec_with_args 核对描述，文档差异检查通过；仅修改文字，未重复运行行为测试。后续保持主 README 的设计说明简短，不自动提交。

## 2026-11-14 支持 CLI 与宿主注入 C 扩展搜索路径

- CLI 新增可重复的 --cpath，接受分号分隔的搜索模板，按出现顺序前置到已有 package.cpath；参数必须在脚本路径前，脚本后的同名参数仍原样透传。
- Machine 新增 prepend_cpath，复用同步加载流程并以 Lua chunk 参数传入路径，避免拼接 Lua 源码与转义问题；空字符串保持原值，CLI 不接触裸 Lua 栈。
- 核对 LuaJIT 自带初始化：默认搜索 ./?.so、/usr/local/lib/lua/5.1/?.so 与 loadall.so；LUA_CPATH 原生支持，;; 在指定位置展开默认路径。保留原生替换语义，不硬编码另一份默认表，不更改进程环境；LUA_PATH 保持原样。README 与 HANDOFF 同步说明。
- 新增默认路径、环境变量展开、重复 CLI 顺序、分号模板、中文/引号/反斜杠与脚本参数边界回归。工作区 38 项测试与 5 项编译失败检查通过，无默认 feature 的 CLI 5 项测试通过；工作区严格 Clippy 通过。
- 在 macOS ARM64 上临时编译 4 份可辨认来源的 C 扩展，验证原生当前目录、LUA_CPATH、CLI 优先级与两级回退共 6 种实际加载场景，并验证二进制字符串；HTTP、WebSocket、spawn、sleep、sync 示例全部通过。
- 本轮未增加 C 依赖或异步 C 桥接协议，未自动提交；后续继续验证其他平台与发布构建的动态符号导出。

## 2026-11-14 配套支持 Lua 文件模块搜索路径

- CLI 新增可重复的 --path，对应 package.path；Machine 新增 prepend_path，与 prepend_cpath 共用私有的搜索路径前置实现，保留已有模板并通过 chunk 参数传值。
- LUA_PATH 延续 LuaJIT 原生初始化及 ;; 默认路径展开，CLI 模板按出现顺序优先搜索；--path 和 --cpath 独立配置，脚本路径后的同名参数继续透传。README 合并为 Lua / C 模块路径对照，HANDOFF 同步更新。
- 将路径顺序、环境展开、特殊字符与默认路径回归扩展到两种模块；新增真实 require 回归，覆盖普通文件、点号模块、?/init.lua、重复 CLI 顺序、环境与当前目录回退，共 6 种加载场景。子进程单独配置环境与工作目录，避免影响并行测试。
- 工作区 39 项测试与 5 项编译失败检查通过，无默认 feature 的 CLI 6 项测试通过；工作区严格 Clippy、格式与差异检查通过。带 --path / --cpath 实际运行 HTTP、WebSocket、spawn、sleep、sync 示例均通过。
- 本轮未自动提交；后续 Lua 与 C 模块路径配置保持配套，继续沿用链接的 LuaJIT 原生默认值。

## 2026-11-14 实测 LuaRocks 生态兼容性

- 在临时目录构建 LuaRocks 3.13.0，以 Lua 5.1 / LuaJIT 配置实际安装 dkjson 2.11-1 与 LuaFileSystem 1.9.0-1，C 包使用当前 cosmos 构建产物的 LuaJIT 头文件；未向项目引入依赖。
- dkjson 通过 LuaRocks path 导出的 LUA_PATH / LUA_CPATH 以及 CLI --path / --cpath 两种方式加载，中文、NUL 字符串 JSON 编解码及异步挂起后继续调用均通过。
- LuaFileSystem 在当前 macOS ARM64 debug CLI 中加载失败：缺失 _luaL_checkoption。nm 确认模块所需的 Lua API 中仅该符号缺失，且定义确实存在于 LuaJIT 静态库，最终可执行文件未导出；路径查找与包安装本身正常。
- 修正前序最小 C 扩展验证的适用范围：部分 API 可用不代表完整 LuaRocks C 包生态兼容。后续需统一处理 LuaJIT C API 在最终链接产物中的保留与导出，并覆盖真实 C rocks 及不同平台/发布配置。
- 临时 LuaRocks 工具、rocks tree 与缓存已清理。本轮仅调查并记录结果，未修改实现，未自动提交。

## 2026-11-14 核对并验证 C API 符号导出修复方向

- nm 确认 luaL_checkoption 位于已参与链接的 lj_api.o，不能仅靠 whole-archive 解决目标文件内部的符号裁剪；还须在最终宿主显式保留并动态导出 Lua C ABI。
- 在当前 macOS CLI 临时使用 -exported_symbol 匹配 _lua_*、_luaL_*、_luaopen_*、_luaJIT_* 链接，确认 luaL_checkoption 出现在最终导出表；实际编译并加载调用该 API 的 C 扩展，正常分支与参数错误均通过。
- 正式方案需配套处理静态库完整链接及最终宿主的平台导出配置，覆盖 CLI 和外部 Machine 宿主；依赖库的 rustc-link-arg 不能作为宿主自动获得最终链接配置的保证。Rust ffi 可见性保持私有。
- 本轮仅验证方案，未修改构建实现；临时 C 模块已清理，CLI 恢复默认构建。后续正式实现需用真实 C rocks、release/LTO 与各目标平台回归，不自动提交。

## 2026-11-14 本地验证依赖库链接配置向外部宿主传播

- 使用当前 cosmos / cosmos-macros 的隔离副本及工作区外的独立 Machine 宿主，在 macOS ARM64 / Rust 1.94.1 debug 构建下做四组对照：默认、库侧 rustc-link-arg、库侧 whole-archive 加 link-arg、再由宿主显式设置导出参数。
- 库侧构建日志确认实际输出四个导出参数，但 rustc --print link-args 显示它们没有进入外部宿主的最终链接命令；nm 仍无 luaL_checkoption，真实 C 模块加载仍报缺符号。
- rustc-link-lib=static:+whole-archive 的元数据确实传播：最终命令出现 -force_load。不过仍不保留/导出该函数，实际加载继续失败。宿主 build.rs 输出相同导出参数后，最终命令、符号表、C 模块正常调用及参数错误测试全部通过。
- 结论从文档推断收敛为本地实测：原生库链接修饰符会传播，依赖库的 rustc-link-arg 不会；仅修改 cosmos/build.rs 不能完成外部宿主的导出配置。
- 报告、四组原始日志、结果 JSON 与临时验证脚本保存在 target/cosmos-link-propagation-g4xxxqeo/；隔离源码与编译目录已清理。项目实现与原 CLI 二进制保持原样，未自动提交；后续正式方案据此处理宿主链接边界。

## 2026-11-14 为 CLI 配置 C API 导出并记录 Machine 宿主要求

- 新增 cosmos-cli/build.rs，以目标平台而非构建主机选择参数；仅对 cosmos-cli 最终二进制导出 lua_*、luaL_*、luaopen_*、luaJIT_*，macOS 使用 -exported_symbol，Linux 使用 --export-dynamic-symbol，不公开 Rust ffi 或增加 C 依赖。
- 当前 LuaJIT 静态库的 148 个相关 API 在 macOS debug 及 release / fat LTO CLI 中均完整保留并导出，无需额外 whole-archive 或改动 cosmos/build.rs。release 验证使用 profile.release.lto="fat"、codegen-units=1。
- README 新增 C 扩展节，说明 Lua 5.1 / LuaJIT ABI、LuaRocks 路径配置，以及外部 Machine 宿主必须自行设置最终链接参数；提供 CLI build.rs 参考，提醒替换宿主二进制名并合并已有导出配置。HANDOFF 同步更新。
- 新增最终 CLI 动态符号查找回归，覆盖四个 API 符号族与之前缺失的 luaL_checkoption。工作区 40 项测试与 5 项编译失败检查、无默认 feature 的 CLI 7 项测试通过；工作区严格 Clippy、格式与差异检查通过。
- 从 LuaRocks 的 LuaFileSystem 1.9.0-1 源码包编译真实 lfs.so，debug、release / fat LTO 及无 network 构建均通过文件属性、目录迭代、参数错误及异步挂起后再次调用验证。HTTP、WebSocket、spawn、sleep、sync 示例实际运行全部通过。
- 临时源码包及 C 扩展已清理。Linux 导出参数已配置，本轮未在 Linux 上构建或运行；本机验证为 macOS ARM64。不自动提交，后续平台回归仍需覆盖实际 Linux 环境。

## 2026-11-14 提交 CLI 模块配置、C 扩展支持与使用文档

- 按用户要求，将默认 network feature、Lua / C 搜索路径参数与 Machine 配置 API、CLI C ABI 导出及回归、项目与网络 README、交接和进度记录一并提交，不 push。
- 提交前核对完整差异与新增文件，沿用已通过的工作区 40 项测试、5 项编译失败检查、无默认 feature 回归、严格 Clippy、examples 和 macOS debug / release-LTO 真实 C 扩展验证结果。
- 后续保留外部 Machine 宿主自行配置最终链接的边界；Linux 配置仍待实际环境回归。

## 2026-11-14 调研 lua-resty-mysql 的 cosmos 适配范围

- 用户提供本地仓库 /Users/ellutionist/code/github/openresty/lua-resty-mysql，调研基于 0.29 / 3af1b39；随后明确先列系统改造与生态补齐项，本轮未修改驱动或运行时实现。
- 原库协议主体可保留 Lua，运行依赖集中于 ngx.socket.tcp、ngx.null、ngx.sha1_bin、ngx.config，以及 resty.sha256、可选 resty.rsa / resty.mysql.auth_ed25519。实测仅添加 Lua 路径时在 resty.sha256 缺失处加载失败。
- 系统侧需梳理 socket 接口与返回值适配、连接后 TLS 升级、Unix domain socket 和独立 NULL 表示；现有 TCP/TLS 读写、Machine / Module、LocalSet 与受控桥接可以复用。MySQL TLS 在明文 greeting 和 SSLRequest 之后升级，现有 Tls.connect 的建连即握手模式不能直接替代。
- 生态侧需补二进制 SHA-1 / SHA-256 / SHA-512、RSA-OAEP-SHA1 及 PEM 公钥处理、MariaDB 特殊 Ed25519 签名、MySQL 连接池与租借/取消语义、SQL 引用工具，以及面向真实 MySQL / MariaDB 的 CLI 用例与回归。SQL 预编译语句属于上游尚未实现的新能力，区别于本轮运行时移植。
- 连接池需明确 Machine 或显式客户端隔离，保留复用次数、空闲过期、pool_size/backlog 等语义，并禁止未读尽结果、超时/取消后的不完整连接回池；HTTP 的 hyper 池不能直接复用为普通 TCP 池。
- MariaDB client_ed25519 直接以 SHA-512(password) 作为扩展密钥来源，不能直接套用会再次派生密钥的普通 Ed25519 seed 签名 API。迁移时保留现有长密码 RSA scramble 索引修复、NULL、大整数文本精度和多结果集行为。
- 本机仅发现 MySQL 8.4 客户端，没有服务端；尚未做真实数据库认证或查询验证。后续优先明确公共能力与独立 cosmos.mysql Lua 包的边界，再逐项实现与验证，不自动提交。

## 2026-11-14 第一阶段：空值与顺序协议传输基础

- cosmos.null 采用空指针 lightuserdata，与 ngx.null / cjson.null 的表示一致。新增公开桥接原语 userdata::Null 与 ValueKind::Null，和 Nil 分开，支持 derive、异步返回、表/数组及通道传递；FFI 仍限内部，新增跨 crate 回归和 examples/null.lua。
- Network 新增 cosmos.network.socket，提供 connect/connect_unix、send/receive、settimeout、close，以及 TCP/Unix 监听。复用原 TCP/TLS 读写器与读取状态机；异步收发独占同一 socket，失败、超时或取消后丢弃连接，保留 partial 返回值。旧 split API 保持原有超时后可继续使用的语义。
- starttls / starttls_server 在原连接上升级 TLS，PlainIo 回放 BufReader 已预读的字节，支持 SSLRequest 与 ClientHello 同时到达；明文 TCP、Unix 与 TLS 通过封闭的 Duplex 组合，已有 HTTP/TLS 入口也使用同一底层传输。
- Unix TLS 的大于 8 KiB 回归暴露 tokio-rustls 可能先接受全部明文、仍有密文待发送；补齐 write_all 后同一 deadline 下的 flush，避免双方转入等待响应后停滞。重复 split 改为 nil+错误，不触发 Rust panic。
- 空值回归、socket 升级/取消/部分写入回归、工作区测试及严格 Clippy 已通过；新增 examples/network_socket.lua 覆盖 TCP/Unix 的原地 TLS、大块二进制、超时 partial、读取与握手取消、握手超时及证书拒绝。实际运行 null、network_socket、HTTP、WebSocket、spawn、sleep、sync 及 TCP/TLS 双进程用例均通过。
- 按用户的两阶段安排，后续 encoding / encryption 使用 Rust 原生实现，复用 Null 与受控桥接；本轮未自动提交。

## 2026-11-14 第二阶段：Rust 原生 encoding / encryption 插件

- 新增 cosmos-encoding 与 cosmos-encryption 两个独立 crate，只公开 Encoding / Encryption 这两个 Module 入口。CLI 默认启用对应 feature，也可分别关闭或单独启用；Machine 保持显式注入，不向核心 VM 加入算法依赖。模块表继续由 userdata 声明路径自动组装。
- Encoding 首批提供 Hex、标准 Base64、无 padding 的 URL-safe Base64 和 URI 组件百分号编解码；全部支持二进制字符串，严格校验非法编码，URI 解码不将加号转换为空格。本轮未引入 JSON 接口。
- Encryption 提供 SHA-1 / SHA-256 / SHA-512 原始摘要、SPKI 与 PKCS#1 PEM 公钥的 RSA-OAEP-SHA1/256，以及标准 Ed25519 和 MariaDB client_ed25519 的直接 SHA-512 材料派生。算法使用 RustCrypto / ed25519-dalek，RSA 使用系统安全随机数；未引入 OpenSSL 或其他 C 算法库。
- RSA 构造与公钥方法统一在同一类型，删除仅用于工厂分组的空壳类型。Ed25519 公钥和扩展状态封装在密钥对象内部，只接受 seed 或私密材料，不暴露任意扩展密钥、随机数前缀与调用方混入的公钥。
- 桥接补充同步借用 &[u8] 的 FromLua，不做 UTF-8 转换；临时表字段借用复用调用作用域锚点。扩展外部插件回归，同时验证字符串和二进制字段在重入删除字段、强制 GC 后仍有效，异步参数仍禁止裸借用。derive 的可选缺省字段须明确标注 #[lua(default)]，不能仅靠 Option 推断。
- 编解码通过 RFC 4648 向量、全 256 字节往返和非法输入回归；Ed25519 通过 RFC 8032 与上游 MariaDB 向量。排查并修正手写 RFC 签名 fixture 多出的一个十六进制字符，实际签名与官方向量一致。RSA 两种公钥格式、两种 OAEP 摘要的私钥解密回归通过，并实际使用 OpenSSL 解密 CLI 生成的二进制密文验证互操作；OpenSSL 仅作测试对照。
- 工作区 47 项测试及 5 项编译失败检查通过；CLI 无默认 feature、仅 encoding、仅 encryption 各 8 项测试通过；工作区及无默认 feature CLI 的严格 Clippy 通过。实际运行 encoding、encryption、null、network_socket、HTTP、WebSocket、spawn、sleep、sync 示例全部通过；格式和差异检查通过。
- 新增两个插件 README 和 examples，根 README 链接插件文档并解释 cosmos.null 的真值语义；HANDOFF 与模块命名约定同步更新。不自动提交。
- 下一步仍是独立 MySQL Lua 驱动、连接池及真实 MySQL/MariaDB 的认证和查询回归；当前没有数据库服务端，不能把算法向量和传输回归等同于完整驱动验证。

## 2026-11-14 接入 cosmos-database.mysql 并验证真实数据库

- 按用户要求新增 cosmos-database crate，只公开 Database 插件，MySQL 位于 cosmos.database.mysql；自动注入 Network / Encoding / Encryption，Rust 只处理插件装配和 URL 选项转换，协议与池保留 Lua 实现。CLI 新增默认 database feature，依赖所需的三个已有 feature；核心 Machine 不增加默认插件。
- 以本地 lua-resty-mysql 0.29 / 3af1b39 为基础移植，去除 ngx / resty 运行依赖和早期调试代码，保留原始版权并附完整 BSD 许可；不改动上游仓库，也不引入 MySQL C 客户端或新的 C 算法依赖。新增独立插件 README、第三方声明和 examples/mysql.lua，根文档、HANDOFF、模块命名约定同步更新。
- 支持 mysql_native_password、caching_sha2_password、sha256_password 和 MariaDB client_ed25519，保留长密码 RSA scramble 的正确循环索引；SHA-2 认证切换的 nonce 按 20 字节读取，避免把终止 NUL 算入摘要或 RSA scramble。对照 MySQL 8.4 官方认证源码确认 nonce 终止语义，离线测试覆盖直接认证和 AuthSwitch 后的完整认证。
- 连接使用顺序 socket，支持 TCP、Unix、原连接 TLS 升级，默认验证 TLS 证书，可注入 ctx / server name。增加 URL 解析与百分号、IPv6、TLS 参数校验，错误不回显含密码的 URL。SQL 字符串引用组合 encoding.hex 生成 X'…'，不依赖反斜线转义模式。
- client 实例各自持有空闲池，pool_size 仅限制空闲数，idle_timeout 控制过期，租借先 COM_PING，失效时重新连接；归还将底层连接移交新表，旧句柄不能操作新租借。未读结果、多结果集的 again、未结束事务、autocommit 关闭、I/O 失败或取消均禁止回池；不实现命名全局池与 backlog，传入时明确拒绝。其他会话设置由调用方恢复。
- 协议收发增加序号、字段长度和逻辑包大小校验，支持 16 MiB 物理包分帧、整帧后的空终止帧，以及 0xfe 长行与 EOF 区分。协议异常立即 abort；完整 SQL ERR 包保留 errno / sqlstate，连接仍可继续用；COM_QUIT 从序号 0 开始，不声明 LOCAL INFILE 能力。
- 真实认证暴露前置遗漏：原 TcpWriter / socket 发送参数使用 LuaStr，短二进制有损 UTF-8 解码，长二进制被拒绝，造成握手损坏。新增核心 LuaBytes，短串复制、长串 registry 锚定，统一用于两种发送入口；桥接回归覆盖 4096 阈值两侧，TCP 短包和 TLS 长包示例改为真实非 UTF-8 字节。
- 重入 GC 回归暴露 Option<T> 重新构造 Value 会丢失调用作用域和临时字段标记，可能使 Option<&[u8]> 悬垂。改为组合传递原 Value；外部插件回归在删除原字段、强制 GC 和内存覆写后校验字符串与二进制借用，并验证跨异步等待的二进制参数。
- 用户提供的 library 库实测服务端为 MySQL 8.4.5。明文 TCP 与 TLS 均通过认证、中文/二进制/NULL/BIGINT、临时表增删与事务回滚、SQL 错误后恢复、多结果集、紧凑数组、池隔离/复用/过期、超时、取消和并发回归；通过终止自己创建的已归还连接验证失效重连。未修改业务表、用户配置或服务端全局状态，凭据仅通过环境变量传入，没有写入仓库。
- 默认离线测试不访问外部数据库；显式设置 COSMOS_MYSQL_URL 并 --ignored 才执行真实回归。工作区 52 项测试与 5 项编译失败检查通过，真实 MySQL 明文 / TLS 各一轮显式测试通过；无默认 feature CLI 9 项、仅 database CLI 10 项测试通过；工作区及无默认 feature CLI 严格 Clippy、Rust 格式、README 链接与差异检查通过。
- 实际运行 encoding、encryption、null、network_socket、HTTP、WebSocket、spawn、sleep、sync，以及 TCP / TLS 双进程二进制回归全部通过。未自动提交；后续可补真实 MariaDB / MySQL Unix socket 环境回归及服务端预编译语句，当前不宣称这些已完成。

## 2026-11-14 提交 MySQL 适配与公共基础能力

- 按用户明确要求，将顺序 socket / TLS 升级、Null 与二进制桥接修复、Encoding / Encryption / Database 插件、CLI feature、用例与文档统一提交，不 push。
- 提交前核对全部差异与新增文件、第三方许可和凭据配置方式，沿用本轮已完成的工作区 52 项测试、5 项编译失败检查、CLI feature 组合、严格 Clippy、实际 examples 与真实 MySQL 明文 / TLS 验证结果；没有行为变更，不重复执行已通过的完整回归。
- 当前 MySQL 池为 client 级，pool_size 限制空闲连接数。后续优先考虑活跃连接上限与等待队列、会话状态重置、DECIMAL 精度、查询总超时，以及真实 MariaDB / Unix socket 和长期并发回归；本次只提交现有成果。

## 2026-11-14 接入 Redis 并拆分数据库 feature

- 按用户要求，将本地 lua-resty-redis 0.34 / 74d0c11 适配为 cosmos.database.redis，仍放在 cosmos-database；Lua 实现 RESP2、命令编码与动态生成、pipeline、事务、Pub/Sub 和 client 池，Rust 只负责 URL 转换与插件装配。保留原始版权和 BSD 全文，不修改上游仓库，不注入 ngx / resty 或引入裸 Lua FFI。
- 数据库 crate 新增默认全开的 mysql / redis feature，控制对应代码、内嵌 Lua、类型注册和可选依赖；只有 Redis 时仅安装 Network，MySQL 才安装 Encoding / Encryption。两者共用 URL 百分号、主机与布尔转换；全关时 Database 注册为空操作。
- CLI 的 database feature 改为 mysql + redis 合集，并提供单库开关；对 cosmos-database 显式 default-features=false，防止禁用的数据库被依赖默认值重新开启。实际验证全关、仅 MySQL、仅 Redis、两者同时开启的模块入口与普通依赖树，未启用的模块不能 require。
- Redis 支持 TCP / Unix / TLS、URL、ACL AUTH、SELECT 和三段超时，字符串保持 NUL / 非 UTF-8 / CRLF。nil bulk / array 使用 cosmos.null，顶层 Redis 错误返回 false+err，嵌套与 pipeline 错误保留 {false, err}；超出 Lua 安全整数范围的 int64 回复返回十进制字符串。
- 解析严格验证行结束、bulk 长度与终止符、int64 范围、数组和嵌套；默认请求/回复 64 MiB、数组 65536、深度 128，pipeline 最多 10000 条且请求与回复各共用字节预算，Pub/Sub 缓冲同时限制数量和字节。未知 RESP3 前缀、HELLO 3、MONITOR、复制流、分片订阅与改变回复模式的 CLIENT 子命令明确拒绝。
- Redis 池继续按 client 隔离，pool_size 只限制空闲数，支持过期和 PING 检测失效；归还转移到底层连接的新句柄，旧句柄失效。pipeline 未读完、订阅、MULTI、WATCH、公开 AUTH / SELECT 等会话变更、收发失败或取消均禁止回池。事务解析覆盖 EXECABORT、WATCH 导致的空 EXEC 和 pipeline 内事务；RESET 清理本地状态，但不允许该连接重新入池。
- 接入时处理原生名称冲突：redis.client(opts) 是池工厂，conn:client(...) 仍是 CLIENT 命令；通用调用使用 execute，保留 conn:command(...) 为 Redis COMMAND。重复订阅用频道集合跟踪，不累加错误计数；多参数 unsubscribe 必须读完全部 ACK，等待普通响应时缓冲频道消息。
- 用户提供的 192.168.31.163:6379 实测为 Redis 8.10.3，且授权使用测试服务器。新增 examples/redis.lua，独立键前缀、TTL 和末尾清理，验证二进制/NULL/int64、哈希、pipeline/取消、事务/WATCH/EXECABORT、Pub/Sub、池复用/隔离/失效重建、超时/取消、并发、RESET，以及创建/删除本轮临时 ACL 用户并验证认证和 SELECT 隔离；未更改已有用户或清空数据库。
- 新增 Redis 离线协议回归，覆盖 AUTH / SELECT 的二进制参数、分片接收、嵌套错误、异常长度/终止符/整数/深度/数组上限、pipeline 总回复预算、部分 pipeline 与 Pub/Sub 读超时关闭；本地 TCP / Unix 的 TLS 用可信 CA 和主机名校验。外部服务器用例仍须显式设置 COSMOS_REDIS_URL 并 --ignored。
- 工作区 59 项测试与 5 项编译失败检查通过；数据库空 feature / mysql / redis 独立测试分别通过，CLI 空 feature / mysql / redis / database 组合测试通过；工作区与数据库/CLI 的单库、空配置严格 Clippy 通过。实际用仅 Redis 的 CLI 运行 Redis 示例、仅 MySQL 的 CLI 运行原 MySQL 示例均通过，已恢复默认 feature CLI；格式、文档链接与差异检查通过。
- README 按总览、mysql.md、redis.md 拆分，根 README 与 HANDOFF 同步记录 feature、接口、验证范围和暂不支持的协议模式。不自动提交；后续仍可补有界活跃池、RESP3 / 分片订阅和长时间压力回归，未在本轮宣称完成。

## 2026-11-14 提交 Redis 与数据库 feature 拆分

- 按用户明确要求，将 Redis 驱动、独立数据库 feature、CLI 选择、回归用例、第三方许可和文档一并提交，不 push。
- 提交前核对完整差异和新增文件，沿用已通过的工作区 59 项测试、5 项编译失败检查、feature 组合与依赖隔离、严格 Clippy，以及仅 Redis / 仅 MySQL CLI 的真实服务器回归；本轮没有行为修改，不重复执行完整回归。
- 用户询问如果只再接一个数据库的建议：优先评估 PostgreSQL 与 pgmoon，复用现有异步网络、TLS 和 Lua 协议适配路径；后续需核对 SCRAM 认证、密码学依赖和类型转换。本轮只提交和调研建议，未开始接入新数据库。
