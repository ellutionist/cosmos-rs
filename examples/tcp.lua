local port = tonumber(...)
assert(port, "usage: tcp.lua <port>")

local Tcp = require("cosmos.Tcp")

-- 连接失败：nil + 错误串
local bad, bad_err = Tcp.connect("127.0.0.1", 1)
assert(bad == nil, "expected connect failure")
assert(type(bad_err) == "string", "expected error string")
print("connect error ok:", bad_err)

local conn, err = Tcp.connect("127.0.0.1", port)
assert(conn, "connect failed: " .. tostring(err))
print("connected, state:", conn:state())

local reader, writer = conn:split()
assert(reader and writer, "split failed")

-- receive(n)：精确 n 字节
writer:write("hello")
local data = reader:receive(5)
assert(data == "hello", "receive(n): " .. tostring(data))
print("receive(5):", data)

-- 大 payload（>4096 字节，走 LuaStr 锚定路径；临时串，唯一引用是参数槽，靠锚定保活）
local n2 = writer:write(string.rep("x", 8192))
assert(n2 == 8192, "big write: " .. tostring(n2))
local back = reader:receive(8192)
assert(#back == 8192 and back == string.rep("x", 8192), "big echo mismatch")
print("big write/receive ok:", n2, "bytes")

-- receive("*l")：逐行读
writer:write("first line\nsecond line\n")
local l1 = reader:receive("*l")
local l2 = reader:receive("*l")
assert(l1 == "first line", "line1: " .. tostring(l1))
assert(l2 == "second line", "line2: " .. tostring(l2))
print("receive(*l):", l1, "|", l2)

-- receive("*a")：读到 EOF
writer:write("all the data")
writer:close() -- 客户端关写半 → 服务端收到 EOF 断开
local rest = reader:receive("*a")
assert(rest == "all the data", "receive(*a): " .. tostring(rest))
print("receive(*a):", rest)

-- 之后干净 EOF
local eof = reader:receive("*l")
assert(eof == nil, "expected clean EOF")
print("clean EOF ok")

-- 提前关闭：nil, "closed", partial（三值协议）
local conn2 = assert(Tcp.connect("127.0.0.1", port))
local r2, w2 = conn2:split()
w2:write("hi")
w2:close() -- 服务端回显 "hi" 后收到 EOF 断开
local d2, cerr, cpartial = r2:receive(10)
assert(d2 == nil and cerr == "closed" and cpartial == "hi",
    string.format("premature close: d=%s err=%s partial=%s",
        tostring(d2), tostring(cerr), tostring(cpartial)))
print("premature close ok, partial:", cpartial)

-- 已关闭后再调用：不 panic，reader/writer 各自返回 nil, "closed"
r2:close()
local cd, ce = r2:receive(1)
assert(cd == nil and ce == "closed", "closed reader: " .. tostring(ce))
local wd, we = w2:write("x")
assert(wd == nil and we == "closed", "closed writer: " .. tostring(we))
print("close-after-close ok")

-- settimeout：无数据 → nil, "timeout"，且连接不关闭
local conn3 = assert(Tcp.connect("127.0.0.1", port))
conn3:settimeout(100) -- split 时传给 reader/writer
local r3, w3 = conn3:split()
local td, terr, tpartial = r3:receive(10)
assert(td == nil and terr == "timeout" and tpartial == "",
    string.format("receive timeout: d=%s err=%s partial=%s",
        tostring(td), tostring(terr), tostring(tpartial)))

-- *l 超时：已读到的残行作为 partial 交回
w3:write("half")
local ld, lerr, lpartial = r3:receive("*l")
assert(ld == nil and lerr == "timeout" and lpartial == "half",
    string.format("line timeout: d=%s err=%s partial=%s",
        tostring(ld), tostring(lerr), tostring(lpartial)))

-- 超时不清连接：之后读写照常
w3:write("still alive\n")
assert(r3:receive("*l") == "still alive", "conn unusable after timeout")
r3:settimeout(0) -- 清除超时
print("timeout ok")

print("OK")
