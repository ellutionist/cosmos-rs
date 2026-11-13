# 项目约定（cosmos-rs）

## 交流语言

- 与用户交流、代码注释、文档一律用中文。

## 分层与桥接（核心约束）

功能模块（cosmos-network 的 tcp / tls / http / websocket）只描述协议与状态机，
**不允许出现任何裸 Lua 栈操作**：不写 `cosmos::ffi::*`、不直接调用 `lua_*`、
不持有 `*mut lua_State` 做 gettop/type/push 之类的操作。

桥接的正确做法是**逐层组合**，不是把命令式栈操作换个文件存放（那只是马甲，没有意义）：

1. 基层类型各自实现 `PushLua` / `FromLua` / `IntoOutput`：
   标量、`String` / `&str` / `Vec<u8>`、`Nil`、字符串 map、userdata 等，
   每个类型只在桥接层实现一次；
2. 结构体用 `#[derive(cosmos::FromLua)]` / `#[derive(cosmos::PushLua)]`，
   枚举用 `#[derive(cosmos::IntoOutput)]`，由过程宏按字段/分支生成压栈与读取；
3. 多返回值用元组（`(A, B)`）或 derive 出来的枚举字段顺序表达，
   不要手写 `match variant { … lua_pushlstring(…) }`；
4. 功能模块只负责构造"值"（如 `ReceiveOutcome::Closed(Nil, "closed", partial)`），
   值如何变成 Lua 返回值完全由原语 + 宏决定。

允许出现裸 FFI 的位置仅限：

- cosmos 的运行时/桥接层（`ffi` / `userdata` / `coroutine` / `stack` / `libs` / `thread`
  以及 `#[cosmos::userdata]` 生成的 trampoline）；
- `cosmos-network/src/bridge.rs`：只放无法下沉到 cosmos 的叶子转换
  （如 `Bytes` ↔ Lua 字符串）、C 闭包 trampoline 与类型注册；
- 宿主引导 `src/main.rs`（建 VM / 跑脚本 / 关 VM）。

新增返回形态时，优先补 cosmos 的基层 trait 实现或宏能力，再组合到功能模块里；
不要为一个场景在 bridge 里新写一段栈操作。

## 其他工程约定

- **不自动 commit / push**：只有用户明确要求时才提交。
- 行为修改必须实际运行 `examples/` 下对应用例验证（含相关回归），
  不能只凭"编译通过"或"看起来对"；`cargo test` 覆盖 derive 单元测试。
- 优先 Rust 原生依赖，不引入 C 库；整体是单线程 LocalSet 模型
  （Lua 协程 `!Send`，网络任务与协程在同一线程交错推进）。
- 不做死抽象：公共层只在有明确复用点（至少两个使用方）时抽取，
  避免无意义的包装函数。
- Lua 模块命名统一 `cosmos.network.*`、`cosmos.thread`；
  userdata 用 `#[cosmos::userdata(name = "…")]` 指定 `require` 路径。

## 常用验证命令

```sh
cargo build

# 单进程用例
./target/debug/cosmos-rs examples/websocket.lua
./target/debug/cosmos-rs examples/http.lua
./target/debug/cosmos-rs examples/userdata.lua

# 双进程用例
./target/debug/cosmos-rs examples/tcp_server.lua 19001 &
./target/debug/cosmos-rs examples/tcp_client.lua 19001

./target/debug/cosmos-rs examples/tls_server.lua 19443 &
./target/debug/cosmos-rs examples/tls_client.lua 19443

cargo test
```
