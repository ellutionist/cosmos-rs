-- UDP 客户端：发送一份二进制报文，并验证服务端的回显。
local port = tonumber(...)
assert(port, "usage: udp_client.lua <port>")

local Udp = require("cosmos.network.udp")
local client = assert(Udp.connect("127.0.0.1", port, 3000))
local payload = "hello\0udp\255"
assert(client:send(payload) == #payload)
local data, err = client:receive()
assert(data == payload, err or "UDP echo mismatch")
client:close()
print("UDP client send and receive completed")
