# 代理

`cosmos.network.proxy` 随 Network 安装，支持 HTTP、HTTPS、SOCKS5 和 SOCKS5H。完整用法见 [proxy.lua](../examples/proxy.lua)。模块不读取 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY，也不内置私网直连规则；路由由 Lua 应用显式决定。

`Proxy.new(url[, {ctx=proxy_tls_context}])` 返回不可变代理配置；非法配置返回 `nil, err`。HTTP(S) 默认端口为 80/443，SOCKS 默认 1080；URL 不允许额外路径、query 或 fragment。用户名、密码写在 URL userinfo，按百分号解码；HTTP 使用预发送 Basic，SOCKS 使用 RFC 1929 用户名/密码认证。HTTP 用户名不能包含冒号；SOCKS 两项各限 1..255 字节。不会在错误或接口输出中返回凭证，仅提供 `:scheme()`、`:host()`、`:port()`。

## HTTP client

创建 client 时传 `{proxy=proxy}` 固定代理，或 `{proxy=function(url) ... end}` 逐跳选择。回调接收标准化后的目标 URL 字符串，返回 Proxy 表示代理，返回 nil/无值表示直连；返回 `nil, err` 或抛错终止请求。回调在 Machine 的 LocalSet 执行，可以挂起；请求超时也包含它。首次请求及每次重定向都重新决策，应用可以在这里实现主机白名单、直连或其他策略。

同一个 client 的直连池、不同 Proxy 句柄的代理池相互隔离；不同 client 始终独立。应复用 Proxy 句柄，回调不要每次 new；每个 client 最多缓存 32 条代理路由（LRU），淘汰缓存不强制取消已经取得连接的请求。来源 TLS 配置仍由 client 的 `ctx` 固定，HTTPS 代理使用 Proxy.new 中独立的 ctx；外层强制 HTTP/1.1，隧道内目标可继续协商 HTTP/2。

- HTTP 明文走 absolute-form 正向代理，代理凭证只加入发往代理的请求。
- HTTPS 目标走 CONNECT，然后对目标域名做独立 TLS/SNI/证书验证；HTTPS 代理可形成两层 TLS。
- SOCKS5 在本地异步解析目标域名，最多尝试 16 个地址；SOCKS5H 将域名交给代理，不在本地解析目标。IP 字面量直接发送 IPv4/IPv6 地址。
- `connect_timeout` 在 client.new 设置，默认 10000 毫秒，覆盖代理 DNS/TCP/TLS/握手及目标 TLS；非正数清除该独立超时。request.timeout 仍覆盖请求、路由、重定向和响应读取全过程。
- request 级 proxy/ctx 不允许覆盖固定配置；Proxy-Authorization 必须在 Proxy.new 配置，禁止作为普通请求头发送给目标。HTTP 返回 407 或 SOCKS 拒绝会返回错误，不自动降级认证或改走直连。

连接和流式 body 仍沿用 [HTTP 生命周期](http.md)。取消连接等待会释放当前连接 future；与池复用竞争产生的原生后台建连仍受 connect_timeout 和 client 生命周期约束，最后一个 client/执行句柄释放时通知并取消未完成建连。已接纳的流式响应可以继续读取；Machine 销毁不保留 Lua 回调。

## WebSocket

`websocket.connect(url, {proxy=proxy, ctx=origin_tls_context, timeout=...})` 使用同一代理配置；HTTP(S) 代理对 ws/wss 都建立 CONNECT 隧道，SOCKS 直接建立流隧道。WebSocket 自身仍强制 HTTP/1.1，timeout 覆盖代理连接、目标 TLS 和升级握手；选择是否代理由应用在调用前决定。

CONNECT 响应头总计最多 16 KiB、每组最多 64 个字段、最多 4 个 informational 响应，保留预读隧道字节；SOCKS 使用有界协议字段，错误/超时不把半握手连接放入池。代理 URL 最多 4096 字节，凭证合计最多 1024 字节。代理 URL 输入应由应用妥善保存；Lua 字符串和认证消息不承诺内存擦除。

协议依据：[HTTP CONNECT](https://www.rfc-editor.org/rfc/rfc9110.html#name-connect)、[SOCKS5](https://www.rfc-editor.org/rfc/rfc1928)、[SOCKS 用户名/密码认证](https://www.rfc-editor.org/rfc/rfc1929)。本模块不实现代理服务端、PAC、自动系统代理、SOCKS4、UDP ASSOCIATE 或业务地址过滤。
