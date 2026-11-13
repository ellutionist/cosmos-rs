# 项目约定（cosmos-rs）

## 交流语言

- 与用户交流、代码注释、文档一律用中文。

## 分层与桥接（核心约束）

功能模块（cosmos-network 的 tcp / tls / http / websocket）只描述协议与状态机，
**不允许出现任何裸 Lua 栈操作**：不写 `cosmos::ffi::*`、不直接调用 `lua_*`、
不持有 `*mut lua_State` 做 gettop/type/push 之类的操作。

桥接的正确做法是**逐层组合**，不是把命令式栈操作换个文件存放（那只是马甲，没有意义）：

1. 基层类型各自实现 `PushLua` / `FromLua`：
   标量、`String` / `&str` / `Vec<u8>`、`Nil`、字符串 map、userdata 等，
   每个类型只在桥接层实现一次；
2. 结构体用 `#[derive(cosmos::FromLua)]` / `#[derive(cosmos::PushLua)]`，
   枚举用 `#[derive(cosmos::PushLua)]`，由过程宏按字段/分支生成压栈与读取；
3. 多返回值用元组（`(A, B)`）或 derive 出来的枚举字段顺序表达，
   不要手写 `match variant { … lua_pushlstring(…) }`；
4. 功能模块只负责构造"值"（如 `ReceiveOutcome::Closed(Nil, "closed", partial)`），
   值如何变成 Lua 返回值完全由原语 + 宏决定。

允许出现裸 FFI 的位置仅限：

- cosmos 的运行时/桥接层（`ffi` / `userdata` / `coroutine` / `stack` / `libs` / `builtin` / `machine`
  以及 `#[cosmos::userdata]` 生成的 trampoline）；
- `cosmos-network/src/bridge.rs`：只放无法下沉到 cosmos 的叶子转换
  （如 `Bytes` ↔ Lua 字符串）、C 闭包 trampoline 与类型注册；

宿主通过 `Machine` 创建/执行/销毁 VM，`cosmos-cli/src/main.rs` 只处理命令行与脚本执行，不再手动管理 Lua 栈。

新增返回形态时，优先补 cosmos 的基层 trait 实现或宏能力，再组合到功能模块里；
不要为一个场景在 bridge 里新写一段栈操作。

## 其他工程约定

- **不自动 commit / push**：只有用户明确要求时才提交。
- 行为修改必须实际运行 `examples/` 下对应用例验证（含相关回归），
  不能只凭"编译通过"或"看起来对"；`cargo test --workspace` 覆盖 derive 单元测试。
- 优先 Rust 原生依赖，不引入 C 库；整体是单线程 LocalSet 模型
  （Lua 协程 `!Send`，网络任务与协程在同一线程交错推进）。
- `Machine` 注入 Tokio `Handle`，不接管 Runtime 所有权；宿主须在注入的 Runtime 上 await 执行 API。
  Runtime 可为多线程，Lua 及持有 Lua 引用的任务仍由 Machine 的 LocalSet 固定在宿主线程。
- 插件实现 `Module` 并通过 `Machine::add_module` 注入；注册在该 Machine 的 Runtime/LocalSet 上下文同步执行。
- 不做死抽象：公共层只在有明确复用点（至少两个使用方）时抽取，
  避免无意义的包装函数。
- Lua 状态指针统一命名为 `lua_state`，未使用时为 `_lua_state`；宏生成代码同样遵守。
- Lua 模块命名统一 `cosmos.network.*`、`cosmos.time`、`cosmos.thread`、`cosmos.sync`；
  userdata 用 `#[cosmos::userdata(name = "…")]` 指定相对 `cosmos` 的路径，注册层自动组装父模块表，
  父表字段与对应 `require` 入口复用同一张类型表。

## 进度记录

- 请将每一轮修改的核心逻辑、踩坑记录和下一步计划更新到 `PROGRESS.md`。

## 常用验证命令

```sh
cargo build

# 单进程用例
./target/debug/cosmos-cli examples/websocket.lua
./target/debug/cosmos-cli examples/http.lua

# 双进程用例
./target/debug/cosmos-cli examples/tcp_server.lua 19001 &
./target/debug/cosmos-cli examples/tcp_client.lua 19001

./target/debug/cosmos-cli examples/tls_server.lua 19443 &
./target/debug/cosmos-cli examples/tls_client.lua 19443

cargo test --workspace
```
