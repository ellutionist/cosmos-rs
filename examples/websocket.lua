-- WebSocket 模块端到端测试：HTTP server 按 path 升级到 WebSocket，客户端用同模块收发
-- 运行：cargo run -p cosmos-cli -- examples/websocket.lua

local HttpServer = require("cosmos.network.http.server")
local HttpClient = require("cosmos.network.http.client")
local client = HttpClient.new()
local Ws = require("cosmos.network.websocket")
local TlsContext = require("cosmos.network.tcp.tls.context")
local thread = cosmos.thread

local failures = 0
local function check(cond, msg)
  if cond then
    print("ok   " .. msg)
  else
    failures = failures + 1
    print("FAIL " .. msg)
  end
end

-- Result<(), E> 成功时压 0 个值，不能直接 assert
local function noerr(_, err)
  assert(err == nil, err)
end

local ws_opts = nil -- 稍后填 tls 配置

local function ws_handler(conn, req)
  assert(req:header("upgrade") == "websocket", "ws handler 应看到 upgrade 头")
  if req:path() == "/wsbye" then
    noerr(conn:send("closing"))
    return
  end
  while true do
    local op, data = conn:receive()
    if not op then
      -- nil, "closed"：对端关闭
      break
    end
    if op == "text" then
      if data == "bye" then
        noerr(conn:send("bye-ack"))
        break
      end
      noerr(conn:send("echo:" .. data))
    else
      noerr(conn:send_binary(data))
    end
  end
end

local function handler(req)
  local path = req:path()
  if path == "/ws" or path == "/wsbye" then
    return { websocket = ws_handler }
  end
  return { status = 200, body = "http:" .. path }
end

-- 普通 HTTP 与 WebSocket 共用同一个监听端口
local srv = assert(HttpServer.listen("127.0.0.1", 0, handler))
local port = srv:port()
local ws_base = "ws://127.0.0.1:" .. port
local http_base = "http://127.0.0.1:" .. port

-- 同一端口上的普通 HTTP 请求不受影响
local r = assert(client:request("GET", http_base .. "/hello"))
check(r.status == 200 and r.body == "http:/hello", "同端口的普通 HTTP 仍可用")

-- 非升级请求打到 /ws：400
r = assert(client:request("GET", http_base .. "/ws"))
check(r.status == 400, "非 upgrade 请求 /ws 返回 400")

-- 文本 echo
local conn = assert(Ws.connect(ws_base .. "/ws"))
check(conn:state() == 0, "连接后 state() == 0")
noerr(conn:send("hello"))
local op, data = conn:receive()
check(op == "text" and data == "echo:hello", "文本帧 echo")
noerr(conn:send("中文 ütf8"))
op, data = conn:receive()
check(op == "text" and data == "echo:中文 ütf8", "UTF-8 文本帧 echo")

-- 非法 UTF-8 走 send 应报错，且连接不受影响
local _, err = conn:send("\255\254")
check(err ~= nil and err:find("UTF", 1, true) ~= nil, "send 拒绝非法 UTF-8")
noerr(conn:send("still-alive"))
op, data = conn:receive()
check(op == "text" and data == "echo:still-alive", "非法 UTF-8 后连接仍可用")

-- 二进制帧（任意字节，含 0 与 0xFF）
local bin = "a\0b\255c\1\2\3"
noerr(conn:send_binary(bin))
op, data = conn:receive()
check(op == "binary" and data == bin, "二进制帧 echo（字节完全一致）")

-- ping 由自动 pong 处理，不影响后续收发
noerr(conn:ping("p1"))
noerr(conn:send("after-ping"))
op, data = conn:receive()
check(op == "text" and data == "echo:after-ping", "ping 后收发正常")
-- 超大 ping payload 被拒绝
local _, perr = conn:ping(string.rep("x", 126))
check(perr ~= nil, "ping payload > 125 字节被拒绝")

-- 1MB 消息
local big = string.rep("x", 1024 * 1024)
noerr(conn:send(big))
op, data = conn:receive()
check(op == "text" and data == "echo:" .. big, "1MB 文本消息往返")

-- 客户端主动 close
noerr(conn:close(1000, "done"))
check(conn:state() == 1, "close 后 state() == 1")
op, data = conn:receive()
check(op == nil and data == "closed", "close 后 receive 返回 nil,closed")

-- 服务端主动关闭：先收到最后一条消息，再拿到 closed
conn = assert(Ws.connect(ws_base .. "/wsbye"))
op, data = conn:receive()
check(op == "text" and data == "closing", "服务端关闭前最后一条消息")
op, data = conn:receive()
check(op == nil and data == "closed", "服务端关闭后 receive 返回 nil,closed")
check(conn:state() == 1, "服务端关闭后 state() == 1")

-- 客户端超时：服务端不主动说话时应返回 nil,timeout 且连接仍可用
conn = assert(Ws.connect(ws_base .. "/ws"))
conn:settimeout(50)
op, data = conn:receive()
check(op == nil and data == "timeout", "receive 超时返回 nil,timeout")
conn:settimeout(0)
noerr(conn:send("after-timeout"))
op, data = conn:receive()
check(op == "text" and data == "echo:after-timeout", "超时后连接仍可用")
noerr(conn:close())

-- 并发连接：4 个协程各跑 10 次 echo
local workers = {}
local results = {}
for i = 1, 4 do
  workers[i] = thread.spawn(function()
    local c = assert(Ws.connect(ws_base .. "/ws"))
    for n = 1, 10 do
      noerr(c:send(i .. ":" .. n))
      local o, d = c:receive()
      if o ~= "text" or d ~= "echo:" .. i .. ":" .. n then
        results[i] = "mismatch"
        break
      end
    end
    results[i] = results[i] or "ok"
    noerr(c:close())
  end)
end
for i = 1, 4 do
  workers[i]:join()
  check(results[i] == "ok", "并发连接 worker " .. i)
end

-- wss：同一个 HTTP server 开 TLS（ALPN 强制 http/1.1 后走 upgrade）
local sctx = TlsContext.new_server()
noerr(sctx:add_ca_file("examples/certs/ca.pem"))
noerr(sctx:use_cert_chain_file("examples/certs/cert.pem"))
noerr(sctx:use_private_key_file("examples/certs/key.pem"))
local cctx = TlsContext.new()
noerr(cctx:add_ca_file("examples/certs/ca.pem"))
local tls_srv = assert(HttpServer.listen("127.0.0.1", 0, handler, { tls = sctx }))
local tls_conn = assert(Ws.connect("wss://localhost:" .. tls_srv:port() .. "/ws", { ctx = cctx }))
noerr(tls_conn:send("secure"))
op, data = tls_conn:receive()
check(op == "text" and data == "echo:secure", "wss 文本 echo")
noerr(tls_conn:close())
tls_srv:close()

-- 自定义请求头透传到服务端
local hconn = assert(Ws.connect(ws_base .. "/ws", { headers = { ["x-token"] = "abc" } }))
noerr(hconn:send("hdr"))
op, data = hconn:receive()
check(op == "text" and data == "echo:hdr", "带自定义头的握手")
noerr(hconn:close())

-- 连接失败：端口不通 / scheme 不对
local bad, berr = Ws.connect("ws://127.0.0.1:1/ws")
check(bad == nil and berr ~= nil, "连接失败返回 nil,err")
bad, berr = Ws.connect("http://127.0.0.1/ws")
check(bad == nil and berr:find("scheme", 1, true) ~= nil, "非 ws scheme 报错")

-- ws_max_message：超过上限的连接被服务端断开
local small_srv = assert(HttpServer.listen("127.0.0.1", 0, function(req)
  if req:path() ~= "/ws" then
    return { status = 404, body = "nope" }
  end
  return {
    websocket = function(c)
      local o = c:receive()
      if o then
        noerr(c:send("unexpected"))
      end
    end,
  }
end, { ws_max_message = 1024 }))
local sc = assert(Ws.connect("ws://127.0.0.1:" .. small_srv:port() .. "/ws"))
noerr(sc:send(string.rep("y", 2048)))
op, data = sc:receive()
check(op == nil, "超过 ws_max_message 的连接被服务端断开")
sc:close() -- 已被服务端断开，忽略返回
small_srv:close()

srv:close()

if failures > 0 then
  error(failures .. " 个用例失败")
end
print("all websocket checks passed")
