local port = tonumber(...)
assert(port, "usage: tcp_server.lua <port>")

local Tcp = require("cosmos.network.tcp.stream")

local srv = assert(Tcp.listen(port, 3000))
print("listening on", port)

-- accept 后每个连接交给一个轻线程处理；主协程继续 accept
local tasks = {}
for i = 1, 2 do
    local conn = assert(srv:accept())
    tasks[#tasks + 1] = cosmos.thread.spawn(function()
        local r, w = conn:split()
        local line, err = r:receive("*l")
        assert(line, "handler read: " .. tostring(err))
        -- 第一个连接故意慢，验证第二个连接不被串行阻塞
        if line == "slow" then
            cosmos.time.sleep(300)
        end
        w:write(line .. "\n")
        w:close()
    end)
end

for _, t in ipairs(tasks) do
    assert(t:join() == nil, "handler failed")
end
srv:close()
print("server done")
