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
