-- UDP 回显服务端：先启动，再运行传入同一端口的 udp_client.lua。
local port = tonumber(...)
assert(port, "用法：udp_server.lua <端口>")

local Udp = require("cosmos.network.udp")
local server = assert(Udp.bind("127.0.0.1", port, 10000))
local host, number = server:localaddr()
print("UDP 监听", host, number)

local data, from_host, from_port = server:receivefrom()
assert(data, from_host)
assert(server:sendto(data, from_host, from_port) == #data)
server:close()
print("UDP 服务端回显完成")
