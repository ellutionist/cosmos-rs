# cosmos-rs

cosmos-rs 是一个基于 LuaJIT 和 Tokio 的异步 Lua 运行环境，用于编写脚本、网络服务，或为 Rust 程序提供可扩展的 Lua 脚本能力。可以通过 CLI 独立使用，也可以通过 `Machine` 嵌入现有应用。

## 特点

- **同步写法，异步执行**：Lua 中直接调用异步接口，等待时挂起当前协程，其他协程继续运行。
- **内建并发工具**：提供时间、协程，以及 Channel、Notify、Semaphore、Mutex 等同步原语。
- **方便嵌入**：每个 `Machine` 拥有独立的 Lua 环境，复用宿主的 Tokio Runtime，支持多次执行脚本并保留状态。
- **插件扩展**：Rust 插件实现 `Module` 后通过 API 注入，可以向 Lua 暴露类型与方法；[网络插件](cosmos-network/README.md) 默认随 CLI 启用。

## 异步运行模型

每个由 cosmos 管理的 Lua 协程都绑定一个 Rust 异步协程（`Future`），由 Tokio 驱动。Lua 调用网络、计时器或同步等待接口时会挂起；对应的 Future 等待操作就绪，再将结果交回 Lua 并恢复执行。因此，脚本可以用顺序代码表达异步流程。

调度采用协作式模型：同一 `Machine` 内的 Lua 协程通过 `LocalSet` 在固定的宿主线程上交错运行，一个协程等待异步操作时，其他协程可以继续执行。即使宿主使用多线程 Tokio Runtime，Lua 协程也不会在线程之间迁移。

## 使用 CLI

需要 Rust / Cargo、C 编译器和 `make`。在项目根目录构建，LuaJIT 会自动编译：

```sh
cargo build --release -p cosmos-cli
```

构建后的程序位于 `target/release/cosmos-cli`：

```sh
./target/release/cosmos-cli script.lua arg1 arg2
./target/release/cosmos-cli -
./target/release/cosmos-cli --help
```

脚本路径后的参数通过 Lua 的 `...` 传入；`-` 表示从标准输入读取脚本。CLI 在脚本主协程结束后退出，常驻服务需要保持主协程运行。

内建模块的用法见 [时间](examples/sleep.lua)、[协程](examples/spawn.lua)和[同步原语](examples/sync.lua)示例。

### 模块搜索路径

默认沿用 LuaJIT 的原生搜索路径。Lua 文件与 C 扩展分别配置：

| 模块 | 命令行参数 | 环境变量 | 搜索模板示例 |
| --- | --- | --- | --- |
| Lua 文件 | `--path` | `LUA_PATH` | `./lua/?.lua;./lua/?/init.lua` |
| C 扩展 | `--cpath` | `LUA_CPATH` | `./lib/?.so` |

```sh
LUA_PATH='./lua/?.lua;;' LUA_CPATH='./lib/?.so;;' ./target/release/cosmos-cli script.lua
./target/release/cosmos-cli --path './lua/?.lua;./lua/?/init.lua' --cpath './lib/?.so' script.lua
```

两个参数均写在脚本路径之前，可重复指定或用分号分隔，按参数顺序优先于现有路径搜索。环境变量沿用 LuaJIT 的替换规则，`;;` 会展开为原生默认路径。Unix 默认 C 路径包括 `./?.so`、`/usr/local/lib/lua/5.1/?.so` 和 `/usr/local/lib/lua/5.1/loadall.so`。

### C 扩展

`require` 可加载兼容 Lua 5.1 / LuaJIT ABI 的 C 扩展。CLI 的 macOS / Linux 构建已配置 Lua C API 的保留与动态导出，LuaRocks 包须按 Lua 5.1 / LuaJIT 安装，并配置对应的搜索路径。

**嵌入 Machine 时，最终宿主需要自行配置 C API 导出。** LuaJIT 随 `cosmos` 静态链接；`cosmos/build.rs` 发出的 `rustc-link-arg` 不会传播到宿主，仅设置 `package.cpath` 或使用 `whole-archive` 仍可能报 `symbol not found`。

宿主可参考 [CLI 的 build.rs](cosmos-cli/build.rs)，将 `rustc-link-arg-bin` 中的二进制名改为自己的宿主名，并与已有导出配置合并。需保留并导出 `lua_*`、`luaL_*`、`luaopen_*`、`luaJIT_*`：macOS 使用 `-exported_symbol`，Linux 使用 `--export-dynamic-symbol`。脚本中的 C 扩展调用仍同步执行，阻塞操作会阻塞 Machine 所在线程，也不会自动接入 Tokio 异步调度。

## 使用 Machine

Rust 宿主添加 `cosmos` 和 `tokio` 依赖，在 Tokio Runtime 的异步上下文中创建并执行 `Machine`：

```rust
use cosmos::Machine;

async fn run_script() -> Result<(), String> {
    let mut machine = Machine::new(tokio::runtime::Handle::current())?;
    machine.exec_file("script.lua", &[]).await
}
```

`exec` 执行源码，`exec_file` 执行文件并传入参数。`Machine` 默认加载时间、协程和同步模块；其他插件通过 `add_module` 安装。

宿主通过 `machine.prepend_path("./lua/?.lua;./lua/?/init.lua")?` 或 `machine.prepend_cpath("./plugins/?.so")?` 前置搜索模板，保留已有路径；`LUA_PATH`、`LUA_CPATH` 在创建 `Machine` 时生效。

宿主负责 Runtime 的创建与驱动，并启用所需的时间和 I/O 驱动。`Machine` 只持有 Runtime 的 `Handle`；Runtime 可以是多线程的，Lua 执行仍保持在同一宿主线程。

完整的 Runtime 创建、插件注入和脚本调用见 [Rust 嵌入示例](cosmos-network/examples/machine.rs)。
