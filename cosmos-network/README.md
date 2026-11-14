# cosmos-network

cosmos-rs 的网络插件，为 Lua 提供 TCP、UDP、TLS、HTTP 和 WebSocket。网络等待会挂起当前 Lua 协程，接口可以直接按顺序调用。

## 功能

- **TCP**：连接、监听、读写分离、按长度或分隔符读取，以及操作超时。
- **UDP**：IPv4 / IPv6 报文收发、来源地址、广播开关，以及可取消的超时等待。
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
| `cosmos.network.udp` | [UDP 收发、报文边界、并发与取消](../examples/udp.lua) |
| `cosmos.network.tcp.tls`、`cosmos.network.tcp.tls.context` | [TLS 服务端](../examples/tls_server.lua)、[客户端](../examples/tls_client.lua) |
| `cosmos.network.http.client`、`cosmos.network.http.server` | [HTTP / HTTPS 与 HTTP/2](../examples/http.lua) |
| `cosmos.network.websocket` | [WebSocket 客户端与服务端](../examples/websocket.lua) |

示例从项目根目录用 `cosmos-cli` 运行。TCP、TLS 用例需分别启动服务端和客户端，并传入相同端口；HTTP、WebSocket 用例会自行启动本地服务。TLS 示例使用 [examples/certs](../examples/certs/) 中的测试证书。

`cosmos.network.socket.new()` 创建连接句柄，`settimeout(ms)` 同时作用于连接、收发和 TLS 升级。TCP 使用 `connect(host, port)`，Unix 使用 `connect_unix(path)`；`send` 返回字节数，`receive` 沿用原有读取模式，`close` 成功返回 `1`，`abort()` 立即丢弃底层流。同一句柄上的异步操作独占连接，超时、读取错误或取消后需重新连接。发送支持任意二进制，跨异步等待由 `LuaBytes` 复制或锚定，不进行 UTF-8 转换。

`starttls(server_name, ctx)` 在当前流上完成客户端升级，TCP 可省略名称以使用连接主机；`starttls_server(ctx)` 完成服务端升级。两者均保留预读字节，握手失败或取消后关闭底层流。`listen(host, port, timeout_ms?)` / `listen_unix(path, timeout_ms?)` 返回监听器，超时作用于每次 `accept()`；Unix 监听路径由调用者创建和清理。

## UDP

使用 `require("cosmos.network.udp")`，随 Network 插件安装，不需要额外 feature。静态构造函数与 socket 方法在同一类型表上。

| 接口 | 返回值与用途 |
| --- | --- |
| `Udp.bind(host, port[, timeout_ms])` | 绑定本地地址，返回未连接 socket；端口 `0` 由系统分配 |
| `Udp.connect(host, port[, timeout_ms])` | 自动绑定本地地址并设置默认目标，返回已连接 socket |
| `socket:send(data)` | 向默认目标发送一份报文，返回字节数；用于已连接 socket |
| `socket:sendto(data, host, port)` | 向指定目标发送一份报文，返回字节数；用于未连接 socket |
| `socket:receive([max_size])` | 接收一份报文，返回原始字节字符串 |
| `socket:receivefrom([max_size])` | 接收一份报文，返回 `data, host, port` |
| `socket:localaddr()` / `socket:peeraddr()` | 返回本地 / 默认对端的 `host, port`；未连接时 peeraddr 返回错误 |
| `socket:settimeout(ms)` | 设置之后开始的操作的超时，毫秒，`<= 0` 清除；成功无返回值 |
| `socket:setbroadcast(enabled)` / `socket:broadcast()` | 设置广播发送开关 / 读取开关；设置成功返回 true |
| `socket:close()` / `socket:state()` | 幂等关闭，无返回值；state 为 `0` 开启、`1` 关闭 |

操作失败返回 `nil, err`，超时为 `"timeout"`、关闭为 `"closed"`、接收超限为 `"buffer overflow"`。空报文成功返回 `""`，发送空报文成功返回 `0`；每次调用只处理一份报文，不拆分或拼接。单次发送和接收上限均为 65507 字节，`max_size` 缺省也是 65507；实际发送还受操作系统与链路限制。超过接收上限的报文整份丢弃并返回错误，不返回截断内容，下一份报文仍可继续读取。

构造函数的超时缺省不设上限，传入时覆盖域名解析及地址初始化，同时作为后续收发的初始超时；`sendto` 的域名解析与发送共用一个超时。超时和取消当前等待都不关闭 socket。同一句柄可跨协程并发收发，多个接收者会竞争报文，分配顺序不保证；`close()` 唤醒全部挂起操作并返回 closed，等待者退出后释放其持有的底层引用。最后一个句柄和在途操作释放时自动回收 socket。

`connect` 不执行握手，不代表远端可达；它设置默认目标，并由内核过滤其他来源。域名解析后选择首个可初始化的地址，`sendto` 选择与本地 socket 地址族匹配的目标。UDP 本身不保证送达、顺序或去重，需要可靠性时由应用协议处理。IPv6 字面地址无需方括号，返回的 IPv6 来源保留 scope ID。完整用法见 [udp.lua](../examples/udp.lua)，传入 `::1` 可运行 IPv6 回归。
