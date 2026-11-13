-- HTTP 压测用服务端（单进程、单 runtime）
-- 运行：cargo run -p cosmos-cli --release -- examples/http_bench.lua [port] [body_bytes]
-- 然后：wrk -t1 -c10 -d10s --latency http://127.0.0.1:<port>/

local HttpServer = require("cosmos.network.http.server")

local arg_port, arg_body = ...
local port = tonumber(arg_port) or 18080
local body_bytes = tonumber(arg_body) or 13
local body = string.rep("x", body_bytes)

io.stdout:setvbuf("no")

local srv = assert(HttpServer.listen("127.0.0.1", port, function(req)
  if req:path() == "/echo" then
    return { status = 200, headers = { ["content-type"] = "text/plain" }, body = req:body() }
  end
  return { status = 200, headers = { ["content-type"] = "text/plain" }, body = body }
end))

print(string.format("listening on 127.0.0.1:%d (body=%d bytes)", srv:port(), body_bytes))

-- 主协程返回会触发 lua_close；压测期间需要进程一直活着
while true do
  cosmos.time.sleep(60000)
end
