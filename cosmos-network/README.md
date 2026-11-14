# cosmos-network

cosmos-rs 的网络插件，为 Lua 提供 TCP、UDP、TLS、HTTP 和 WebSocket。网络等待会挂起当前 Lua 协程，接口可以直接按顺序调用。

## 功能

- **TCP**：连接、监听、读写分离、按长度或分隔符读取，以及操作超时。
- **UDP**：IPv4 / IPv6 报文收发、来源地址、广播开关，以及可取消的超时等待。
- **TLS**：TLS 连接与监听、自定义证书、双向认证和服务端证书热重载。
- **顺序协议 socket**：统一收发、TCP / Unix domain socket、同一连接内升级 TLS；收发失败或取消时关闭连接。
- **HTTP**：HTTP / HTTPS 客户端与服务端，支持 HTTP/1.1、HTTP/2；客户端提供实例独立的连接池、重定向与请求超时；双向流式 body、文件响应、单 Range 与条件缓存见 [HTTP 用法](http.md)。
- **URL**：标准解析与相对地址合并、不可变组件修改、有序重复查询参数；HTTP 重定向复用解析规则，见 [URL 用法](url.md)。
- **WebSocket**：支持 ws / wss，文本与二进制消息、ping / pong、同一连接并发收发；服务端通过 HTTP handler 升级连接。

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

完整用法见 [Rust 嵌入示例](examples/machine.rs)，CLI 和 Machine 的基础使用见[项目 README](../README.md)。TCP 的独立客户端与服务端写法见 [TCP 用法](../README.md#tcpcosmosnetworktcpstream)。

## Lua 模块与示例

| 模块 | 示例 |
| --- | --- |
| `cosmos.network.http.url` | [URL 解析、修改、查询与真实 HTTP 重定向](../examples/url.lua) |
| `cosmos.network.socket` | [明文握手、TLS 升级、Unix、超时与取消](../examples/network_socket.lua) |
| `cosmos.network.tcp.stream` | [TCP 服务端](../examples/tcp_server.lua)、[客户端](../examples/tcp_client.lua) |
| `cosmos.network.udp` | [UDP 服务端](../examples/udp_server.lua)、[客户端](../examples/udp_client.lua)、[报文边界、并发与取消](../examples/udp.lua) |
| `cosmos.network.tcp.tls`、`cosmos.network.tcp.tls.context` | [TLS 服务端](../examples/tls_server.lua)、[客户端](../examples/tls_client.lua) |
| `cosmos.network.http.client`、`cosmos.network.http.server` | [HTTP / HTTPS 与 HTTP/2](../examples/http.lua) |
| `cosmos.network.http.body`、`cosmos.network.http.file` | [流式上传/下载、文件范围、背压与取消](../examples/http_stream.lua) |
| `cosmos.network.websocket` | [WebSocket 客户端与服务端](../examples/websocket.lua)、[并发、取消与关闭](../examples/websocket_concurrent.lua) |

示例从项目根目录用 `cosmos-cli` 运行。TCP、UDP、TLS 双进程用例需先启动服务端，再启动客户端，并传入相同端口；HTTP、WebSocket 用例会自行启动本地服务。`http_stream.lua` 需传入一个已存在的临时父目录。TLS 示例使用 [examples/certs](../examples/certs/) 中的测试证书。

`cosmos.network.socket.new()` 创建连接句柄，`settimeout(ms)` 同时作用于连接、收发和 TLS 升级。TCP 使用 `connect(host, port)`，Unix 使用 `connect_unix(path)`；`send` 返回字节数，`receive` 沿用原有读取模式，`close` 成功返回 `1`，`abort()` 立即丢弃底层流。同一句柄上的异步操作独占连接，超时、读取错误或取消后需重新连接。发送支持任意二进制，跨异步等待由 `LuaBytes` 复制或锚定，不进行 UTF-8 转换。

`starttls(server_name, ctx)` 在当前流上完成客户端升级，TCP 可省略名称以使用连接主机；`starttls_server(ctx)` 完成服务端升级。两者均保留预读字节，握手失败或取消后关闭底层流。`listen(host, port, timeout_ms?)` / `listen_unix(path, timeout_ms?)` 返回监听器，超时作用于每次 `accept()`；Unix 监听路径由调用者创建和清理。

## UDP

`cosmos.network.udp` 随 Network 插件安装。`bind(host, port, timeout_ms?)` 绑定本地地址，端口 `0` 由系统分配；使用 `receivefrom()` 接收 `data, host, port`，再用 `sendto(data, host, port)` 向指定地址发送。`connect(host, port, timeout_ms?)` 自动绑定并设置默认对端，使用 `send(data)` / `receive()` 收发，只接收该对端的报文；UDP connect 不做握手，也不代表远端可达。

每次调用处理一份完整报文，支持 IPv4 / IPv6、二进制和空报文。发送返回字节数，接收返回原始字符串，空报文为 `""`。发送上限及缺省接收上限均为 65507 字节；`receive(max_size?)` / `receivefrom(max_size?)` 可指定更小上限，超限返回 `nil, "buffer overflow"` 并丢弃当前报文，下一份仍可继续读取。

`settimeout(ms)` 设置之后开始的操作的超时，`<= 0` 清除；构造时传入的超时也作为收发的初始配置，缺省不超时。失败返回 `nil, err`，超时为 `"timeout"`、关闭为 `"closed"`。同一句柄可跨协程收发，超时或取消等待不关闭 socket；`close()` 幂等，并唤醒所有等待者。`localaddr()` / `peeraddr()` 返回本地与默认对端的 `host, port`，`state()` 为 `0` 开启、`1` 关闭，`setbroadcast(enabled)` / `broadcast()` 设置与读取广播开关。

收发示例见[项目 README](../README.md#udpcosmosnetworkudp)，独立脚本见[服务端](../examples/udp_server.lua)和[客户端](../examples/udp_client.lua)。[udp.lua](../examples/udp.lua) 覆盖报文边界、并发与取消，传入 `::1` 可运行 IPv6 回归。

## WebSocket 并发与生命周期

同一连接可由一个协程持续 `receive()`，其他协程同时 `send()` / `send_binary()`；写入按连接串行化，多个接收者依次消费完整消息。后台收帧队列最多缓存 8 条消息，满时反压读取。`state()` 在接收等待期间仍为 `0`，对端关闭或错误后为 `1`。

`settimeout(ms)` 只作用于之后开始的操作。接收超时或取消不会丢弃半帧，也不会关闭连接；写入开始后超时或取消会关闭连接，避免复用不完整的帧。`close()` 幂等，结束连接并唤醒等待者；无操作超时时，close 帧最多等待 5 秒。服务端 handler 结束或取消后关闭连接，最后一个句柄释放时取消后台读任务。
