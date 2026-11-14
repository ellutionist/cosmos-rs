-- 单进程 UDP 回归；所有端口由系统分配，也可传入 ::1 验证 IPv6。
local host = (...) or "127.0.0.1"
local Udp = require("cosmos.network.udp")
assert(Udp == cosmos.network.udp)
local function join(task)
    local _, err = task:join()
    assert(err == nil, err)
end
local function failure(expected, value, err)
    assert(value == nil and err == expected, tostring(err))
end

local server = assert(Udp.bind(host, 0, 1000))
local local_host, port = server:localaddr()
assert(local_host == host and port > 0 and server:state() == 0)
failure("not connected", server:peeraddr())
failure("not connected", server:send("no peer"))
local repeated, bind_error = Udp.bind(host, port, 1000)
assert(repeated == nil and bind_error:find("io:", 1, true))
local client = assert(Udp.connect(host, port, 1000))
local client_host, client_port = client:localaddr()
local peer_host, peer_port = client:peeraddr()
assert(peer_host == host and peer_port == port)
failure("already connected", client:sendto("wrong mode", host, port))
if host == "127.0.0.1" then
    assert(server:broadcast() == false)
    assert(server:setbroadcast(true) and server:broadcast() == true)
    assert(server:setbroadcast(false) and server:broadcast() == false)
end

-- 空报文为成功的空字符串；二进制跨短串复制与长串锚定阈值。
for _, data in ipairs({"hello", "", "a\0\255b", string.rep("\0\128\255x",1024), string.rep("\0\128\255x",2048)}) do
    assert(client:send(data) == #data)
    local packet = {server:receivefrom()}
    assert(#packet == 3 and packet[1] == data and packet[2] == client_host and packet[3] == client_port)
    assert(server:sendto(packet[1], packet[2], packet[3]) == #data)
    assert(client:receive() == data)
end

-- 报文边界互不合并；超限只丢弃当前报文，下一份仍完整可读。
assert(client:send("first") == 5 and client:send("second") == 6)
assert(server:receive() == "first" and server:receive() == "second")
assert(client:send("too long") == 8 and client:send("ok") == 2)
failure("buffer overflow", server:receive(2))
assert(server:state() == 0 and server:receive(2) == "ok")
failure("datagram too large", client:send(string.rep("x",65508)))
failure("datagram too large", server:sendto(string.rep("x",65508),host,client_port))

-- 已连接 socket 仅接收默认目标的报文，未连接 socket 可通过域名选择同族目标。
local stranger = assert(Udp.bind(host,0,1000))
assert(stranger:sendto("foreign",client_host,client_port) == 7)
local destination = host == "127.0.0.1" and "localhost" or host
assert(server:sendto("trusted",destination,client_port) == 7)
assert(client:receive() == "trusted")
stranger:close()

server:settimeout(10)
failure("timeout", server:receive())
assert(server:state() == 0)
assert(client:send("after timeout") == 13 and server:receive() == "after timeout")
server:settimeout(0)

-- 挂起的接收不会占用 userdata 借用，同一个句柄仍可发送及修改后续超时。
local started = false
local receiver = cosmos.thread.spawn(function()
    started = true
    assert(server:receive() == "duplex")
end)
while not started do cosmos.time.sleep(0) end
server:settimeout(10)
assert(server:sendto("while receiving",host,client_port) == 15)
assert(client:receive() == "while receiving")
failure("timeout", server:receive())
assert(client:send("duplex") == 6)
join(receiver)

-- 取消只取消当前等待，不关闭 socket，也不消费之后到达的报文。
server:settimeout(0)
started = false
local cancelled = cosmos.thread.spawn(function()
    started = true
    server:receivefrom()
    error("已取消的接收不能恢复")
end)
while not started do cosmos.time.sleep(0) end
cancelled:kill()
local _, cancel_error = cancelled:join()
assert(cancel_error and cancel_error:find("cancelled",1,true))
assert(client:send("after cancel") == 12 and server:receive() == "after cancel")

-- 非法数值参数不能截断、环绕或提前消费报文。
for _, number in ipairs({-1,65536,0.5,math.huge,0/0}) do
    local bound, err = Udp.bind(host,number)
    assert(bound == nil and err:find("invalid port",1,true))
    local connected, err = Udp.connect(host,number)
    assert(connected == nil and err:find("invalid port",1,true))
    local sent, err = server:sendto("invalid",host,number)
    assert(sent == nil and err:find("invalid port",1,true))
end
assert(client:send("preserved") == 9)
for _, size in ipairs({0,-1,0.5,65508,math.huge,0/0}) do
    local value, err = server:receivefrom(size)
    assert(value == nil and err:find("invalid receive size",1,true))
end
assert(server:receive() == "preserved")
for _, ms in ipairs({math.huge,0/0,1e13}) do
    local value, err = server:settimeout(ms)
    assert(value == nil and err:find("invalid timeout",1,true))
end
server:settimeout(-1e30)
local value, err = server:sendto("wrong family", host == "::1" and "127.0.0.1" or "::1", client_port)
assert(value == nil and err:find("no address matching",1,true))
local bad_bound, bad_bind_error = Udp.bind("\0",port,1000)
assert(bad_bound == nil and bad_bind_error:find("io:",1,true))
local bad_peer, bad_peer_error = Udp.connect("\0",port,1000)
assert(bad_peer == nil and bad_peer_error:find("io:",1,true))
local bad_send, bad_send_error = server:sendto("invalid", "\0", client_port)
assert(bad_send == nil and bad_send_error:find("io:",1,true))
assert(client:send("recovered") == 9 and server:receive() == "recovered")

-- 关闭必须唤醒所有接收者；所有句柄的后续操作均返回 closed。
local waiting, closed = 0, {}
for i=1,3 do
    closed[i] = cosmos.thread.spawn(function()
        waiting = waiting + 1
        failure("closed", server:receivefrom())
    end)
end
while waiting < 3 do cosmos.time.sleep(0) end
server:close()
server:close()
assert(server:state() == 1)
for _, task in ipairs(closed) do join(task) end
failure("closed", server:receive())
failure("closed", server:receivefrom())
failure("closed", server:sendto("closed",host,client_port))
failure("closed", server:send("closed"))
failure("closed", server:localaddr())
failure("closed", server:peeraddr())
failure("closed", server:settimeout(100))
failure("closed", server:setbroadcast(true))
failure("closed", server:broadcast())
local rebound = assert(Udp.bind(host,port,1000))
rebound:close()
client:close()

local gc_host, gc_port
do
    local temporary = assert(Udp.bind(host,0))
    gc_host, gc_port = temporary:localaddr()
end
collectgarbage("collect")
local collected = assert(Udp.bind(gc_host,gc_port,1000))
collected:close()
print("UDP "..host.." 报文边界、二进制、空报文、来源地址、超时、并发、取消及关闭验证通过")
