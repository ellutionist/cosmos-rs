# 项目约定（cosmos-rs）

## 交流语言

- 与用户交流、代码注释和项目文档使用中文。
- 代码中的错误信息、日志、断言与 panic 文案、CLI 帮助、模块/chunk 调试名称及包描述统一使用英文；文档示例中的诊断与输出文案也遵守此规则，不能用转义形式隐藏中文诊断。
- 非注释代码避免出现中文字符，标识符使用英文。仅在确有需要的 Unicode 测试数据中保留原始非 ASCII 字节，通过 Rust Unicode 转义或 Lua UTF-8 字节转义表达，不能删除相关覆盖来满足语言约束。

## 分层与桥接（核心约束）

功能模块（cosmos-network 的 tcp / udp / tls / http / websocket）只描述协议与状态机，
**不允许出现任何裸 Lua 栈操作**：不写 `cosmos::ffi::*`、不直接调用 `lua_*`、
不持有 `*mut lua_State` 做 gettop/type/push 之类的操作。

桥接的正确做法是**逐层组合**，不是把命令式栈操作换个文件存放（那只是马甲，没有意义）：

1. 基层类型各自实现 `PushLua` / `FromLua`：
   标量、`String` / `&str` / `&[u8]` / `Vec<u8>`、`Nil`、字符串 map、userdata 等，
   每个类型只在桥接层实现一次；
2. 结构体用 `#[derive(cosmos::FromLua)]` / `#[derive(cosmos::PushLua)]`，
   枚举用 `#[derive(cosmos::PushLua)]`，由过程宏按字段/分支生成压栈与读取；
3. 多返回值用元组（`(A, B)`）或 derive 出来的枚举字段顺序表达，
   不要手写 `match variant { … lua_pushlstring(…) }`；
4. 功能模块只负责构造"值"（如 `ReceiveOutcome::Closed(Nil, "closed", partial)`），
   值如何变成 Lua 返回值完全由原语 + 宏决定。

裸 FFI 仅允许出现在 cosmos 的运行时/桥接层（`ffi` / `userdata` / `light_thread` / `stack` / `libs` / `builtin` / `machine`）。
`ffi` 模块及其声明全部限 crate 内，禁止向外重导出。

`cosmos-network/src/bridge.rs` 只放叶子转换、Rust 调用适配和类型注册，
通过 `Value` / `Output` / `Call` 等受控上下文组合，不持有裸状态指针、不使用 C 函数或栈常量。
过程宏只生成 Rust `LuaCallable` 与转换组合；C ABI、错误边界和异步挂起协议统一由 cosmos 内部承担。
`FromLua<'lua>` 的读取生命周期必须与 `Value<'lua>` 一致；同步方法可以借用，跨异步回调的结果必须拥有数据或独立锚点。
表遍历的字段视图不能保存；同步借用表字段字符串时由调用作用域按需锚定，不能依赖字段在重入或 GC 后仍存在。

宿主通过 `Machine` 创建/执行/销毁 VM，`cosmos-cli/src/main.rs` 只处理命令行与脚本执行，不再手动管理 Lua 栈。

新增返回形态时，优先补 cosmos 的基层 trait 实现或宏能力，再组合到功能模块里；
不要为一个场景在 bridge 里新写一段栈操作。

## Rust 可见性（核心约束）

- 函数、结构体、枚举、模块和字段**默认私有**，只根据实际调用方扩大可见性，不为假想复用预留公开接口。
- 当前模块及其子模块使用的实现保持私有；父级或兄弟模块需要时，采用 `pub(super)` 或限定到最近共同祖先的
  `pub(in …)`；确实需要 crate 内跨模块访问时采用 `pub(crate)`；只有跨 crate 的正式接口使用 `pub`。
- 类型公开不代表其字段、构造函数、内部状态或存储枚举也应公开。对外句柄和桥接值优先隐藏表示，
  不允许调用方依赖执行状态、registry 锚点、缓存或存储阈值等实现细节。
- `cosmos` 的公开面以宿主 API（`Machine`、`Module`、`LightThread` 句柄）和插件必需的桥接为限；
  VM 生命周期、内建模块初始化、执行状态机及底层管理函数保持内部可见。测试辅助代码不作为正式公开 API。
- 过程宏在使用方 crate 中展开，所需桥接通过 `cosmos::userdata` 的受控上下文公开提供；
  不允许生成 `cosmos::ffi`、`lua_*` 或手工栈操作，也不为宏展开公开底层实现。收窄前必须核对生成代码和外部插件调用方。
- `cosmos-network` 对外只提供 `Network` 插件，协议实体及其状态、选项、返回 DTO、字段按内部实际使用范围开放；
  `cosmos-macros` 仅公开过程宏入口；`cosmos-cli` 的参数类型与执行函数保持私有。
- 各 crate 保持 `#![warn(unreachable_pub)]`；不能靠私有父模块掩盖过宽的 `pub` 声明，
  也不能通过无必要的公开重导出绕过边界。可见性调整需验证外部宏展开、插件注入与现有调用路径。

## 其他工程约定

- **不自动 commit / push**：只有用户明确要求时才提交。
- 按 module 分批推进：每完成一个模块，逐项核对是否满足迁移/功能需求，运行验证并更新进度后停下来让用户检查，等待用户的提交指令；不自动连续实现下一个模块。
- Git 提交信息（标题与正文）统一使用英文；与用户交流、代码注释和项目文档仍使用中文。
- 行为修改必须实际运行 `examples/` 下对应用例验证（含相关回归），
  不能只凭"编译通过"或"看起来对"；`cargo test --workspace` 覆盖 derive 单元测试。
- 优先 Rust 原生依赖，不引入 C 库；整体是单线程 LocalSet 模型
  （Lua 协程 `!Send`，网络任务与协程在同一线程交错推进）。
- `Machine` 注入 Tokio `Handle`，不接管 Runtime 所有权；宿主须在注入的 Runtime 上 await 执行 API。
  Runtime 可为多线程，Lua 及持有 Lua 引用的任务仍由 Machine 的 LocalSet 固定在宿主线程。
- Lua 执行统一使用 `LightThread`；私有 Execution 负责栈与挂起操作，Control 负责取消与完成通知，
  builtin 只适配 Lua 接口。执行状态不实现独立 Future，不在 lua_resume 期间借用共享 RefCell。
- 插件实现 `Module` 并通过 `Machine::add_module` 注入；注册在该 Machine 的 Runtime/LocalSet 上下文同步执行。
- 不做死抽象：公共层只在有明确复用点（至少两个使用方）时抽取，
  避免无意义的包装函数。
- Lua 状态指针统一命名为 `lua_state`，未使用时为 `_lua_state`；宏生成代码同样遵守。
- 文件系统（`cosmos.fs`）和时间/日期（`cosmos.time`）属于 builtin，随 Machine 自动安装，不拆为可选插件；其他生态模块按 `ROADMAP.md` 推进，业务策略保留在应用层。
- Lua 模块命名统一 `cosmos.network.*`、`cosmos.encoding.*`、`cosmos.encryption.*`、`cosmos.database.*`、`cosmos.fs`、`cosmos.time`、`cosmos.thread`、`cosmos.sync`；
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
