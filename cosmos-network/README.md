# cosmos-network

cosmos-rs 的网络插件，为 Lua 提供 TCP、TLS、HTTP 和 WebSocket。网络等待会挂起当前 Lua 协程，接口可以直接按顺序调用。

## 功能

- **TCP**：连接、监听、读写分离、按长度或分隔符读取，以及操作超时。
- **TLS**：TLS 连接与监听、自定义证书、双向认证和服务端证书热重载。
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
| `cosmos.network.tcp.stream` | [TCP 服务端](../examples/tcp_server.lua)、[客户端](../examples/tcp_client.lua) |
| `cosmos.network.tcp.tls`、`cosmos.network.tcp.tls.context` | [TLS 服务端](../examples/tls_server.lua)、[客户端](../examples/tls_client.lua) |
| `cosmos.network.http.client`、`cosmos.network.http.server` | [HTTP / HTTPS 与 HTTP/2](../examples/http.lua) |
| `cosmos.network.websocket` | [WebSocket 客户端与服务端](../examples/websocket.lua) |

示例从项目根目录用 `cosmos-cli` 运行。TCP、TLS 用例需分别启动服务端和客户端，并传入相同端口；HTTP、WebSocket 用例会自行启动本地服务。TLS 示例使用 [examples/certs](../examples/certs/) 中的测试证书。
