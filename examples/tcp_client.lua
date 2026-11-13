local port = tonumber(...)
assert(port, "usage: tcp_client.lua <port>")

local Tcp = require("cosmos.Tcp")

-- 先发一个慢请求（服务端 handler 会 sleep 300ms）
local c1 = assert(Tcp.connect("127.0.0.1", port, 2000))
c1:settimeout(2000)
local r1, w1 = c1:split()
w1:write("slow\n")

-- 再发一个快请求：若 handler 串行，快请求会被慢请求阻塞
local c2 = assert(Tcp.connect("127.0.0.1", port, 2000))
c2:settimeout(2000)
local r2, w2 = c2:split()
local t0 = cosmos.now_ms()
w2:write("fast\n")
assert(r2:receive("*l") == "fast", "fast echo")
local fast_ms = cosmos.now_ms() - t0
assert(fast_ms < 200, "handlers should run concurrently, fast took " .. fast_ms .. "ms")
print(string.format("fast response in %.0fms", fast_ms))

assert(r1:receive("*l") == "slow", "slow echo")
w1:close()
w2:close()
print("OK")
