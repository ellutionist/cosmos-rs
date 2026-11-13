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
