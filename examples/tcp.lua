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
assert(conn:state() == 0, "connected state should be 0, got " .. tostring(conn:state()))
print("connected, state:", conn:state())

-- 连接超时：第三参可选（黑洞地址 192.0.2.1/80 不会响应握手）
local bc, bcerr = Tcp.connect("192.0.2.1", 80, 150)
assert(bc == nil and bcerr == "timeout",
    string.format("connect timeout: c=%s err=%s", tostring(bc), tostring(bcerr)))
print("connect timeout ok")

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

-- receive()：缺省等价 "*l"
writer:write("third line\n")
local l3 = reader:receive()
assert(l3 == "third line", "default receive: " .. tostring(l3))
print("receive() default ok:", l3)

-- receive("*a")：读到 EOF
writer:write("all the data")
writer:close() -- 客户端关写半 → 服务端收到 EOF 断开
assert(writer:state() == 1, "writer state should be 1 after close")
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

-- *L：保留行尾 \n（以及前置 \r）
local conn4 = assert(Tcp.connect("127.0.0.1", port))
local r4, w4 = conn4:split()
w4:write("keep me\r\n")
assert(r4:receive("*L") == "keep me\r\n", "*L should keep EOL")
w4:write("starless\n")
assert(r4:receive("L") == "starless\n", "pattern without '*' should work")
w4:write("a\rb\r\n")
assert(r4:receive("*l") == "a\rb", "*l only trims the trailing CR/LF")
print("*L ok")

-- receiveuntil：返回分隔符之前的数据，分隔符被消费
w4:write("hello--world--tail")
assert(r4:receiveuntil("--") == "hello", "receiveuntil 1")
assert(r4:receiveuntil("--") == "world", "receiveuntil 2")
assert(r4:receive(4) == "tail", "rest after receiveuntil")

-- 分隔符跨 BufReader 分块边界：先写满 8192 字节并停在 "\r"，再补 "\n"
local pad = string.rep("x", 8191)
w4:write(pad .. "\r")
cosmos.sleep(50)
w4:write("\nTAIL")
local cross = r4:receiveuntil("\r\n")
assert(#cross == 8191 and cross == pad, "cross-boundary receiveuntil")
assert(r4:receive(4) == "TAIL", "rest after cross-boundary until")
print("receiveuntil ok")

-- setbufsize：*l 上限，超限返回 nil,"buffer overflow",partial，解除后继续读不丢数据
local conn5 = assert(Tcp.connect("127.0.0.1", port))
local r5, w5 = conn5:split()
r5:setbufsize(4)
w5:write("abcdefgh\n")
local od, oerr, opart = r5:receive("*l")
assert(od == nil and oerr == "buffer overflow" and opart == "abcd",
    string.format("line cap: d=%s err=%s partial=%s",
        tostring(od), tostring(oerr), tostring(opart)))
r5:setbufsize(0) -- 解除上限：未读的 "efgh\n" 还在流里
assert(r5:receive("*l") == "efgh", "line rest after overflow")

-- *a 上限
w5:write("0123456789")
w5:close() -- 关写半，*a 靠 EOF 结束
r5:setbufsize(4)
local ad, aerr, apart = r5:receive("*a")
assert(ad == nil and aerr == "buffer overflow" and apart == "0123", "*a cap")
r5:setbufsize(0)
local ad2 = r5:receive("*a")
assert(ad2 == "456789", "*a rest after overflow: " .. tostring(ad2))

-- receiveuntil 上限
local conn6 = assert(Tcp.connect("127.0.0.1", port))
local r6, w6 = conn6:split()
r6:setbufsize(4)
w6:write("0123456789")
w6:close()
local ud, uerr, upart = r6:receiveuntil("--")
assert(ud == nil and uerr == "buffer overflow" and upart == "0123", "until cap")
r6:setbufsize(0)
local ud2, uerr2, upart2 = r6:receiveuntil("--")
assert(ud2 == nil and uerr2 == "closed" and upart2 == "456789",
    string.format("until rest: d=%s err=%s partial=%s",
        tostring(ud2), tostring(uerr2), tostring(upart2)))
print("setbufsize ok")

print("OK")
