# cosmos-network

cosmos-rs 的网络插件，为 Lua 提供 TCP、TLS、HTTP 和 WebSocket。网络等待会挂起当前 Lua 协程，接口可以直接按顺序调用。

## 功能

- **TCP**：连接、监听、读写分离、按长度或分隔符读取，以及操作超时。
- **TLS**：TLS 连接与监听、自定义证书、双向认证和服务端证书热重载。
- **顺序协议 socket**：统一收发、TCP / Unix domain socket、同一连接内升级 TLS；收发失败或取消时关闭连接。
- **HTTP**：HTTP / HTTPS 客户端与服务端，支持 HTTP/1.1、HTTP/2；客户端提供实例独立的连接池、重定向与请求超时。
- **WebSocket**：支持 ws / wss，文本与二进制消息、ping / pong；服务端通过 HTTP handler 升级连接。

## 启用

CLI 的 `network` feature 默认启用，构建后即可在 Lua 中 `require("cosmos.network.*")`。仅需内建模块时可关闭，或单独显式启用：

```sh
cargo build --release -p cosmos-cli --no-default-features
cargo build --release -p cosmos-cli --no-default-features --features network
```

Rust 宿主添加 `cosmos-network` 依赖后，将插件注入已有的 `Machine`：

```rust
machine.add_module(cosmos_network::Network)?;
```

完整用法见 [Rust 嵌入示例](examples/machine.rs)，CLI 和 Machine 的基础使用见[项目 README](../README.md)。

## Lua 模块与示例

| 模块 | 示例 |
| --- | --- |
| `cosmos.network.socket` | [明文握手、TLS 升级、Unix、超时与取消](../examples/network_socket.lua) |
| `cosmos.network.tcp.stream` | [TCP 服务端](../examples/tcp_server.lua)、[客户端](../examples/tcp_client.lua) |
| `cosmos.network.tcp.tls`、`cosmos.network.tcp.tls.context` | [TLS 服务端](../examples/tls_server.lua)、[客户端](../examples/tls_client.lua) |
| `cosmos.network.http.client`、`cosmos.network.http.server` | [HTTP / HTTPS 与 HTTP/2](../examples/http.lua) |
| `cosmos.network.websocket` | [WebSocket 客户端与服务端](../examples/websocket.lua) |

示例从项目根目录用 `cosmos-cli` 运行。TCP、TLS 用例需分别启动服务端和客户端，并传入相同端口；HTTP、WebSocket 用例会自行启动本地服务。TLS 示例使用 [examples/certs](../examples/certs/) 中的测试证书。

`cosmos.network.socket.new()` 创建连接句柄，`settimeout(ms)` 同时作用于连接、收发和 TLS 升级。TCP 使用 `connect(host, port)`，Unix 使用 `connect_unix(path)`；`send` 返回字节数，`receive` 沿用原有读取模式，`close` 成功返回 `1`，`abort()` 立即丢弃底层流。同一句柄上的异步操作独占连接，超时、读取错误或取消后需重新连接。发送支持任意二进制，跨异步等待由 `LuaBytes` 复制或锚定，不进行 UTF-8 转换。

`starttls(server_name, ctx)` 在当前流上完成客户端升级，TCP 可省略名称以使用连接主机；`starttls_server(ctx)` 完成服务端升级。两者均保留预读字节，握手失败或取消后关闭底层流。`listen(host, port, timeout_ms?)` / `listen_unix(path, timeout_ms?)` 返回监听器，超时作用于每次 `accept()`；Unix 监听路径由调用者创建和清理。
