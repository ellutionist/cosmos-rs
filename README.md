# cosmos-rs

## 目录

- [特性概览](#特性概览)
- [快速开始](#快速开始)
- [异步运行模型](#异步运行模型)
- [内建模块](#内建模块)
  - [cosmos.time：时间与休眠](#cosmostime时间与休眠)
  - [cosmos.thread：协程管理](#cosmosthread协程管理)
  - [cosmos.sync：同步原语](#cosmossync同步原语)
  - [cosmos.null：可存储的空值](#cosmosnull可存储的空值)
- [网络插件](#网络插件)
  - [TCP：cosmos.network.tcp.stream](#tcpcosmosnetworktcpstream)
  - [UDP：cosmos.network.udp](#udpcosmosnetworkudp)
  - [TLS：cosmos.network.tcp.tls](#tlscosmosnetworktcptls)
  - [顺序协议 socket：cosmos.network.socket](#顺序协议-socketcosmosnetworksocket)
  - [HTTP 客户端：cosmos.network.http.client](#http-客户端cosmosnetworkhttpclient)
  - [HTTP 服务端：cosmos.network.http.server](#http-服务端cosmosnetworkhttpserver)
  - [WebSocket：cosmos.network.websocket](#websocketcosmosnetworkwebsocket)
- [编解码插件](#编解码插件)
  - [JSON：cosmos.encoding.json](#jsoncosmosencodingjson)
  - [TOML：cosmos.encoding.toml](#tomlcosmosencodingtoml)
  - [YAML：cosmos.encoding.yaml](#yamlcosmosencodingyaml)
- [文本插件](#文本插件)
- [图片插件](#图片插件)
- [加密插件](#加密插件)
- [数据库插件](#数据库插件)
  - [MySQL：cosmos.database.mysql](#mysqlcosmosdatabasemysql)
  - [Redis：cosmos.database.redis](#rediscosmosdatabaseredis)
- [嵌入 Rust 程序](#嵌入-rust-程序)
- [编写 Rust 插件](#编写-rust-插件)
- [CLI 说明](#cli-说明)
  - [模块搜索路径](#模块搜索路径)
  - [C 扩展](#c-扩展)
  - [插件开关](#插件开关)
- [目录结构](#目录结构)
- [测试与示例](#测试与示例)

cosmos-rs 是一个基于 LuaJIT 与 Tokio 的异步 Lua 运行环境。你可以把它当作独立脚本引擎运行网络服务，也可以把 `Machine` 嵌入 Rust 应用，为宿主提供可扩展的 Lua 脚本能力。

核心思想只有一条：**用同步写法表达异步流程**。Lua 代码里直接调用网络、计时器、同步等待接口，看起来是顺序阻塞调用，实际上等待时只挂起当前协程，同一进程里的其他协程继续运行。

```lua
local HttpServer = require("cosmos.network.http.server")

local srv = HttpServer.listen("0.0.0.0", 8080, function(req)
    -- 每个请求都是一个独立协程；这里 sleep 不会阻塞其他请求
    cosmos.time.sleep(200)
    return { status = 200, body = "hello from " .. req:remote_addr() }
end)

print("listening on port " .. srv:port())
cosmos.sync.notify.new():wait() -- 常驻：主协程等待，请求协程在后台运行
```

## 特性概览

- **同步写法，异步执行**：Lua 协程与 Rust `Future` 一一对应，由 Tokio 驱动；等待网络或计时器时挂起当前协程，其他协程交错运行。
- **单线程协作式调度**：每个 `Machine` 是一个独立 Lua VM 加 `LocalSet`，所有 Lua 协程固定在同一个宿主线程上，从不跨线程迁移——共享状态无需加锁，天然避免 Lua 状态竞争。
- **内建并发原语**：时间、协程（`spawn`/`join`/`wait`/`kill`），以及 Channel、Notify、Semaphore、Mutex 等同步原语。
- **插件化**：网络、编解码、文本、图片、加密、数据库（MySQL / Redis）都是可独立裁剪的 Rust 插件，通过 `add_module` 注入；CLI 默认仅启用网络，其他插件按需启用，`full` 启用全部插件。
- **方便嵌入**：每个 `Machine` 拥有独立 Lua 环境，复用宿主的 Tokio Runtime；多次执行脚本保留全局状态，宿主可注册自己的 userdata 类型。
- **Rust 侧零裸指针**：插件通过 `#[cosmos::userdata]` 宏注册类型，`#[derive(PushLua, FromLua)]` 组合完成 Lua 栈桥接，业务代码不接触任何 Lua C API。

## 快速开始

需要 Rust 1.89+；后续通用生态计划见 [ROADMAP.md](ROADMAP.md)。

默认配置需要 Rust 1.89+ / Cargo、C 编译器和 `make`（LuaJIT 在构建时自动编译）：

```sh
cargo build --release
```

构建产物为 `target/release/cosmos-cli`：

```sh
./target/release/cosmos-cli script.lua arg1 arg2
```

脚本路径之后的参数通过 Lua 的 `...` 传入；`-` 表示从标准输入读取脚本：

```sh
echo 'print("hi from stdin")' | ./target/release/cosmos-cli -
```

CLI 在脚本主协程结束后退出，常驻服务（服务器、事件循环）需要保持主协程运行，例如上面的 `notify:wait()` 无限等待。

## 异步运行模型

每个由 cosmos 管理的 Lua 协程都绑定一个 Rust `Future`。Lua 调用异步接口时，当前协程挂起并把控制权交回调度器；对应的 Future 在 Tokio 上等待 I/O 或计时器就绪，完成后把结果交回 Lua 并恢复执行。因此脚本可以用顺序代码表达并发流程：

```lua
local t1 = cosmos.thread.spawn(function()
    cosmos.time.sleep(200)
    return "a"
end)
local t2 = cosmos.thread.spawn(function()
    cosmos.time.sleep(100)
    return "b"
end)

local idx = cosmos.thread.wait(t1, t2) -- 返回先结束的下标：2
print(t2:join())                       -- b
```

调度是协作式的：同一 `Machine` 内的 Lua 协程通过 `LocalSet` 在固定线程上交错运行。即使宿主使用多线程 Tokio Runtime，Lua 协程也不会在线程之间迁移；普通 Rust 任务可以分布到工作线程，而 Lua 执行始终留在 `Machine` 所在线程。

一个完整的 TCP echo 服务，每个连接一个协程：

```lua
local Tcp = require("cosmos.network.tcp.stream")

local listener = assert(Tcp.listen(8080, 5000))
while true do
    local conn = assert(listener:accept())
    cosmos.thread.spawn(function()
        local reader, writer = conn:split() -- 读写分离，可一个协程读、另一个写
        while true do
            local line = assert(reader:receive("*l"))
            writer:write(line .. "\n")
        end
    end)
end
```

## 内建模块

内建模块随 `Machine` 创建自动加载，无需 `require` 也可通过 `cosmos.*` 访问。

### cosmos.fs：异步文件系统

`cosmos.fs` 提供文件、目录、元数据、原子写入和文件锁；`cosmos.fs.path` 提供路径操作。文件 I/O 在工作线程执行，同一共享句柄串行推进。完整接口及取消语义见 [FS 文档](cosmos/fs.md)，实际用法见 [fs.lua](examples/fs.lua)。

### cosmos.time：时间与休眠

```lua
cosmos.time.sleep(100)        -- 毫秒，支持小数（如 0.5）
print(cosmos.time.now_ms())   -- Unix 毫秒时间戳
print(cosmos.time.now_us())   -- Unix 微秒时间戳
```

`sleep` 挂起当前协程而非整个进程；`now_ms` / `now_us` 是真实系统时间，可用作日志时间戳。

`cosmos.time.instant/deadline` 提供单调计时与截止时间，`cosmos.time.datetime` 提供日期解析、格式化、UTC/固定偏移/IANA 时区转换。它们同样属于 builtin；用法见 [时间文档](cosmos/time.md) 和 [datetime.lua](examples/datetime.lua)。

### cosmos.thread：协程管理

```lua
local t = cosmos.thread.spawn(function(a, b)
    cosmos.time.sleep(10)
    return a + b
end, 1, 2)

print(t:join())               -- 3；出错时返回 nil, err
cosmos.thread.wait(t1, t2)    -- 等待任意一个结束，返回其参数下标
t:kill()                      -- 取消尚未结束的协程
```

### cosmos.sync：同步原语

有界 Channel、通知、信号量、互斥锁。所有等待接口的末参都是可选毫秒超时：缺省（或 nil）无限等待，`0` 表示只尝试立即完成。

```lua
local channel = cosmos.sync.channel.new(2) -- 容量 2
channel:send("v")
print(channel:recv(1000))     -- v；超时返回 nil, "timeout"

local notify = cosmos.sync.notify.new()
notify:notify_one()           -- 唤醒一个等待者；notify_all() 唤醒全部
notify:wait(5000)

local sem = cosmos.sync.semaphore.new(3)
local permit = assert(sem:acquire(1000))
permit:release()

local mutex = cosmos.sync.mutex.new()
local guard = assert(mutex:lock(1000))
guard:release()
```

### cosmos.null：可存储的空值

Lua 的 `nil` 不能保存在表和数组里，`cosmos.null` 用于表示可存储的空值。它在条件判断中为真，应通过 `value == cosmos.null` 判断：

```lua
local row = { field = cosmos.null }
assert(row.field ~= nil)          -- 字段存在
assert(row.field == cosmos.null)  -- 判断空值
```

## 网络插件

网络能力在 `cosmos-network` crate，Lua 命名空间 `cosmos.network.*`。

### TCP：cosmos.network.tcp.stream

`connect` 建立客户端连接，`listen` / `accept` 接受服务端连接。连接通过 `split()` 拆成 reader 和 writer，可以分别交给不同协程读写。

客户端：

```lua
local Tcp = require("cosmos.network.tcp.stream")

local conn = assert(Tcp.connect("127.0.0.1", 9001, 3000)) -- 第三参为连接超时（毫秒），可省略
conn:settimeout(1000)     -- 毫秒；<= 0 清除超时，超时不关闭连接
conn:setbufsize(65536)    -- 限制按行、分隔符或 EOF 读取的累积大小；<= 0 解除
local reader, writer = conn:split()

local payload = "hello\0tcp"
assert(writer:write(payload .. "\n"))    -- 返回发送字节数，保证整块写完
local data, err = reader:receive("*l")  -- 读一行，去掉行尾换行符
assert(data == payload, err)
writer:close()
reader:close()
```

服务端（先启动，等待客户端连接）：

```lua
local Tcp = require("cosmos.network.tcp.stream")

local server = assert(Tcp.listen(9001, 3000)) -- 监听 0.0.0.0:9001，第二参为 accept 超时
local conn = assert(server:accept())
conn:settimeout(1000)
local reader, writer = conn:split()

local data, err = reader:receive("*l")
assert(data, err)
assert(writer:write(data .. "\n")) -- 回显收到的内容
writer:close()
reader:close()
server:close()
```

`reader:receive()` 缺省按行读取；`receive(n)` 读精确 n 字节，`receive("*a")` 读到 EOF，`receive("*L")` 保留行尾换行符。`receiveuntil(delim)` 返回按分隔符读取的迭代器，每次返回分隔符前的内容并消费分隔符。

读取失败返回 `nil, err, partial`，常见错误为 `"closed"` / `"timeout"` / `"buffer overflow"`；`partial` 是本次已读取的内容。可分别运行[服务端](examples/tcp_server.lua)和[客户端](examples/tcp_client.lua)，传入相同端口；脚本还演示多连接并发与二进制回显。

### UDP：cosmos.network.udp

支持 IPv4 / IPv6、二进制和空报文。`bind` 用于接收任意来源的报文，`connect` 设置默认对端，之后用 `send` / `receive` 收发：

客户端：

```lua
local Udp = require("cosmos.network.udp")

local client = assert(Udp.connect("127.0.0.1", 9000, 3000)) -- 自动绑定本地端口，设置默认对端
client:settimeout(1000)             -- 后续收发超时；<= 0 清除
assert(client:send("hello\0udp"))   -- 一次发送一份报文，返回字节数
local data, err = client:receive()  -- 一次接收一份报文，可传 max_size 限制大小
assert(data == "hello\0udp", err)
client:close()
```

服务端（先启动，等待客户端发包）：

```lua
local Udp = require("cosmos.network.udp")

local server = assert(Udp.bind("127.0.0.1", 9000, 3000)) -- 绑定本地端口，第三参为初始超时
local data, host, port = server:receivefrom() -- 报文内容、来源 IP、来源端口
assert(data, host)                          -- 失败时返回 nil, 错误串
assert(server:sendto(data, host, port))      -- 向来源地址回包
server:close()
```

失败返回 `nil, 错误串`，超时为 `"timeout"`、关闭为 `"closed"`。收发上限均为 65507 字节；接收超过 `max_size` 的报文时返回 `nil, "buffer overflow"`，丢弃该报文，下一份仍可继续接收。空报文返回 `""`，不会被当作关闭。

同一句柄可跨协程收发，超时或取消等待后仍能继续使用；`close()` 唤醒所有等待者。`localaddr()` / `peeraddr()` 查询本地与默认对端地址，`setbroadcast(true)` 开启广播发送。可分别运行[服务端](examples/udp_server.lua)和[客户端](examples/udp_client.lua)，传入相同端口；报文边界、并发与取消用法见 [udp.lua](examples/udp.lua)。

### TLS：cosmos.network.tcp.tls

基于 rustls（ring 后端）。配置集中在 `TlsContext`：信任根、`verify_none`、客户端证书、ALPN 都在这里设置并缓存构建结果：

```lua
local Tls = require("cosmos.network.tcp.tls")
local TlsContext = require("cosmos.network.tcp.tls.context")

-- 客户端：ctx 可选，缺省用 webpki-roots 公有根 + 校验开启
local conn = assert(Tls.connect("example.com", 443, 3000))        -- (host, port, 超时?, ctx?)
local conn2 = assert(Tls.connect("example.com", 443, 3000, ctx)) -- 自建 ctx（位置参数）

-- 服务端
local sctx = TlsContext.new_server()
sctx:use_cert_chain_file("cert.pem")   -- 证书链（PEM 文件）
sctx:use_private_key_file("key.pem")   -- 私钥
sctx:add_alpn("h2")
local listener = assert(Tls.listen(8443, sctx, 3000)) -- 位置参数：ctx, 超时
```

连接成功后与 TCP 完全同构：`split()`、`receive` 状态机、`settimeout`、`setbufsize`、`receiveuntil` 全部复用；`close()` 会发送 `close_notify`。

### 顺序协议 socket：cosmos.network.socket

在连接建立后按需切换协议的通用 socket，支持连接内 TLS 升级（STARTTLS）与 Unix domain socket：

```lua
local Socket = require("cosmos.network.socket")

local s = Socket.new()
s:connect("127.0.0.1", 25)   -- 面向行的协议：先明文握手
print(s:receive("*l"))
s:send("EHLO x\r\n")
s:starttls()                 -- 连接内升级 TLS，之后收发都走加密通道
s:send("QUIT\r\n")
s:close()
```

### HTTP 客户端：cosmos.network.http.client

客户端与服务端支持流式收发、文件源、背压、单 Range 和条件缓存；用法见 [HTTP 文档](cosmos-network/http.md)，完整运行示例见 [http_stream.lua](examples/http_stream.lua)。

hyper + hyper-util 连接池，外挂重定向与整体超时。**每个 client 独立持有连接池**，克隆句柄和并发请求共享该实例的池：

```lua
local HttpClient = require("cosmos.network.http.client")

local client = HttpClient.new() -- 默认 webpki 公有根 + ALPN h2/http1.1

local resp = assert(client:request("GET", "https://example.com/data.json", {
    headers = { ["x-token"] = "..." },
    timeout  = 3000,   -- 毫秒：连接 + 重定向 + 读完 body 的总上限
    redirects = 5,     -- 最多跟随几次重定向，0 关闭
    max_body = 1048576 -- 可选：响应体上限，超过返回错误
}))
print(resp.status, resp.version, resp.headers["content-type"], resp.body)
```

TLS 配置在创建时固定，由该 client 的全部请求与重定向共用：

```lua
local cctx = TlsContext.new()                 -- 客户端配置：webpki 公有根
cctx:add_ca_file("internal-ca.pem")           -- 追加私有 CA 到信任根
cctx:use_cert_chain_file("client.pem")        -- 客户端证书（mTLS）
cctx:use_private_key_file("client-key.pem")

local tls_client = HttpClient.new({ ctx = cctx })
```

失败返回 `nil, 错误串`。v1 不处理 gzip（固定发送 `Accept-Encoding: identity`）。

### HTTP 服务端：cosmos.network.http.server

hyper auto builder（h1 keep-alive + h2 多路复用），每个请求跑一个独立 Lua 协程：

```lua
local HttpServer = require("cosmos.network.http.server")

local srv = assert(HttpServer.listen("0.0.0.0", 8080, function(req)
    -- req:method() / req:path() / req:target() / req:version()
    -- req:headers() / req:header(name) / req:body() / req:remote_addr()
    if req:path() == "/health" then
        return { status = 200, body = "ok" }
    end
    return {
        status = 200,
        headers = { ["content-type"] = "text/plain" },
        body = req:method() .. " " .. req:path(),
    }
end, {
    tls = server_ctx,          -- 可选：TLS 服务端上下文
    max_body = 8 * 1024 * 1024,
    ws_max_message = 4 * 1024 * 1024,
}))

print("listening on port " .. srv:port())
cosmos.sync.notify.new():wait() -- 常驻：主协程等待；srv:close() 停止服务
```

handler 内可以调用任何异步接口（sleep、向其他服务发请求等），不会阻塞其他请求。

### WebSocket：cosmos.network.websocket

RFC 6455。服务端不单独监听端口——HTTP handler 对选定 path 返回 `{ websocket = function(conn, req) ... end }`，由 HTTP 层完成 101 升级：

```lua
local HttpServer = require("cosmos.network.http.server")

local srv = HttpServer.listen("0.0.0.0", 8080, function(req)
    if req:path() == "/ws" then
        return { websocket = function(conn, req)
            while true do
                local op, data = conn:receive()
                -- op: "text" / "binary"；关闭: nil,"closed"；超时: nil,"timeout"
                if not op then break end
                if op == "text" then
                    conn:send("echo: " .. data)
                else
                    conn:send_binary(data)
                end
            end
        end }
    end
    return { status = 404, body = "not found" }
end)
cosmos.sync.notify.new():wait() -- 常驻：主协程等待
```

客户端：

```lua
local Ws = require("cosmos.network.websocket")

local conn = assert(Ws.connect("ws://127.0.0.1:8080/ws", { timeout = 3000 }))
conn:send("hello")            -- 文本帧（要求合法 UTF-8）
conn:send_binary("\0\1\2")    -- 二进制帧
conn:ping("x")                -- fastwebsockets 自动回 pong
print(conn:receive())         -- "text", data
conn:close(1000, "bye")       -- 自动回 close 帧
print(conn:state())           -- 0 打开 / 1 已关闭
```

wss 使用 `ctx` 指定 TLS 上下文（缺省公有根，ALPN 强制 http/1.1）；`max_message` 限制单条消息上限，缺省 64 MiB。

HTTP(S)/SOCKS5/SOCKS5H 代理、认证与逐跳路由见 [代理文档](cosmos-network/proxy.md) 和 [proxy.lua](examples/proxy.lua)。

URL 解析、相对地址合并、组件修改与有序查询参数由 `cosmos.network.http.url` 提供，随 Network 启用；见 [URL 文档](cosmos-network/url.md) 与 [url.lua](examples/url.lua)。

## 编解码插件

构建 CLI 时通过 `--features encoding` 或 `--features full` 启用。`cosmos-encoding` 提供 `cosmos.encoding.*`，支持二进制编解码和 JSON、TOML、YAML 文本转换：

```lua
local hex    = require("cosmos.encoding.hex")
local base64 = require("cosmos.encoding.base64")
local uri    = require("cosmos.encoding.uri")

assert(hex.encode("a\0b\255") == "610062ff")
assert(hex.decode("FF00") == "\255\0")
assert(base64.encode("foo") == "Zm9v")
assert(base64.decode_urlsafe("-_8") == "\251\255")
assert(uri.encode("a b+c/\0") == "a%20b%2Bc%2F%00")
```

### JSON：cosmos.encoding.json

基于 sonic-rs，文本与字符串值使用 UTF-8。解码的 `null` 是 `cosmos.null`；空容器与精确数字可以显式指定：

```lua
local json = require("cosmos.encoding.json")
assert(json.null == cosmos.null)

local doc = assert(json.decode('{"list":[],"null":null,"text":"\228\184\173\230\150\135😀"}'))
assert(doc.null == cosmos.null)
print(json.encode(doc))              -- {"list":[],"null":null,"text":"中文😀"}

assert(json.encode({}) == '{}')
assert(json.encode(json.array()) == '[]')    -- 显式空数组
assert(json.encode(json.object()) == '{}')   -- 显式空对象

-- 默认仅将安全整数转为 Lua number；大整数保留数字文本
local exact = assert(json.number.new('1.2345678901234567890123456789'))
assert(json.encode({ exact }) == '[1.2345678901234567890123456789]')

-- lossless_numbers 保持数字原样文本
local precise = assert(json.decode('[1,1.2300,1e+03]', { lossless_numbers = true }))
```

编码失败（循环引用、函数值、非法 UTF-8 等）返回 `nil, 错误串`；解码非法输入同样返回 `nil, err`。读取元表标签和表字段都不会执行用户元方法。

### TOML：cosmos.encoding.toml

用于配置文档，支持嵌套表、数组、表数组与日期时间：

```lua
local toml = require("cosmos.encoding.toml")

local config = assert(toml.decode([=[
name = "cosmos"
ports = [8080, 8081]
updated = 2026-11-14T12:34:56+08:00
[database]
host = "localhost"
]=]))
config.database.port = 3306
print(config.updated:text())        -- 日期时间保留类型，再次编码仍为日期时间
print(assert(toml.encode(config)))
```

根值必须是字符串键表，TOML 不支持 `cosmos.null`。大整数以精确数字 userdata 返回，可用 `toml.number.new(text)` 构造；日期时间可用 `toml.datetime.new(text)` 构造。空数组与空表使用 `toml.array()` / `toml.object()`，失败返回 `nil, err`。完整示例见 [toml.lua](examples/toml.lua)。

### YAML：cosmos.encoding.yaml

支持配置映射、数组、空值、锚点与合并键：

```lua
local yaml = require("cosmos.encoding.yaml")

local config = assert(yaml.decode([[
name: cosmos
enabled: true
ports: [8080, 8081]
optional: null
]]))
assert(config.optional == cosmos.null)
config.workers = 4
print(assert(yaml.encode(config)))
```

每次解码一份文档，映射键必须是字符串；`yaml.array()` / `yaml.object()` 标记空容器。大整数保留为精确数字，`yes` / `off` 等保持字符串；重复键、不支持的标签、循环别名与超限输入返回 `nil, err`。完整用法见 [yaml.lua](examples/yaml.lua)，类型范围与限制见 [encoding 文档](cosmos-encoding/README.md)。

### XML：cosmos.encoding.xml

XML 提供独立文档/节点、命名空间和保留原文的编辑，见 [XML 文档](cosmos-encoding/xml.md) 与 [xml.lua](examples/xml.lua)。

## 文本插件

构建 CLI 时通过 `--features text` 或 `--features full` 启用。`cosmos-text` 提供编译型正则、捕获组与可挂起的替换回调、Unicode 规范化/大小写及 Rust OpenCC 繁简转换。用法见 [Text 文档](cosmos-text/README.md) 和 [text.lua](examples/text.lua)。名称清洗、匹配与规范化顺序由应用组织。

## 图片插件

构建 CLI 时通过 `--features image` 或 `--features full` 启用。`cosmos-image` 提供 `cosmos.image`，支持 PNG/JPEG/WebP、缩放裁剪及像素操作，耗时处理由 Rust 工作线程执行。用法见 [Image 文档](cosmos-image/README.md)、[image.lua](examples/image.lua) 和 [HTTP 组合示例](examples/image_http.lua)。文件读写复用 builtin FS，裁剪比例与检测规则由应用决定。

## 加密插件

构建 CLI 时通过 `--features encryption` 或 `--features full` 启用。`cosmos-encryption` 提供 `cosmos.encryption.*`，支持增量摘要、HMAC、Argon2 密码哈希、安全随机数及非对称算法。接口与资源边界见 [Encryption 文档](cosmos-encryption/README.md)，完整用法见 [crypto.lua](examples/crypto.lua)。摘要返回原始字节，需要文本时与编解码插件组合：

```lua
local encryption = require("cosmos.encryption")
local hex = require("cosmos.encoding.hex")

assert(hex.encode(encryption.sha1("abc")) == "a9993e364706816aba3e25717850c26c9cd0d89d")
assert(hex.encode(encryption.sha256("abc"))
    == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
assert(#encryption.sha512("abc") == 64)

-- Ed25519：32 字节种子派生签名密钥（RFC 8032）
local ed = require("cosmos.encryption.ed25519")
local key = assert(ed.from_seed(hex.decode("9d61b19deffd5a60ba844af492ec2cc4"
    .. "4449c5697b326919703bac031cae7f60")))
local sig = key:sign("message")
assert(ed.verify(key:public_key(), "message", sig))

-- RSA-OAEP 公钥加密（MySQL 认证等场景），摘要 sha1 / sha256
local rsa = require("cosmos.encryption.rsa")
local public = assert(rsa.new(pem_string))
assert(#public:encrypt("secret\0data", "sha256") == public:size())
```

## 数据库插件

构建 CLI 时通过 `--features database` 或 `--features full` 启用两种数据库，也可单选 `mysql` / `redis`。`cosmos-database` 组合网络、编解码和加密能力，协议实现随 crate 内嵌（不依赖外部数据库客户端库）。

### MySQL：cosmos.database.mysql

通过 `cosmos.network.socket` 实现协议：

```lua
local mysql = require("cosmos.database.mysql")

local opts = assert(mysql.parse_url("mysql://user:pass@127.0.0.1:3306/dbname"))
opts.timeout, opts.pool_size = 2000, 4

-- 单连接
local db = mysql.new()
assert(db:connect(opts))
print(assert(db:query("SELECT VERSION()"))[1].version)

-- 或连接池：mysql.client(opts)，租借连接用完 set_keepalive() 归还
local pool = mysql.client(opts)
local conn = assert(pool:connect())
local rows = assert(conn:query("SELECT id, name FROM users WHERE id = 1"))
for _, row in ipairs(rows) do print(row.id, row.name) end
conn:set_keepalive()
```

`query` 没有占位符绑定，字符串参数用 `quote_sql_str` 转成十六进制字面量（支持 NUL 和任意二进制）；SELECT 返回行数组，写入语句返回 `{ affected_rows = n, insert_id = ... }`，失败返回 `nil, err, errno, sqlstate`：

```lua
local name = "o'brien"
local rows = assert(db:query(
    "SELECT id FROM users WHERE name = " .. mysql.quote_sql_str(name)))

local result = assert(db:query("INSERT INTO t (value) VALUES ('x')"))
print(result.affected_rows, result.insert_id)
```

### Redis：cosmos.database.redis

```lua
local redis = require("cosmos.database.redis")

local opts = assert(redis.parse_url("redis://127.0.0.1:6379/0"))
opts.timeout, opts.pool_size = 2000, 2

local pool = redis.client(opts)   -- 连接池，支持并发
local conn = assert(pool:connect())

assert(conn:set("k", "v", "EX", 60) == "OK")
assert(conn:get("k") == "v")
print(conn:incr("counter"))
assert(conn:hmget("hash", { "a", "missing" })[2] == cosmos.null)

-- 流水线：批处理往返
assert(conn:init_pipeline())
conn:set("p", "1")
conn:get("p")
local replies = assert(conn:commit_pipeline())
-- replies = { "OK", "1" }

conn:hello(3)                     -- 切换 RESP3
conn:close()
```

不存在的键返回 `cosmos.null`；命令错误返回 `false, err`。

## 嵌入 Rust 程序

宿主添加 `cosmos` 与 `tokio` 依赖，在异步上下文中创建并执行 `Machine`。`Machine` 默认加载文件系统、时间与日期、协程和同步模块；插件通过 `add_module` 安装：

```rust
use cosmos::{Machine, Module};
use cosmos_network::Network;

struct Host;

#[cosmos::userdata(name = "example.host")]
impl Host {
    fn add(a: i64, b: i64) -> i64 {
        a + b
    }
}

struct HostModule;

impl Module for HostModule {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Host>();
        machine.load("host definitions", "host_ready = true")
    }
}

fn main() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut machine = Machine::new(runtime.handle().clone())?;
    machine.add_module(Network)?;
    machine.add_module(HostModule)?;

    runtime.block_on(async {
        machine
            .exec("assert(host_ready); answer = cosmos.example.host.add(20, 22)")
            .await?;
        machine
            .exec("cosmos.time.sleep(1); assert(answer == 42)")
            .await?;
        machine.exec_file("script.lua", &["host".to_string()]).await
    })?;
    drop(machine);
    Ok(())
}
```

要点：

- `exec` 执行源码，`exec_file` 执行文件并传参，`exec_with_args` 直接传参给代码块；多次执行共享全局状态，`Machine` 销毁时先取消后台任务再关闭 VM。
- 每个 `Machine` 是独立 Lua VM，复用宿主的 Tokio Runtime；宿主负责创建并驱动 Runtime，且需启用所用的 I/O / 时间驱动。
- `machine.prepend_path("./lua/?.lua;./lua/?/init.lua")?` 或 `machine.prepend_cpath("./plugins/?.so")?` 前置模块搜索模板，保留已有路径。
- 插件通过 `#[cosmos::userdata(name = "...")]` 宏声明类型路径，`machine.register::<T>()` 注册，`machine.load(name, source)` 加载 Lua 定义——业务代码不接触 Lua 裸指针。

完整示例见 [cosmos-network/examples/machine.rs](cosmos-network/examples/machine.rs)。

## 编写 Rust 插件

插件实现 `Module` trait 后注入 Machine。以计数器的完整实现为例：

```rust
use std::cell::RefCell;
use cosmos::{Machine, Module};

struct Counter(RefCell<i64>);

#[cosmos::userdata(name = "example.counter")]
impl Counter {
    fn new(start: i64) -> Self {
        Counter(RefCell::new(start))
    }

    fn add(&self, delta: i64) -> i64 {
        let mut value = self.0.borrow_mut();
        *value += delta;
        *value
    }
}

struct CounterModule;

impl Module for CounterModule {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Counter>();
        Ok(())
    }
}
```

方法可以是 `async fn`——此时调用方在结果就绪前挂起，由运行时统一桥接，插件内无需处理 Lua 协程调度。参数与返回值通过 `#[derive(FromLua, PushLua)]` 在结构体 / 枚举上声明式桥接：

```rust
use cosmos::userdata::Nil;

#[derive(cosmos::PushLua)]
pub enum ReceiveOutcome {
    Data(Vec<u8>),
    Closed(Nil, &'static str, Vec<u8>), // 与 Lua 三值协议一一对应
    Timeout(Nil, &'static str),
}
```

更多桥接细节见 [cosmos/src/userdata.rs](cosmos/src/userdata.rs) 与 [cosmos-macros](cosmos-macros)。

## CLI 说明

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

两个参数均写在脚本路径之前，可重复指定或用分号分隔。环境变量沿用 LuaJIT 的替换规则，`;;` 展开为原生默认路径。

### C 扩展

`require` 可加载兼容 Lua 5.1 / LuaJIT ABI 的 C 扩展。CLI 的 macOS / Linux 构建已配置 Lua C API 的保留与动态导出；LuaRocks 包须按 Lua 5.1 / LuaJIT 安装，并配置对应的搜索路径。

**嵌入 `Machine` 时，最终宿主需要自行配置 C API 导出。** LuaJIT 随 `cosmos` 静态链接；`cosmos/build.rs` 发出的 `rustc-link-arg` 不会传播到宿主，仅设置 `package.cpath` 或使用 `whole-archive` 仍可能报 `symbol not found`。宿主可参考 [CLI 的 build.rs](cosmos-cli/build.rs)，需保留并导出 `lua_*`、`luaL_*`、`luaopen_*`、`luaJIT_*`：macOS 使用 `-exported_symbol`，Linux 使用 `--export-dynamic-symbol`。

C 扩展的调用是同步的：阻塞操作会阻塞 `Machine` 所在线程，也不会自动接入 Tokio 异步调度。

### 插件开关

默认仅启用 `network`。`encoding`、`encryption`、`database`、`image`、`text` 按需启用，`full` 开启全部插件：`cargo build --release -p cosmos-cli --features full`。

| Feature | 插件 | 依赖 |
| --- | --- | --- |
| `network` | 网络（TCP / UDP / TLS / socket / HTTP / WebSocket） | — |
| `full` | 全部插件 | `network` + `encoding` + `encryption` + `database` + `image` + `text` |
| `text` | 文本（正则 / Unicode / 繁简转换；默认关闭） | — |
| `image` | 图片（PNG / JPEG / WebP、缩放裁剪、像素；默认关闭） | — |
| `encoding` | 编解码（hex / base64 / uri / json / toml / yaml） | — |
| `encryption` | 加密（摘要 / HMAC / 密码 / 随机数 / Ed25519 / RSA） | — |
| `database` | 数据库（MySQL + Redis；默认关闭） | `mysql` + `redis` |
| `mysql` | 仅 MySQL | `network` + `encoding` + `encryption` |
| `redis` | 仅 Redis | `network` |

关闭默认 feature 后可按需组合，例如仅要网络与 MySQL：

```sh
cargo build --release --no-default-features --features network,mysql
```

## 目录结构

```
cosmos/           核心运行时：Lua VM 管理、协程调度、桥接层
cosmos-macros/    过程宏：#[userdata]、#[derive(PushLua, FromLua)]
cosmos-network/   网络插件：TCP / TLS / socket / HTTP / WebSocket
cosmos-encoding/  编解码插件：hex / base64 / uri / json
cosmos-encryption/ 加密插件：摘要 / HMAC / 密码 / 随机数 / Ed25519 / RSA
cosmos-database/  数据库插件：MySQL / Redis
cosmos-text/      文本插件：正则 / Unicode / 繁简转换
cosmos-image/     图片插件：编解码 / 缩放裁剪 / 像素
cosmos-cli/       命令行入口
examples/         端到端 Lua 示例
```

## 测试与示例

```sh
cargo test --workspace
```

端到端示例（`examples/`）：涉及可选插件的示例需要启用对应 feature，或构建时使用 `--features full`。

| 示例 | 说明 |
| --- | --- |
| `sleep.lua` | 时间与休眠 |
| [fs.lua](examples/fs.lua) | builtin 异步文件、目录、原子操作与文件锁 |
| [datetime.lua](examples/datetime.lua) | builtin 日期、时区和单调计时 |
| `spawn.lua` | 协程 spawn / join / wait / kill |
| `sync.lua` | Channel / Notify / Semaphore / Mutex |
| `null.lua` | 可存储空值 |
| `tcp_client.lua` + `tcp_server.lua` | TCP echo（双进程） |
| `tls_client.lua` + `tls_server.lua` | TLS echo（双进程） |
| `network_socket.lua` | 顺序协议 socket 与 STARTTLS |
| [proxy.lua](examples/proxy.lua) | HTTP 代理、逐跳路由、流式与池隔离 |
| [url.lua](examples/url.lua) | URL 组件、查询参数与标准重定向 |
| [http_stream.lua](examples/http_stream.lua) | HTTP 流式上传/下载、文件范围、条件缓存与取消 |
| `http.lua` | HTTP 客户端 + 服务端（单进程，含重定向、超时、h2） |
| `websocket.lua` | WebSocket 客户端 + 服务端升级（单进程） |
| [websocket_concurrent.lua](examples/websocket_concurrent.lua) | 并发收发、取消与关闭 |
| `encoding.lua` / `json.lua` | 编解码与 JSON |
| `encryption.lua` | 哈希与签名 |
| [text.lua](examples/text.lua) | 正则、可挂起替换、Unicode 与繁简转换 |
| [image.lua](examples/image.lua) | 图片编解码、变换与像素读写 |
| [image_http.lua](examples/image_http.lua) | HTTP 图像下载、上传与处理组合 |
| [xml.lua](examples/xml.lua) | XML 文档、节点、命名空间与原文编辑 |
| [crypto.lua](examples/crypto.lua) | 增量文件摘要、HMAC、Argon2 密码和安全随机数 |
| `mysql.lua` / `redis.lua` | 数据库端到端（需本地服务） |

运行方式：

```sh
./target/release/cosmos-cli examples/http.lua
./target/release/cosmos-cli examples/websocket.lua
./target/release/cosmos-cli examples/tcp_server.lua 19001 &
./target/release/cosmos-cli examples/tcp_client.lua 19001
```
