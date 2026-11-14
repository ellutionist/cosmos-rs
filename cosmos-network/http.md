# HTTP 流式传输

HTTP/1.1、HTTP/2 的客户端和服务端共用 `cosmos.network.http.body`。网络输入、普通文件和 Lua 生产函数均可逐块读取或发送；无需把整个文件放进 Lua 字符串。完整用法和本地验收见 [http_stream.lua](../examples/http_stream.lua)，旧字节串接口见 [http.lua](../examples/http.lua)。

请求地址与重定向使用标准 URL 解析，支持相对路径、点段、query/fragment 合并；fragment 不发送，URL userinfo 不提供隐式认证。通用组件与查询操作见 [URL 用法](url.md) 和 [url.lua](../examples/url.lua)。

client.new 支持固定代理或可挂起的逐跳路由回调，HTTP(S)/SOCKS5/SOCKS5H 与认证见 [代理用法](proxy.md)。

## 收发方式

客户端 `client:request(method, url, options)` 默认仍返回字符串 `response.body`；传入 `stream=true` 后，在响应头就绪时返回，`response.body` 是 body 句柄。请求的 `options.body` 可以是字符串，也可以是 body 句柄，两者独立配置。

服务端 `server.listen(host, port, handler, options)` 默认仍先缓冲完整请求体。传入 `stream_body=true` 后，在请求头就绪时调用 handler；通过 `request:stream()` 读取输入，或用 `request:body(limit?)` 显式收集剩余内容。handler 返回的 `{status, headers, body}` 中，body 同样接受字符串或 body 句柄。可直接返回 `request:stream()`，也可在输出回调里按需读取输入，组合流式转发。

| body 接口 | 语义 |
| --- | --- |
| `body.from_file(file, options?)` | 普通文件源；`offset` 默认 0，`length` 默认读到创建源时的文件末尾 |
| `body.from_callback(fn, options?)` | `fn(max_bytes)` 返回不超过请求大小的非空二进制字符串；`nil` 表示 EOF，`nil, err` 表示失败；`options.length` 可声明总长度 |
| `reader:read(size?)` | 返回一个块；默认 64 KiB，最大 1 MiB；EOF 为 `nil`，错误为 `nil, err` |
| `reader:read_all(limit?)` | 收集剩余内容，默认及最大上限均为 64 MiB；超限关闭 reader 并返回错误 |
| `reader:trailers()` | 读到 EOF 后获取输入尾部字段；不自动转发到输出流 |
| `reader:settimeout(ms)` | 后续每次读取的超时；0 清除，返回 true |
| `reader:close()` / `reader:is_closed()` | 幂等关闭 / 查询关闭或 EOF 状态 |

文件偏移、长度、声明总长度均为非负精确整数，最大 `2^53-1`，范围不能超出文件快照长度。输入文件必须可读，目录等其他类型会被拒绝。读期间文件变短会返回错误；变长不会超过原定范围。读取基于同一个打开的文件，不会因路径被替换而切换到新文件，也不提供并发写入的内容快照。

文件源按偏移读取，不改变原 `cosmos.fs.file` 的游标。`body.from_file` 默认 `close_file=false`；设为 true 后，EOF、关闭、发送结束、错误或取消会关闭原共享文件句柄。两者共享文件的超时和串行调度；原文件关闭会使源失效。即使 `close_file=false`，原生读取派发后的取消仍沿用 FS 的关闭语义，详见 [FS 文档](../cosmos/fs.md)。

body 是一次性流：可连续读取，或交给一次 HTTP 发送；开始手动读取后不能再交给 HTTP，发送后不能手动读取或重复发送。多个读取者串行消费同一输入，不会各自获得副本。调用者应消费到 EOF 或显式关闭不再需要的响应体，及时释放连接和上传源。

## 文件响应

`cosmos.network.http.file.response(file, request, options?)` 返回可直接交给 handler 的响应表。默认接管文件的关闭责任；`options.close_file=false` 可保留原句柄。

- 支持 GET 和 HEAD，其他方法返回 405。HEAD 不读取文件，保留完整表示的 Content-Length，忽略 Range。
- 支持单个 `bytes=start-end`、`start-`、`-suffix`；有效范围返回 206 和 Content-Range，超出文件范围返回 416。无效语法和多范围请求回退完整 200，不生成 multipart/byteranges。
- 默认根据长度与纳秒 mtime 生成弱 ETag，并在可用时发送 Last-Modified。处理 If-Match、If-Unmodified-Since、If-None-Match、If-Modified-Since 及其优先级，返回 304 / 412。
- `options.etag` 可提供有效的强/弱 ETag。If-Range 仅在强 ETag 完全匹配时使用范围；弱 ETag 或日期回退完整响应。文件 mtime 不能证明一秒内未发生多次变化，因此不把 HTTP 日期当成强验证器，遵循 [RFC 9110](https://httpwg.org/specs/rfc9110.html#field.if-range)。
- `options.content_type` 默认 `application/octet-stream`；`options.cache_control` 可显式指定。鉴权、路径边界、MIME 选择、Content-Disposition 和缓存策略由应用设置；应用可以修改返回的 headers。

元数据与内容来自同一文件句柄；默认弱 ETag 不是内容摘要。应用提供强 ETag 时，需要自行确保它对应当前内容。发送期间应避免原地修改文件，可组合 FS 原子替换实现内容发布。

## 限制与生命周期

发送方按 64 KiB 分块，使用容量为 1 的队列连接本地生产源与 HTTP 驱动；接收方按需拉取。生产函数可调用其他异步 API，始终在 Machine 的 LocalSet 上执行，多线程 Tokio Runtime 也不会将 Lua 回调发送到工作线程。网络栈和系统套接字另有各自的有界缓冲。

客户端 `timeout` 继续覆盖整个请求、重定向和响应读取；流式模式返回响应头后仍保留原截止时间。body 的读取超时与该截止时间取较早者。超时/读取失败/进行中读取被取消后关闭 body；取消尚在排队的读取不终止已开始的读取。显式 close 会唤醒所有等待者。

客户端 `max_body` 限制响应总字节数；流式模式在读取时报告超限。服务端 `max_body` 同时限制缓冲和流式请求，默认 64 MiB；大文件上传应显式调高。流式 handler 自行处理读失败并选择响应状态，缓冲模式超限仍返回 413。`read_all` 的收集上限独立生效；超出 64 MiB 的传输请使用 read 循环。

声明长度的源不能提前 EOF；回调不能返回空块或超过请求大小。已知长度由 HTTP 驱动生成 Content-Length，未知长度由 HTTP/1.1 分块编码或 HTTP/2 数据帧承载；不要手工设置 Transfer-Encoding。手工 Content-Length 必须与已知长度一致。HEAD / 204 / 205 / 304 不调用生产函数。

字符串请求保持原重定向行为。保留原方法/body 的重定向（例如 307 / 308）不能自动重放一次性流，会明确返回错误；转换到 GET 的重定向可继续跟随并取消原上传。跨源仍剥离凭证头。

对端断开、发送取消或 Machine 销毁会取消相应的本地生产任务；响应结束后关闭关联的请求输入。文件关闭遵循前面的所有权配置。原生系统调用不能强制撤销，已经派发的文件 I/O 可能执行到完成；不承诺取消会回滚文件副作用。服务端停止接受与已有连接排空仍属于路线图的服务生命周期阶段。
