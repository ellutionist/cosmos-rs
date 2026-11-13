-- cosmos.thread：模仿 OpenResty ngx.thread 的协程管理
local hits = {}
local t1 = cosmos.thread.spawn(function(x)
    cosmos.sleep(100)
    hits[#hits + 1] = "slow:" .. x
end, "a")
local t2 = cosmos.thread.spawn(function(x)
    cosmos.sleep(20)
    hits[#hits + 1] = "fast:" .. x
end, "b")
assert(t1:join() == nil, "join t1")
assert(t2:join() == nil, "join t2")
assert(hits[1] == "fast:b" and hits[2] == "slow:a",
    "spawn args/order: " .. table.concat(hits, ","))
print("spawn/join ok:", table.concat(hits, ","))

-- wait：任一结束返回参数下标，其余线程继续运行且之后仍可 join
local done = {}
local w1 = cosmos.thread.spawn(function() cosmos.sleep(120); done[#done + 1] = 1 end)
local w2 = cosmos.thread.spawn(function() cosmos.sleep(30); done[#done + 1] = 2 end)
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

-- kill：取消挂起中的线程
local victim = cosmos.thread.spawn(function() cosmos.sleep(5000) end)
victim:kill()
local vok, verr = victim:join()
assert(vok == nil and type(verr) == "string", "killed join should report cancelled")
print("kill ok:", verr)

print("OK")
