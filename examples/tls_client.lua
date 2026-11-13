local port = tonumber(...)
assert(port, "usage: tls_client.lua <port>")

local Tls = require("cosmos.network.tcp.tls")
local TlsContext = require("cosmos.network.tcp.tls.context")

-- 1) mTLS 客户端：带客户端证书，服务端要求并校验它
local mctx = TlsContext.new()
assert(mctx:add_ca_file("examples/certs/ca.pem") == nil, "add_ca_file")
assert(mctx:use_cert_chain_file("examples/certs/client.pem") == nil, "use_cert_chain_file")
assert(mctx:use_private_key_file("examples/certs/client.key") == nil, "use_private_key_file")

local c1 = assert(Tls.connect("localhost", port, 2000, mctx))
local r1, w1 = c1:split()
w1:write("mtls\n")
assert(r1:receive("*l") == "mtls", "mtls echo mismatch")
w1:close()
print("mtls echo ok")

-- 2) 不带客户端证书：服务端拒绝（TLS1.3 下可能握手"成功"后才在读写时报错）
local pctx = TlsContext.new()
assert(pctx:add_ca_file("examples/certs/ca.pem") == nil, "add_ca_file")
local c2, cerr = Tls.connect("localhost", port, 2000, pctx)
if c2 then
    local r2 = c2:split()
    local d, e = r2:receive("*l")
    assert(d == nil and type(e) == "string", "server should reject missing client cert")
    print("mtls reject ok:", e)
else
    assert(type(cerr) == "string", "expected error string")
    print("mtls reject ok (connect):", cerr)
end

-- 3) 服务端 reload 后不再要求客户端证书：普通客户端成功
local c3 = assert(Tls.connect("localhost", port, 2000, pctx))
local r3, w3 = c3:split()
w3:write("plain\n")
assert(r3:receive("*l") == "plain", "post-reload echo mismatch")
w3:close()
print("reload client ok")

print("OK")
