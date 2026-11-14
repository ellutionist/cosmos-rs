-- 顺序协议 socket：明文握手后升级 TLS、Unix 传输，以及失败和取消后的关闭。
local Socket = require("cosmos.network.socket")
local Context = require("cosmos.network.tcp.tls.context")
assert(Socket == cosmos.network.socket)
assert(Socket.listener == require("cosmos.network.socket.listener"))

local server_context = Context.new_server()
server_context:use_cert_chain_file("examples/certs/cert.pem")
server_context:use_private_key_file("examples/certs/key.pem")
local client_context = Context.new()
client_context:add_ca_file("examples/certs/ca.pem")

local function join(task)
    local _, err = task:join()
    assert(err == nil, err)
end

local function upgrade_echo(listener, connect)
    local payload = string.rep(string.char(0, 128, 255, 1), 2048) .. "\0tail"
    local server = cosmos.thread.spawn(function()
        local peer = assert(listener:accept())
        peer:settimeout(2000)
        assert(peer:send("greeting\n") == 9)
        -- 让 SSL 请求和 ClientHello 一起到达，验证升级时移交预读缓冲。
        cosmos.time.sleep(30)
        assert(peer:receive(9) == "STARTTLS\n")
        assert(peer:starttls_server(server_context))
        local value = assert(peer:receive(#payload))
        assert(value == payload)
        assert(peer:send(value) == #payload)
        assert(peer:close() == 1)
    end)
    local client = Socket.new()
    client:settimeout(2000)
    connect(client)
    assert(client:receive() == "greeting")
    assert(client:send("STARTTLS\n") == 9)
    assert(client:starttls("localhost", client_context))
    local again, err = client:starttls("localhost", client_context)
    assert(again == nil and err == "TLS is already enabled")
    assert(client:send(payload) == #payload)
    local value, failure = client:receive(#payload)
    assert(value == payload, failure)
    assert(client:close() == 1)
    join(server)
end

local listener = assert(Socket.listen("127.0.0.1", 0, 2000))
local port = listener:port()
upgrade_echo(listener, function(client) assert(client:connect("127.0.0.1", port) == 1) end)

-- 半个协议包超时后，保留 partial 返回值，同时关闭连接。
local client = Socket.new()
client:settimeout(20)
assert(client:connect("127.0.0.1", port))
local peer = assert(listener:accept())
assert(peer:send("ab") == 2)
local value, err, partial = client:receive(4)
assert(value == nil and err == "timeout" and partial == "ab")
assert(client:state() == 1)
local sent, closed = client:send("invalid")
assert(sent == nil and closed == "closed")
peer:close()

-- 取消挂起的读取后，释放独占借用并丢弃连接。
client:settimeout(2000)
assert(client:connect("127.0.0.1", port))
peer = assert(listener:accept())
local started = false
local reader = cosmos.thread.spawn(function()
    started = true
    client:receive(1)
    error("已取消的读取不能继续执行")
end)
while not started do cosmos.time.sleep(0) end
reader:kill()
local _, cancelled = reader:join()
assert(cancelled and cancelled:find("cancelled", 1, true))
assert(client:state() == 1)
peer:close()

-- 升级过程中取消或超时，底层明文流也不能重新使用。
assert(client:connect("127.0.0.1", port))
peer = assert(listener:accept())
started = false
local handshake = cosmos.thread.spawn(function()
    started = true
    client:starttls("localhost", client_context)
    error("已取消的握手不能继续执行")
end)
while not started do cosmos.time.sleep(0) end
handshake:kill()
local _, handshake_cancelled = handshake:join()
assert(handshake_cancelled and handshake_cancelled:find("cancelled", 1, true))
assert(client:state() == 1)
peer:close()
assert(client:connect("127.0.0.1", port))
peer = assert(listener:accept())
client:settimeout(20)
local upgraded, handshake_error = client:starttls("localhost", client_context)
assert(upgraded == nil and handshake_error == "timeout")
assert(client:state() == 1)
peer:close()
client:settimeout(2000)

-- 证书验证失败时，原明文连接也必须关闭。
local rejected = cosmos.thread.spawn(function()
    local socket = assert(listener:accept())
    socket:settimeout(2000)
    local ok, failure = socket:starttls_server(server_context)
    assert(ok == nil and type(failure) == "string")
    assert(socket:state() == 1)
end)
assert(client:connect("127.0.0.1", port))
local ok, failure = client:starttls("localhost")
assert(ok == nil and type(failure) == "string")
assert(client:state() == 1)
join(rejected)
listener:close()

-- Unix socket 复用同一套读写及 TLS 升级逻辑；监听路径由调用者清理。
if package.config:sub(1, 1) == "/" then
    local path = os.tmpname()
    os.remove(path)
    local unix_listener = assert(Socket.listen_unix(path, 2000))
    upgrade_echo(unix_listener, function(socket) assert(socket:connect_unix(path) == 1) end)
    unix_listener:close()
    assert(os.remove(path))
end
print("socket 明文、TLS 升级、Unix、超时与取消验证通过")
