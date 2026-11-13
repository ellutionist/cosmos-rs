local port = tonumber(...)
assert(port, "usage: tls_server.lua <port>")

local Tls = require("cosmos.Tls")
local TlsContext = require("cosmos.TlsContext")

-- 服务端 ctx：证书链 + 私钥 + 客户端证书校验（mTLS）
local ctx = TlsContext.new_server()
assert(ctx:use_cert_chain_file("examples/certs/cert.pem") == nil, "use_cert_chain_file")
assert(ctx:use_private_key_file("examples/certs/key.pem") == nil, "use_private_key_file")
assert(ctx:add_ca_file("examples/certs/ca.pem") == nil, "add_ca_file")
ctx:verify_client()

local srv = assert(Tls.listen(port, ctx, 3000))
print("listening on", port)

local function echo(conn)
    local r, w = conn:split()
    local line = r:receive("*l")
    assert(line, "server read line failed")
    w:write(line .. "\n")
    w:close()
end

-- 1) 带客户端证书：mTLS 握手成功，回显
echo(assert(srv:accept()))

-- 2) 不带客户端证书：握手失败（错误带分类前缀）
local bad, err = srv:accept()
assert(bad == nil and type(err) == "string", "expected client cert rejection")
print("mtls reject:", err)

-- 3) 热替换为不要求客户端证书的配置：监听不中断，新连接生效
local ctx2 = TlsContext.new_server()
assert(ctx2:use_cert_chain_file("examples/certs/cert.pem") == nil, "use_cert_chain_file")
assert(ctx2:use_private_key_file("examples/certs/key.pem") == nil, "use_private_key_file")
assert(srv:reload(ctx2) == nil, "reload failed")
echo(assert(srv:accept()))

srv:close()
print("server done")
