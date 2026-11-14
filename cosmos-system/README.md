# cosmos-system

显式信号订阅与进程信息插件。CLI 默认关闭，通过 system 或 full feature 启用；Rust 宿主使用 `machine.add_module(cosmos_system::System)?`。不依赖 Network，不接管 Runtime、进程退出或业务停止顺序。

| 模块/接口 | 说明 |
| --- | --- |
| `cosmos.system.process.pid()` | 宿主进程 ID |
| `cosmos.system.process.executable()` | 当前可执行文件路径字节串；Unix 保留非 UTF-8 路径 |
| `cosmos.system.signal.subscribe(name)` | 显式建立订阅，失败返回 nil, err |
| `subscription:recv(timeout_ms?)` | 接收信号并返回名称；超时/关闭返回 nil, timeout/closed |
| `subscription:close()` / `:is_closed()` | 关闭订阅、唤醒等待者 / 查询状态 |

Unix 支持 interrupt/terminate/hangup/quit/user1/user2，Windows 支持 interrupt/break。多个等待者串行消费同一订阅；不同订阅各自接收 Tokio 的事件通知。OS 信号可能合并，不保证逐次计数。接收超时/取消保留订阅，close 会关闭它；所有 Lua 等待仍固定在 Machine 的 LocalSet，Machine 销毁取消等待并释放对象。

安装插件不订阅信号。首次显式订阅时 Tokio 会改变对应进程信号的处理方式；即使关闭订阅，Unix 的默认动作也不会自动恢复，因此由宿主/应用决定后续动作，见 [Tokio Caveats](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html#caveats)。不全局安装 tracing，不调用 exit，不主动发送信号。

完整使用与超时/关闭示例见 [system.lua](../examples/system.lua)。HTTP 停止接收和排空见 [HTTP 生命周期](../cosmos-network/http.md) 与 [http_shutdown.lua](../examples/http_shutdown.lua)：应用可以先停止后台任务，再 shutdown 服务端、关闭数据库、刷新日志，顺序由应用决定。子进程管理仍待明确的通用使用需求，不扩展为业务服务框架。
