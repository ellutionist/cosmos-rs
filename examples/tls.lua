local port = tonumber(...)
assert(port, "usage: tls.lua <port>")

local Tls = require("cosmos.Tls")
local TlsContext = require("cosmos.TlsContext")

local function read_file(path)
    local f = assert(io.open(path, "rb"))
    local s = f:read("*a")
    f:close()
    return s
end

local cert_pem = read_file("examples/certs/cert.pem")

-- 不给 context：默认公有根证书，测试服务器是私有 CA 签的，必须被拒绝（错误分类 verify:）
local bad, berr = Tls.connect("localhost", port, 1000)
assert(bad == nil and type(berr) == "string", "private CA should be rejected")
assert(berr:match("^verify: "), "expected verify error, got: " .. berr)
print("verify reject ok:", berr)

-- 指定 CA（追加到信任根）：握手成功
local ctx = TlsContext.new()
assert(ctx:add_ca_file("examples/certs/ca.pem") == nil, "add_ca_file failed")

local conn, err = Tls.connect("localhost", port, 2000, ctx)
assert(conn, "tls connect failed: " .. tostring(err))
assert(conn:state() == 0, "state should be 0")

-- getpeercert：对端叶证书应为服务的 cert.pem
local peer = conn:getpeercert()
assert(peer and peer:match("^%-%-%-%-%-BEGIN CERTIFICATE%-%-%-%-%-"),
    "getpeercert should return PEM")
assert((peer:gsub("%s", "")) == (cert_pem:gsub("%s", "")), "peer cert mismatch")
print("tls connected, peer cert ok")

local r, w = conn:split()

-- 按行
w:write("hello tls\n")
assert(r:receive("*l") == "hello tls", "tls line")

-- 大 payload（跨 TLS record / 分块）
local big = string.rep("z", 8192)
assert(w:write(big) == 8192, "tls big write")
local back = r:receive(8192)
assert(#back == 8192 and back == big, "tls big echo mismatch")
print("tls big echo ok")

-- receiveuntil 迭代器在 TLS reader 上同样可用（同一个 TcpReader 类型）
w:write("part1|part2|")
local it = r:receiveuntil("|")
assert(it() == "part1", "tls until 1")
assert(it() == "part2", "tls until 2")
print("tls until ok")

-- 半关闭：writer:close() 发 close_notify，对端回 close_notify 后 reader 读到干净 EOF
-- （r 已被 receiveuntil 迭代器接管，另开一条连接观察）
local c2 = assert(Tls.connect("localhost", port, 2000, ctx))
local rr, ww = c2:split()
ww:write("abc")
ww:close()
assert(rr:receive("*a") == "abc", "clean EOF after close_notify")
print("tls close_notify ok")

-- IP 直连：证书含 IP:127.0.0.1 SAN
local ipconn = assert(Tls.connect("127.0.0.1", port, 2000, ctx))
ipconn:close()
assert(ipconn:state() == 1, "closed state should be 1")

-- add_ca_dir：整个目录批量加载（非 PEM 文件跳过）
local dctx = TlsContext.new_server()
assert(dctx:add_ca_dir("examples/certs") == nil, "add_ca_dir failed")
local dconn = assert(Tls.connect("localhost", port, 2000, dctx))
dconn:close()
print("add_ca_dir ok")

-- verify_none：不校验证书也能连
local insecure = TlsContext.new()
insecure:verify_none()
local ic = assert(Tls.connect("localhost", port, 2000, insecure))
ic:close()
print("verify_none ok")

-- 客户端证书 + ALPN：配置能构建并连接（服务端未要求 mTLS/ALPN）
local mtls = TlsContext.new()
mtls:verify_none()
mtls:use_cert_chain_file("examples/certs/client.pem")
mtls:use_private_key_file("examples/certs/client.key")
mtls:add_alpn("h2")
local mc = assert(Tls.connect("localhost", port, 2000, mtls))
mc:close()
print("mtls/alpn config ok")

-- 会话复用参数：设置容量 / 关闭后都能正常连接
local rctx = TlsContext.new()
rctx:add_ca_file("examples/certs/ca.pem")
rctx:set_resumption_size(64)
local rc1 = assert(Tls.connect("localhost", port, 2000, rctx))
rc1:close()
rctx:disable_resumption()
local rc2 = assert(Tls.connect("localhost", port, 2000, rctx))
rc2:close()
print("resumption config ok")

-- settimeout：无数据超时
local slow = assert(Tls.connect("localhost", port, 2000, ctx))
slow:settimeout(100)
local r2 = slow:split()
local d, terr = r2:receive(1)
assert(d == nil and terr == "timeout", "tls timeout: " .. tostring(terr))
print("tls timeout ok")

print("OK")
