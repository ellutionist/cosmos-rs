-- 同一连接并发收发、接收取消/超时及关闭唤醒。
local Server = require('cosmos.network.http.server')
local Ws = require('cosmos.network.websocket')
local thread = cosmos.thread
local Channel = cosmos.sync.channel
local function noerr(_, err) assert(err == nil, err) end
local server = assert(Server.listen('127.0.0.1', 0, function()
    return {websocket=function(conn)
        while true do
            local op, data = conn:receive()
            if not op then return end
            noerr(conn:send(data))
        end
    end}
end))
local conn = assert(Ws.connect('ws://127.0.0.1:' .. server:port() .. '/'))
local replies = Channel.new(64)
local reader = thread.spawn(function()
    for _ = 1, 32 do
        local op, data = conn:receive()
        assert(op == 'text', data)
        assert(replies:send(data))
    end
end)
cosmos.time.sleep(10)
assert(conn:state() == 0)
local writers = {}
for i = 1, 32 do writers[i] = thread.spawn(function() noerr(conn:send(tostring(i))) end) end
for _, writer in ipairs(writers) do assert(writer:join() == nil) end
local seen = {}
for _ = 1, 32 do local value = assert(replies:recv(1000)); assert(not seen[value]); seen[value] = true end
assert(reader:join() == nil)
conn:settimeout(10)
local op, err = conn:receive()
assert(op == nil and err == 'timeout')
conn:settimeout(0)
local cancelled = thread.spawn(function() conn:receive() end)
cosmos.time.sleep(10)
cancelled:kill()
noerr(conn:ping('alive'))
noerr(conn:send('after cancellation'))
op, err = conn:receive()
assert(op == 'text' and err == 'after cancellation')
local closed = Channel.new(2)
local waiters = {}
for i = 1, 2 do
    waiters[i] = thread.spawn(function()
        local value, why = conn:receive()
        assert(value == nil and why == 'closed', why)
        assert(closed:send(true))
    end)
end
cosmos.time.sleep(10)
noerr(conn:close())
assert(closed:recv(1000) and closed:recv(1000))
for _, waiter in ipairs(waiters) do assert(waiter:join() == nil) end
assert(conn:state() == 1)
noerr(conn:close())
server:close()
print('WebSocket concurrent reads, writes, cancellation, timeouts and close passed')
