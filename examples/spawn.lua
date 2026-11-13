-- cosmos.thread：模仿 OpenResty ngx.thread 的协程管理
local hits = {}
-- unit 异步返回没有值，与 Option::None 的一个 nil 区分。
assert(select("#", cosmos.sleep(0)) == 0, "sleep should return zero values")
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

-- wait 失败仍保留三个返回值：nil、错误串、参数下标。
local waiting = cosmos.thread.spawn(function() cosmos.sleep(30) end)
local failed = cosmos.thread.spawn(function() error("wait failure") end)
local value, wait_err, failed_idx = cosmos.thread.wait(waiting, failed)
assert(value == nil and type(wait_err) == "string" and failed_idx == 2,
    "wait should return nil, error, argument index")
assert(wait_err:find("wait failure", 1, true), "wait should preserve error")
assert(select("#", waiting:join()) == 0, "successful join should return zero values")
print("wait error propagation ok:", failed_idx)

-- kill：取消挂起中的线程
local victim = cosmos.thread.spawn(function() cosmos.sleep(5000) end)
victim:kill()
local vok, verr = victim:join()
assert(vok == nil and type(verr) == "string", "killed join should report cancelled")
print("kill ok:", verr)

print("OK")
