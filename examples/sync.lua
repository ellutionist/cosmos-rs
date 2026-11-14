-- cosmos.sync：所有等待接口使用毫秒超时，缺省无限等待，0 只尝试立即完成。
local sync = require("cosmos.sync")
local spawn = cosmos.thread.spawn
assert(sync == cosmos.sync)
for _, name in ipairs({ "channel", "notify", "semaphore", "mutex", "permit" }) do
    assert(sync[name] == require("cosmos.sync." .. name), "submodule table differs from require")
end
assert(require("cosmos") == cosmos)
assert(getmetatable(sync.channel.new(1)) == sync.channel)

local checks = 0
local function check(value, message)
    assert(value, message)
    checks = checks + 1
end
local function failure(expected, value, err)
    check(value == nil and err == expected, tostring(err))
end
local function join(t)
    local _, err = t:join()
    check(err == nil, err)
end
local function cancel(t)
    t:kill()
    local value, err = t:join()
    check(value == nil and err:find("cancelled", 1, true), "cancellation failed")
end

-- channel：原始对象身份、二进制串、nil 与关闭错误可以区分。
do
    local ch = sync.channel.new(4)
    local object = { x = 42 }
    local fn = function() return "\229\155\158\232\176\131" end
    check(ch:send(object) == true)
    check(ch:send(fn) == true)
    check(ch:send("a\0b\255") == true)
    check(ch:send(nil) == true)
    check(ch:len() == 4)
    ch:close()
    ch:close()
    check(ch:recv() == object)
    check(ch:recv() == fn)
    check(ch:recv() == "a\0b\255")
    check(select("#", ch:recv()) == 1, "nil messages must return exactly one value")
    failure("closed", ch:recv())
    failure("closed", ch:send(1))
end

-- 有界背压：阻塞的 send 超时/取消后不能留下消息，缓冲区顺序保持 FIFO。
do
    local ch = sync.channel.new(1)
    check(ch:send(1, 0))
    failure("timeout", ch:send(2, 2))
    local sent = false
    local sender = spawn(function() check(ch:send(3)); sent = true end)
    cosmos.time.sleep(2)
    check(not sent)
    check(ch:recv() == 1)
    join(sender)
    check(ch:recv() == 3)
    failure("timeout", ch:recv(0))
    check(ch:send(4))
    local victim = spawn(function() ch:send(5) end)
    cosmos.time.sleep(2)
    cancel(victim)
    check(ch:recv() == 4)
    failure("timeout", ch:recv(1))

    local soon = spawn(function() check(ch:send("\231\168\141\229\144\142\229\136\176\232\190\190")) end)
    failure("timeout", ch:recv(0))
    join(soon)
    check(ch:recv(0) == "\231\168\141\229\144\142\229\136\176\232\190\190")
end

-- 同一个 userdata 上多个发送者/接收者可以并发挂起，close 唤醒所有等待者。
do
    local ch, received, workers = sync.channel.new(2), {}, {}
    for i = 1, 8 do
        workers[#workers + 1] = spawn(function()
            local value = assert(ch:recv())
            check(not received[value], "duplicate receive")
            received[value] = true
        end)
    end
    cosmos.time.sleep(2)
    for i = 1, 8 do
        workers[#workers + 1] = spawn(function() check(ch:send(i)) end)
    end
    for _, worker in ipairs(workers) do join(worker) end
    for i = 1, 8 do check(received[i]) end

    workers = {}
    for i = 1, 3 do
        workers[i] = spawn(function() failure("closed", ch:recv()) end)
    end
    cosmos.time.sleep(2)
    ch:close()
    for _, worker in ipairs(workers) do join(worker) end

    ch, workers = sync.channel.new(1), {}
    check(ch:send(1))
    for i = 1, 3 do
        workers[i] = spawn(function() failure("closed", ch:send(i)) end)
    end
    cosmos.time.sleep(2)
    ch:close()
    for _, worker in ipairs(workers) do join(worker) end
    check(ch:recv() == 1)
end

-- registry 保活队列和等待发送的临时值，消费/取消后释放锚点。
do
    local ch = sync.channel.new(1)
    local weak = setmetatable({}, { __mode = "v" })
    local function put()
        local object = { 123 }
        weak[1] = object
        check(ch:send(object))
    end
    put()
    collectgarbage("collect")
    check(weak[1] ~= nil, "queued value was collected too early")
    local function take()
        check(ch:recv() == weak[1])
    end
    take()
    collectgarbage("collect")
    check(weak[1] == nil, "anchor was not released after consuming the value")
    check(ch:send(true))
    local function pending_send()
        local object = { 456 }
        weak[2] = object
        check(ch:send(object))
    end
    local worker = spawn(pending_send)
    cosmos.time.sleep(2)
    collectgarbage("collect")
    check(weak[2] ~= nil)
    check(ch:recv() == true)
    join(worker)
    check(ch:recv() == weak[2])

    local mutex = sync.mutex.new()
    check(ch:send(mutex))
    check(ch:recv() == mutex, "userdata identity was lost")

    check(ch:send(true))
    local victim = spawn(function()
        local object = { 789 }
        weak[3] = object
        ch:send(object)
    end)
    cosmos.time.sleep(2)
    check(weak[3] ~= nil)
    cancel(victim)
    collectgarbage("collect")
    check(weak[3] == nil, "anchor was not released after cancelling send")
    check(ch:recv() == true)
end

-- notify_one 最多保存一个信号；notify_all 只广播给已经等待的协程。
do
    local event = sync.notify.new()
    check(event:notify_one())
    check(event:notify_one())
    check(event:wait(0))
    failure("timeout", event:wait(1))
    check(event:notify_all())
    failure("timeout", event:wait(1))
    local hits, workers = 0, {}
    for i = 1, 3 do
        workers[i] = spawn(function() check(event:wait()); hits = hits + 1 end)
    end
    cosmos.time.sleep(2)
    check(event:notify_one())
    cosmos.time.sleep(2)
    check(hits == 1)
    check(event:notify_all())
    for _, worker in ipairs(workers) do join(worker) end
    check(hits == 3)
    for i = 1, 3 do
        workers[i] = spawn(function() failure("closed", event:wait()) end)
    end
    cosmos.time.sleep(2)
    event:close()
    for _, worker in ipairs(workers) do join(worker) end
    failure("closed", event:wait(0))
    failure("closed", event:notify_one())
    failure("closed", event:notify_all())
end

-- 已通知但尚未恢复的等待者被取消，通知仍应交给下一个等待者。
do
    local event = sync.notify.new()
    local victim = spawn(function() event:wait() end)
    local survivor = spawn(function() check(event:wait(100)) end)
    cosmos.time.sleep(2)
    check(event:notify_one())
    victim:kill()
    join(survivor)
    cancel(victim)
end

-- semaphore：限制并发数量、公平排队、超时和取消不会消耗配额。
do
    local sem = sync.semaphore.new(2)
    local active, peak, workers = 0, 0, {}
    for i = 1, 8 do
        workers[i] = spawn(function()
            local permit = assert(sem:acquire())
            active = active + 1
            peak = math.max(peak, active)
            cosmos.time.sleep(2)
            active = active - 1
            permit:release()
            permit:release()
        end)
    end
    for _, worker in ipairs(workers) do join(worker) end
    check(peak == 2 and sem:available() == 2)
    local a, b = assert(sem:acquire()), assert(sem:acquire())
    failure("timeout", sem:acquire(1))
    local victim = spawn(function() sem:acquire() end)
    cosmos.time.sleep(2)
    cancel(victim)
    a:release()
    b:release()
    check(sem:available() == 2)
    local function abandoned_permit() assert(sem:acquire()) end
    abandoned_permit()
    collectgarbage("collect")
    check(sem:available() == 2, "GC must return the permit")
    a, b = assert(sem:acquire()), assert(sem:acquire())
    local waiter = spawn(function() failure("closed", sem:acquire()) end)
    cosmos.time.sleep(2)
    sem:close()
    join(waiter)
    a:release()
    b:release()
    failure("closed", sem:acquire(0))
end

-- mutex：临界区允许 yield，多协程排队；显式锁守卫可重复释放。
do
    local mutex, total, workers = sync.mutex.new(), 0, {}
    for i = 1, 6 do
        workers[i] = spawn(function()
            for j = 1, 5 do
                check(mutex:with_lock(function()
                    local value = total
                    cosmos.time.sleep(0)
                    total = value + 1
                    return true
                end))
            end
        end)
    end
    for _, worker in ipairs(workers) do join(worker) end
    check(total == 30, "mutual exclusion failed")
    local guard = assert(mutex:lock(0))
    failure("timeout", mutex:lock(1))
    local ran = false
    failure("timeout", mutex:with_lock(function() ran = true end, 1))
    check(not ran)
    guard:release()
    guard:release()
    guard = assert(mutex:lock())
    guard:release()
    check(select("#", mutex:with_lock(function() end)) == 1)
    local object = {}
    check(mutex:with_lock(function() return object end) == object)
    local value, err = mutex:with_lock(function() error("error inside lock") end)
    check(value == nil and err:find("error inside lock", 1, true))
    assert(mutex:lock(0)):release()

    local entered = sync.channel.new(1)
    local holder = spawn(function()
        mutex:with_lock(function()
            check(entered:send(true))
            cosmos.time.sleep(10000)
        end)
    end)
    check(entered:recv())
    cancel(holder)
    -- 取消 with_lock 无需等待 GC 就应释放锁。
    assert(mutex:lock(0)):release()
    local waiting_guard = assert(mutex:lock())
    local waiter = spawn(function() mutex:lock() end)
    cosmos.time.sleep(2)
    cancel(waiter)
    waiting_guard:release()
    assert(mutex:lock(0)):release()
end

-- 参数严格校验；失败不能污染后续调用或留下借用。
do
    for _, value in ipairs({ 0, -1, 1.5, math.huge, 0/0, 4294967296, "1" }) do
        check(not pcall(sync.channel.new, value))
        check(not pcall(sync.semaphore.new, value))
    end
    local ch = sync.channel.new(1)
    check(not pcall(function() ch:send() end))
    for _, value in ipairs({ -1, math.huge, 0/0, "1" }) do
        check(not pcall(function() ch:recv(value) end))
    end
    check(ch:send("\228\187\141\231\132\182\229\143\175\231\148\168"))
    check(ch:recv() == "\228\187\141\231\132\182\229\143\175\231\148\168")
end

-- 异步拥有型接收者不依赖原 userdata 保活；VM 退出时回收未结束的等待任务。
do
    local weak = setmetatable({}, { __mode = "v" })
    local ready = sync.channel.new(1)
    local worker = spawn(function()
        local function temporary_channel()
            local ch = sync.channel.new(1)
            weak[1] = ch
            return ch
        end
        ready:send(true)
        failure("timeout", temporary_channel():recv(20))
    end)
    check(ready:recv())
    collectgarbage("collect")
    check(weak[1] == nil, "wait methods must not hold the original userdata borrow")
    join(worker)
    spawn(function() sync.channel.new(1):recv() end)
    spawn(function() sync.notify.new():wait() end)
    cosmos.time.sleep(2)
end

print("all sync checks passed:", checks)
