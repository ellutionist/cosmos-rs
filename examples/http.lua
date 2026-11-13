-- HTTP 模块端到端测试：同一进程里起 server + 发 client 请求（靠协程交错）
-- 运行：cargo run -- examples/http.lua

local HttpServer = require("cosmos.network.http.server")
local HttpClient = require("cosmos.network.http.client")
local TlsContext = require("cosmos.network.tcp.tls.context")
local Tcp = require("cosmos.network.tcp.stream")

local failures = 0
-- Result<(), E> 成功时压 0 个值，不能直接 assert
local function noerr(_, err)
  assert(err == nil, err)
end
local function check(cond, msg)
  if cond then
    print("ok   " .. msg)
  else
    failures = failures + 1
    print("FAIL " .. msg)
  end
end

local other -- 跨域重定向目标（稍后创建，handler 里按 upvalue 取端口）
local function handler(req)
  local path = req:path()
  if path == "/echo" then
    return {
      status = 200,
      headers = {
        ["x-method"] = req:method(),
        ["x-body-len"] = tostring(#req:body()),
      },
      body = req:body(),
    }
  elseif path == "/slow" then
    cosmos.sleep(200) -- 毫秒
    return { status = 200, headers = { ["x-remote"] = req:remote_addr() }, body = "slow done" }
  elseif path == "/redirect" then
    return { status = 302, headers = { location = "/echo?from=redirect" }, body = "" }
  elseif path == "/redirect307" then
    return { status = 307, headers = { location = "/echo" }, body = "" }
  elseif path == "/cross" then
    return { status = 302, headers = { location = "http://127.0.0.1:" .. other:port() .. "/auth" }, body = "" }
  elseif path == "/same" then
    return { status = 302, headers = { location = "/whoami" }, body = "" }
  elseif path == "/whoami" then
    return { status = 200, headers = { ["x-seen-auth"] = req:header("authorization") or "none" }, body = "" }
  elseif path == "/proxy" then
    -- handler 内部再发客户端请求：服务端协程里嵌套一个客户端协程
    local sub = assert(HttpClient.request("GET", "http://127.0.0.1:" .. other:port() .. "/echo?via=proxy"))
    return { status = 200, body = "proxied:" .. sub.body }
  elseif path == "/boom" then
    error("handler exploded")
  elseif path == "/missing" then
    return { status = 404, body = "nope" }
  elseif path == "/raw" then
    return "a\0b\255c"
  elseif path == "/nothing" then
    return
  elseif path == "/nil" then
    return nil
  elseif path == "/nil-ignored" then
    return nil, 42
  elseif path == "/nil-error" then
    return nil, "explicit handler error"
  elseif path == "/bad-return" then
    return false
  elseif path == "/bad-field" then
    return { body = {} }
  end
  return { status = 200, body = "hello " .. req:target() }
end

local srv = assert(HttpServer.listen("127.0.0.1", 0, handler))
local base = "http://127.0.0.1:" .. srv:port()
other = assert(HttpServer.listen("127.0.0.1", 0, function(req)
  return {
    status = 200,
    headers = { ["x-seen-auth"] = req:header("authorization") or "none" },
    body = req:method() .. ":" .. req:body(),
  }
end))

-- GET + query
local r = assert(HttpClient.request("GET", base .. "/hello?x=1&y=2"))
check(r.status == 200, "GET status")
check(r.body == "hello /hello?x=1&y=2", "GET body 带 query")
check(r.version == 11, "明文连接走 HTTP/1.1")
check(r.headers["content-length"] == tostring(#r.body), "响应带 content-length")

-- POST body（二进制安全）
local payload = "a\0b\255c"
r = assert(HttpClient.request("POST", base .. "/echo", {
  headers = { ["x-custom"] = "v1" },
  body = payload,
}))
check(r.status == 200, "POST status")
check(r.body == payload, "POST body 往返（二进制安全）")
check(r.headers["x-method"] == "POST", "handler 看到 method")
check(r.headers["x-body-len"] == tostring(#payload), "handler 看到 body 长度")

-- 404 不是错误，原样返回
r = assert(HttpClient.request("GET", base .. "/missing"))
check(r.status == 404 and r.body == "nope", "404 原样返回")

-- 回调结果由类型化桥接读取，覆盖单值、多值及错误后的下一次调用。
r = assert(HttpClient.request("GET", base .. "/raw"))
check(r.status == 200 and r.body == "a\0b\255c", "handler 直接返回二进制字符串")
for _, path in ipairs({ "/nothing", "/nil", "/nil-ignored" }) do
  r = assert(HttpClient.request("GET", base .. path))
  check(r.status == 200 and r.body == "", "handler 缺省响应 " .. path)
end
for _, path in ipairs({ "/nil-error", "/bad-return", "/bad-field" }) do
  r = assert(HttpClient.request("GET", base .. path))
  check(r.status == 500, "handler 非法返回值 " .. path)
end
r = assert(HttpClient.request("GET", base .. "/hello"))
check(r.status == 200, "handler 转换失败后后续请求正常")

-- 重定向跟随（302 → GET）
r = assert(HttpClient.request("GET", base .. "/redirect"))
check(r.status == 200 and r.body == "", "跟随 302 到 /echo")
-- 关闭重定向
r = assert(HttpClient.request("GET", base .. "/redirect", { redirects = 0 }))
check(r.status == 302 and r.headers.location == "/echo?from=redirect", "redirects=0 返回 302")

-- 307 保留方法和 body
r = assert(HttpClient.request("POST", base .. "/redirect307", { body = "keep" }))
check(r.headers["x-method"] == "POST" and r.body == "keep", "307 保留方法 + body")

-- 重定向跨域时剥掉 Authorization，同源保留
r = assert(HttpClient.request("POST", base .. "/cross", {
  headers = { authorization = "Bearer secret" },
  body = "x",
}))
check(r.headers["x-seen-auth"] == "none", "跨域重定向剥掉 Authorization")
r = assert(HttpClient.request("POST", base .. "/same", {
  headers = { authorization = "Bearer secret" },
  body = "x",
}))
check(r.headers["x-seen-auth"] == "Bearer secret", "同源重定向保留 Authorization")

-- handler 里嵌套客户端请求
r = assert(HttpClient.request("GET", base .. "/proxy"))
check(r.body == "proxied:GET:", "handler 内部发客户端请求")

-- handler 抛错 → 500
r = assert(HttpClient.request("GET", base .. "/boom"))
check(r.status == 500, "handler 错误 → 500")

-- 超时（handler 里 sleep 200ms，客户端 50ms）
local resp, err = HttpClient.request("GET", base .. "/slow", { timeout = 50 })
check(resp == nil and err == "timeout", "整体超时返回 nil, timeout")

-- 并发：两个 /slow 请求同时进行，总耗时接近单个（而不是两倍）
local results = {}
local t0 = cosmos.now_ms()
local t1 = cosmos.thread.spawn(function()
  results[1] = assert(HttpClient.request("GET", base .. "/slow"))
end)
local t2 = cosmos.thread.spawn(function()
  results[2] = assert(HttpClient.request("GET", base .. "/slow"))
end)
t1:join()
t2:join()
local elapsed = cosmos.now_ms() - t0
check(results[1].body == "slow done" and results[2].body == "slow done", "并发请求都成功")
check(elapsed < 350, string.format("两个 200ms 请求并发完成（%.0fms）", elapsed))

-- HTTPS + h2：客户端 ALPN 默认 h2/http1.1，服务端两种都开
local sctx = TlsContext.new_server()
noerr(sctx:add_ca_file("examples/certs/ca.pem"))
noerr(sctx:use_cert_chain_file("examples/certs/cert.pem"))
noerr(sctx:use_private_key_file("examples/certs/key.pem"))
sctx:add_alpn("h2")
sctx:add_alpn("http/1.1")

local cctx = TlsContext.new()
noerr(cctx:add_ca_file("examples/certs/ca.pem"))
-- cctx 不设 ALPN：HTTP 客户端会默认补 h2/http1.1

local tls_srv = assert(HttpServer.listen("127.0.0.1", 0, handler, { tls = sctx }))
local tls_base = "https://localhost:" .. tls_srv:port()

r = assert(HttpClient.request("POST", tls_base .. "/echo", { ctx = cctx, body = "tls-body" }))
check(r.status == 200 and r.body == "tls-body", "HTTPS POST echo")
check(r.version == 20, "HTTPS 协商到 HTTP/2")

-- h1-only 客户端配置：同一个 TLS 服务端回落到 HTTP/1.1
local cctx11 = TlsContext.new()
noerr(cctx11:add_ca_file("examples/certs/ca.pem"))
cctx11:add_alpn("http/1.1")
r = assert(HttpClient.request("GET", tls_base .. "/hello", { ctx = cctx11 }))
check(r.version == 11 and r.body == "hello /hello", "ALPN 只有 http/1.1 时走 h1")

-- h2 单连接多路复用：先热身建立 h2 连接，再并发两个 /slow（同一连接的不同 stream）
assert(HttpClient.request("GET", tls_base .. "/slow", { ctx = cctx }))
local h2r = {}
local ht0 = cosmos.now_ms()
local ta = cosmos.thread.spawn(function()
  h2r[1] = assert(HttpClient.request("GET", tls_base .. "/slow", { ctx = cctx }))
end)
local tb = cosmos.thread.spawn(function()
  h2r[2] = assert(HttpClient.request("GET", tls_base .. "/slow", { ctx = cctx }))
end)
ta:join()
tb:join()
local helapsed = cosmos.now_ms() - ht0
check(h2r[1].version == 20 and h2r[2].version == 20, "两个并发请求都走 HTTP/2")
check(h2r[1].headers["x-remote"] == h2r[2].headers["x-remote"], "两个请求复用了同一条 h2 连接")
check(helapsed < 350, string.format("h2 多路复用并发完成（%.0fms）", helapsed))

-- 默认信任根连自签证书：应当失败（verify 错误）
local resp2, err2 = HttpClient.request("GET", tls_base .. "/hello")
check(resp2 == nil and err2 ~= nil, "默认根证书拒绝自签服务端")

-- 客户端响应体上限
local rb, rberr = HttpClient.request("GET", base .. "/hello", { max_body = 8 })
check(rb == nil and rberr ~= nil, "客户端 max_body 超限返回错误")

-- 大 body（1MB）往返
local big = string.rep("x", 1024 * 1024)
r = assert(HttpClient.request("POST", base .. "/echo", { body = big }))
check(#r.body == #big and r.body == big, "1MB body 往返")

-- h2 大 body（超过默认 64KB 流控窗口）
r = assert(HttpClient.request("POST", tls_base .. "/echo", { ctx = cctx, body = big }))
check(r.version == 20 and #r.body == #big and r.body == big, "h2 1MB body 往返")


-- 请求体超过 max_body
local small = assert(HttpServer.listen("127.0.0.1", 0, handler, { max_body = 1024 }))
local sr = assert(HttpClient.request("POST", "http://127.0.0.1:" .. small:port() .. "/echo", {
  body = string.rep("y", 4096),
}))
check(sr.status == 413, "超过 max_body → 413")
small:close()

tls_srv:close()
srv:close()
-- 连接池里的 keep-alive 连接仍可用（与 C++ 行为一致）；这里验证监听已停止
cosmos.sleep(10)
local conn, cerr = Tcp.connect("127.0.0.1", srv:port(), 200)
check(conn == nil and cerr ~= nil, "close() 后不再监听")
other:close()

if failures > 0 then
  error(string.format("%d checks failed", failures))
end
print("http ok")
