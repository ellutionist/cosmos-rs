-- cosmos.thread：模仿 OpenResty ngx.thread 的协程管理
local hits = {}
-- unit 异步返回没有值，与 Option::None 的一个 nil 区分。
assert(select("#", cosmos.time.sleep(0)) == 0, "sleep should return zero values")
local t1 = cosmos.thread.spawn(function(x)
    cosmos.time.sleep(100)
    hits[#hits + 1] = "slow:" .. x
end, "a")
local t2 = cosmos.thread.spawn(function(x)
    cosmos.time.sleep(20)
    hits[#hits + 1] = "fast:" .. x
end, "b")
assert(t1:join() == nil, "join t1")
assert(t2:join() == nil, "join t2")
assert(hits[1] == "fast:b" and hits[2] == "slow:a",
    "spawn args/order: " .. table.concat(hits, ","))
print("spawn/join ok:", table.concat(hits, ","))

-- wait：任一结束返回参数下标，其余线程继续运行且之后仍可 join
local done = {}
local w1 = cosmos.thread.spawn(function() cosmos.time.sleep(120); done[#done + 1] = 1 end)
local w2 = cosmos.thread.spawn(function() cosmos.time.sleep(30); done[#done + 1] = 2 end)
local idx = cosmos.thread.wait(w1, w2)
assert(idx == 2, "wait index: " .. tostring(idx))
assert(w1:join() == nil, "join after wait")
assert(done[1] == 2 and done[2] == 1, "wait should not stop other threads")
print("wait ok:", idx)

-- 错误传播
local bad = cosmos.thread.spawn(function() error("boom") end)
local ok, err = bad:join()
assert(ok == nil and type(err) == "string", "join should report error")
print("error propagation ok:", err)

-- wait 失败仍保留三个返回值：nil、错误串、参数下标。
local waiting = cosmos.thread.spawn(function() cosmos.time.sleep(30) end)
local failed = cosmos.thread.spawn(function() error("wait failure") end)
local value, wait_err, failed_idx = cosmos.thread.wait(waiting, failed)
assert(value == nil and type(wait_err) == "string" and failed_idx == 2,
    "wait should return nil, error, argument index")
assert(wait_err:find("wait failure", 1, true), "wait should preserve error")
assert(select("#", waiting:join()) == 0, "successful join should return zero values")
print("wait error propagation ok:", failed_idx)

-- kill：取消挂起中的线程
local victim = cosmos.thread.spawn(function() cosmos.time.sleep(5000) end)
victim:kill()
local vok, verr = victim:join()
assert(vok == nil and type(verr) == "string", "killed join should report cancelled")
print("kill ok:", verr)

-- wait 不消耗结果：已完成任务可重复等待，未选中的任务也能继续 wait。
assert(cosmos.thread.wait(w1) == 1, "wait loser should remain observable")
assert(cosmos.thread.wait(w2) == 1, "completed task can be waited repeatedly")
local _, again_err = bad:join()
assert(again_err == err, "repeated join should preserve failure")
local _, repeated_err, repeated_idx = cosmos.thread.wait(failed)
assert(repeated_err == wait_err and repeated_idx == 1, "wait should preserve failure")

-- 多个 join 可同时等待同一任务；kill 不会与 join 的 userdata 借用冲突。
local shared = cosmos.thread.spawn(function() cosmos.time.sleep(60000) end)
local function observe()
    local value, error = shared:join()
    assert(value == nil and error:find("cancelled", 1, true), "shared join should see cancellation")
end
local observer1 = cosmos.thread.spawn(observe)
local observer2 = cosmos.thread.spawn(observe)
cosmos.time.sleep(1)
shared:kill()
assert(select("#", observer1:join()) == 0)
assert(select("#", observer2:join()) == 0)
assert(select("#", observer1:join()) == 0, "successful join is repeatable")
shared:kill() -- 完成后取消是幂等操作。

-- 取消等待者不消耗被等待任务的通知，也不影响其他观察者。
local target = cosmos.thread.spawn(function() cosmos.time.sleep(10) end)
local abandoned = cosmos.thread.spawn(function() cosmos.thread.wait(target) end)
cosmos.time.sleep(1)
abandoned:kill()
local _, abandoned_error = abandoned:join()
assert(abandoned_error:find("cancelled", 1, true))
assert(cosmos.thread.wait(target) == 1)
assert(select("#", target:join()) == 0)

-- 参数解析失败不能拿走前面有效参数的通知；重复传同一句柄也合法。
local pending = cosmos.thread.spawn(function() cosmos.time.sleep(5) end)
assert(not pcall(cosmos.thread.wait, pending, {}))
assert(cosmos.thread.wait(pending, pending) == 1)
assert(cosmos.thread.wait(pending) == 1)

-- 自己发起取消时，控制层不应与正在执行的 Lua 状态产生 RefCell 重入冲突。
local self_cancelled
self_cancelled = cosmos.thread.spawn(function()
    self_cancelled:kill()
    cosmos.time.sleep(0)
    error("cancelled task should not resume")
end)
local _, self_error = self_cancelled:join()
assert(self_error:find("cancelled", 1, true))

-- 丢弃 Lua 句柄不取消任务；任务自身仍持有执行资源。
local detached_done = false
cosmos.thread.spawn(function() cosmos.time.sleep(1); detached_done = true end)
collectgarbage("collect")
cosmos.time.sleep(10)
assert(detached_done, "GC should not cancel a detached task")

-- 连续立即完成的异步操作也要让出调度，不能饿死已就绪的后台任务。
local progressed = false
local ready = cosmos.thread.spawn(function() progressed = true end)
local queue = cosmos.sync.channel.new(1)
for i = 1, 300 do
    assert(queue:send(i))
    assert(queue:recv() == i)
end
assert(progressed, "ready task was starved by immediately ready operations")
assert(select("#", ready:join()) == 0)
print("shared lifecycle checks passed")

print("OK")
