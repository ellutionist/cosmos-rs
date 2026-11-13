# HANDOFF — cosmos-rs（交给 Codex）

> 写给接手的 agent：读完这份再动手。约定优先于个人偏好；用户是中文母语、C++ 背景、对分层和分配很敏感。

## 0. TL;DR

- 项目：LuaJIT + Rust 重写版网络框架，路径 `/Users/ellutionist/code/ellutionist/cosmos-rs`。
- 工作区有**大量未提交改动**（WebSocket 模块 + 桥接分层重构 + `AGENTS.md`），全部已验证通过，**未 commit**。
- 下一个待办是"统一 `IntoOutput` 与 `PushLua`"，方案已定但**用户尚未点头**，先确认再动手（见 §5）。
- 交流、注释、文档一律**中文**；**不要自动 commit / push**。

## 1. 环境与结构

```
cosmos-rs/
  cosmos/            # 运行时 + 桥接层：ffi / userdata / coroutine / stack / libs / thread
  cosmos-macros/     # #[userdata]、#[derive(FromLua/PushLua/IntoOutput)]，生成全部 trampoline
  cosmos-network/    # 功能模块：tcp / tls / http / websocket + bridge.rs（叶子转换）
  src/main.rs        # 宿主：建 VM、跑脚本、关 VM；demo.rs 是 userdata 示例
  examples/*.lua     # 全部端到端用例
  lua/               # cosmos-network 内嵌 Lua 源码（include_str! 随二进制编译）
```

- LuaJIT 2.1（Lua 5.1 C API），单线程：tokio `current_thread` + `LocalSet`，Lua 协程 `!Send`。
- 依赖：hyper 1 + hyper-util（client/server，h2 走 ALPN）、rustls+ring、fastwebsockets（upgrade feature）。
- 最近提交：`4f796f6 feat: serde-like table derives and namespaced Lua modules`，之后的改动都在工作区未提交。

## 2. 硬性约定（详见 `AGENTS.md`）

1. **功能模块零 FFI**：tcp / tls / http / websocket 里不出现 `cosmos::ffi`、不直接调 `lua_*`、不做裸栈操作。
2. **桥接 = 组合**：基层类型各实现 `PushLua` / `FromLua` / `IntoOutput`，结构体靠 derive，枚举靠 derive，
   多返回值靠元组/枚举字段顺序。**禁止**把命令式栈操作换个文件存放（用户明确说过那是"马甲"，没有意义）。
3. 允许裸 FFI：cosmos 运行时/桥接层、`cosmos-network/src/bridge.rs` 的叶子转换与 C trampoline、`src/main.rs`（宿主）。
4. 不做死抽象；公共层要有明确复用点。
5. 任何行为改动必须实际跑 `examples/` 对应用例（含回归），不能只看编译通过。
6. 只有用户明确要求才 commit / push。

## 3. 工作区现状（全部未提交）

```
M Cargo.lock
M cosmos-macros/src/lib.rs
M cosmos-network/Cargo.toml          # + fastwebsockets
M cosmos-network/src/http/{client,mod,server}.rs
M cosmos-network/src/{lib,tcp,tls}.rs
M cosmos/src/{lib,stack,userdata}.rs
M src/{main,demo}.rs
?? AGENTS.md
?? cosmos-network/src/bridge.rs      # 新增：crate 桥接层
?? cosmos-network/src/websocket.rs   # 新增：WebSocket 模块
?? examples/websocket.lua            # 新增：24 项端到端用例
```

## 4. 最近完成的工作

### 4.1 WebSocket（服务端复用 HTTP 端口）

- 服务端不单独 listen：HTTP handler 对选定 path 返回
  `{ websocket = function(conn, req) ... end }`。
- `cosmos-network/src/http/server.rs`：
  - `upgrade_websocket()`（约 264 行）用保留的请求 `Parts` 调 `fastwebsockets::upgrade::upgrade`，
    返回 101；非 upgrade 请求返回 400；HTTP/2 无 Upgrade 机制，同样走 400。
  - 连接驱动改为 `serve_connection_with_upgrades`；`handle()` 里给 ws 分支重建一份请求 userdata。
- `cosmos-network/src/websocket.rs`：
  - `Ws.connect(url, opts)`：`ws://` 明文、`wss://` 走 TlsContext（缺省公有根，强制 ALPN `http/1.1`）；
    opts = `{ headers, timeout, ctx, max_message }`，超时覆盖 DNS+TCP+TLS+握手并作为连接初始读写超时。
  - `WsConn`（`cosmos.network.websocket.conn`）：`send`（校验 UTF-8）/`send_binary`/`ping`/`receive`/
    `close`/`state`/`settimeout`；`receive` 返回 `("text"|"binary", data)`，关闭 `nil,"closed"`，
    超时 `nil,"timeout"`（连接可继续用）。ping 自动 pong、close 自动回 close。
  - 帧流放在 `Rc<RefCell<Option<FragmentCollector<...>>>>`：handler 协程结束后服务端强制 close；
    协程被取消时随 userdata 回收关闭。
  - `run_conn()`（约 203 行）通过 `LuaFunction::call_in_new_thread((conn, request))` 跑 Lua 协程。
- 上限：`ServerOptions.ws_max_message` / 客户端 `max_message`，缺省 64 MiB。
- 注意：`FragmentCollector<S>` 包的是**流** `S`，不是 `WebSocket<S>`。

### 4.2 桥接分层重构

- 新增 `cosmos-network/src/bridge.rs`：只放叶子转换（`Headers`、`LuaBytes`、`ReceivePattern`）、
  `until` 闭包的 C trampoline、全部类型注册。功能模块已 grep 不到任何 `lua_*` / `cosmos::ffi`。
- cosmos 新增基层原语（都在 `cosmos/src/userdata.rs`）：
  - `impl PushLua / FromLua for Vec<u8>`（二进制安全）；
  - `pub struct Nil`（614 行）：代表"一个 nil"，与 `()`（0 个返回值）区分；
  - `pub struct LuaStrMap`（349 行）：Lua table(string→string) ↔ 有序键值表，`Headers` 由它组合；
  - `pub unsafe fn push_closure<T: UserData>`：把 C 函数包成带 userdata upvalue 的闭包；
  - `LuaFunction::call_in_new_thread`（518 行）：建协程 + 压函数/参数，替代各处手写 `lua_newthread`；
  - `impl PushLua for &str`、`impl<A,B: PushLua> IntoOutput for (A,B)`。
- `cosmos/src/stack.rs`：`ReturnKind` + `return_kind()`，HTTP handler 返回值分类不再手写栈检查。
- `cosmos-macros`：新增 `#[derive(IntoOutput)]`（584 行）：枚举按分支字段顺序压栈，字段需 `PushLua`。
- `cosmos-network/src/tcp.rs`：`ReceiveOutcome` 改为
  ```rust
  #[derive(cosmos::IntoOutput)]
  pub enum ReceiveOutcome {
      Data(Vec<u8>),
      Eof(Nil),
      Closed(Nil, &'static str, Vec<u8>),
      Timeout(Nil, &'static str, Vec<u8>),
      Overflow(Nil, &'static str, Vec<u8>),
      Error(Nil, String, Vec<u8>),
  }
  ```
  不再有任何手写 `match + lua_push*`。
- `cosmos-network/src/lib.rs`：`pub use bridge::register;`，外部注册入口不变。

### 4.3 VM 生命周期修复（重要）

WebSocket 用例暴露了退出段错误：脚本结束后 LocalSet 里仍挂着 ws handler 协程（持有 Lua userdata 的
`RefMut` 守卫），而 `lua_close` 先于任务回收执行 → 守卫 Drop 时读写已释放内存。

`src/main.rs` 现在：

```rust
let (L, result) = local.block_on(&rt, run(&code, &script_args));
drop(local);                 // 先回收本地任务（VM 仍存活）
userdata::cleanup(L);
lua_close(L);
```

`run()` 返回 `(*mut lua_State, Result<(), String>)`，内部不再 `lua_close`。所有错误路径也把 L 交回 main。
**不要改回"run 内部关 VM"**。

## 5. 待办（最高优先级，需先确认）

### 统一 `IntoOutput` 与 `PushLua`

用户认为两条 trait + `AsyncOutput` 枚举是冗余的，要求真正合并（不是加 `Output<T>` 包装）。已讨论过的
结论方案：

- 只保留 `PushLua`；
- 异步边界改为
  ```rust
  pub type DeferredPush = Box<dyn FnOnce(*mut lua_State) -> i32>;
  pub fn defer<T: PushLua + 'static>(v: T) -> DeferredPush;   // = Box::new(move |L| v.push_lua(L))
  ```
- 删除 `IntoOutput` trait（`cosmos/src/userdata.rs:683`）及其全部 impl；
- 删除 `AsyncOutput` 枚举（`cosmos/src/coroutine.rs:18`）及 `impl PushLua for AsyncOutput`；
  `PendingFuture = Pin<Box<dyn Future<Output = DeferredPush>>>`；
  poll loop（`coroutine.rs:149-152`）里 `out.push_lua(thread)` 改为直接调用闭包 `out(thread)`；
- async 宏体（`cosmos-macros/src/lib.rs` 的 `gen_method`）从
  `<#ret_ty as IntoOutput>::into_output(#call.await)` 改为 `cosmos::userdata::defer(#call.await)`；
- `#[derive(IntoOutput)]` 合并进 `#[derive(PushLua)]`：struct 仍生成 Lua table（`expand_push_lua`），
  enum 生成"按字段顺序压栈"（把当前 `expand_into_output` 的逻辑搬进来，输出 `impl PushLua`）；
  删除 `IntoOutput` derive 与 `cosmos/src/lib.rs` 的导出；`ReceiveOutcome` 改成 `#[derive(cosmos::PushLua)]`；
- 语义差异需记录：`Option::None` 走 `PushLua` 是 1 个 nil（现在是 0 个值），对 `local v = f()` 等价，
  只有 `select('#', f())` 能观察出差异。

**唯一代价**：每次异步返回多一次 `Box`（复合返回今天本来就走 `AsyncOutput::Custom`，不受影响；
受影响的只有返回 `()` / 标量 的 async 方法，如 `sleep`、`thread.wait`）。stable Rust 没有 specialization，
"统一"与"标量零分配"只能二选一。

**动手前先向用户确认**是否接受这次分配；用户对分配很敏感。验收：
`cargo build`、`cargo test`（7 个 derive 测试）、§7 全部 examples 通过。

### 其它 backlog（未排期，勿主动开工）

- UDP / unix socket：用户之前说"udp 补上，network 先到这"，随后改为先做 WebSocket；优先级未确认。
- HTTP/3：用户明确"先不做"。
- 压测脚本：`examples/http_bench.lua`（服务端）、`examples/http_bench_client.lua`（自己 client 打自己 server），
  历史基线见项目记录（wrk GET 13B ≈146k RPS，自研 client ≈125k）。

## 6. 代码地图

| 文件 | 职责 |
|---|---|
| `cosmos/src/ffi.rs` | LuaJIT C API 声明（唯一 FFI 声明处） |
| `cosmos/src/userdata.rs` | 桥接核心：`UserData`/`PushLua`/`FromLua`/`IntoOutput`、userdata 存放与 GC、`LuaFunction`、`Nil`/`LuaStrMap`、derive 单元测试 |
| `cosmos/src/coroutine.rs` | `LuaCoroutine`（Future）、`AsyncOutput`、TLS yield 通道、`yield_with` |
| `cosmos/src/stack.rs` | 栈辅助：`err_message`、`ReturnKind`/`return_kind` |
| `cosmos-macros/src/lib.rs` | `#[userdata]`、`FromLua`/`PushLua`/`IntoOutput` derive |
| `cosmos-network/src/bridge.rs` | 本 crate 的叶子转换、`until` trampoline、注册入口 |
| `cosmos-network/src/tcp.rs` | TCP 状态机、`ReceivePattern`/`ReceiveOutcome`、`receiveuntil` |
| `cosmos-network/src/tls.rs` | rustls 客户端/服务端、`TlsContext`（证书/信任根/ALPN/mTLS） |
| `cosmos-network/src/http/mod.rs` | `Conn`（明文/TLS 统一流）、`Headers`、`LuaBytes` |
| `cosmos-network/src/http/client.rs` | hyper legacy client、连接池、重定向、整体超时 |
| `cosmos-network/src/http/server.rs` | hyper auto server、请求 DTO、handler 协程、WebSocket 升级入口 |
| `cosmos-network/src/websocket.rs` | ws/wss 客户端、`WsConn`、服务端帧循环 |
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
# examples/error.lua 期望 exit=1（末尾故意 sleep(-1)）

# 双进程
(./target/debug/cosmos-rs examples/tcp_server.lua 19001 &) ; sleep 1
./target/debug/cosmos-rs examples/tcp_client.lua 19001     # OK
(./target/debug/cosmos-rs examples/tls_server.lua 19443 &) ; sleep 1
./target/debug/cosmos-rs examples/tls_client.lua 19443     # OK

cargo test        # 7 passed（derive 默认值/缺字段/PushLua 等）
```

## 8. 坑与注意事项

- **VM 关闭顺序**：先 `drop(local)` 再 `lua_close`（§4.3）。改了会 UAF。
- **async 方法的 self 守卫**：`#[userdata]` 生成的 async 方法会把 `RefMut/Ref` move 进 future，
  跨 await 一直占着借用；同一 userdata 的另一次调用会 `lua_error`。这是有意设计。
- **FromLua 嵌套取值要用绝对索引**：`lua_next` 遍历后再用 `-1` 会错位（`value_idx = lua_gettop(L)`）。
- **hyper upgrade**：必须 `serve_connection_with_upgrades` 且保留 `http::request::Parts`
  （`OnUpgrade` 在 extensions 里），否则 101 之后连接不会被接管。
- **h2 不能走 WebSocket upgrade**（无 RFC 8441），当前直接 400。
- **wss 客户端必须强制 ALPN http/1.1**（`with_alpn_client`），否则服务端可能协商 h2 导致握手失败。
- **`()` 是 0 个返回值，`Nil` 是 1 个 nil**；多返回值占位必须用 `Nil`。
- **`server:close()` 只停 accept**；句柄被 GC 不等于 close（与 C++ 一致）。
- 用户不接受"给函数加马甲"式的分层修补；桥接要么用原语组合，要么别做。
