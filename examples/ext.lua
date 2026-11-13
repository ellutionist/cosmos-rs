local Accumulator = require("cosmos.Accumulator")

-- 同步方法（来自独立扩展 crate）
local acc = Accumulator.new(0)
assert(acc:add(5) == 5, "sync add failed")
assert(acc:get() == 5, "sync get failed")
print("sync ok:", acc:get())

-- 异步 op（扩展里的 tokio，经主程序 yield 通道）
local t0 = cosmos.now_ms()
local r = cosmos.slow_add(200, 3, 4)
local dt = cosmos.now_ms() - t0
assert(r == 7, "slow_add result wrong: " .. r)
assert(dt >= 199, "slow_add returned too early: " .. dt)
print(string.format("slow_add ok: %d in %.1fms", r, dt))

-- 异步方法：await 后改 self（&mut self 守卫被 future 持有）
local t1 = cosmos.now_ms()
local r2 = acc:tick(150)
local dt1 = cosmos.now_ms() - t1
assert(r2 == 6, "async method wrong result: " .. r2)
assert(acc:get() == 6, "async method didn't mutate self")
assert(dt1 >= 149, "async method returned too early: " .. dt1)
print(string.format("async method ok: %d in %.1fms", r2, dt1))

-- &self 异步方法
local t2 = cosmos.now_ms()
local r3 = acc:peek_delayed(100)
assert(r3 == 6, "peek_delayed wrong: " .. r3)
assert(cosmos.now_ms() - t2 >= 99, "peek_delayed too early")
print("shared async ok:", r3)

-- 静态异步方法
local t3 = cosmos.now_ms()
local r4 = Accumulator.delayed_double(100, 21)
assert(r4 == 42, "delayed_double wrong: " .. r4)
assert(cosmos.now_ms() - t3 >= 99, "delayed_double too early")
print("static async ok:", r4)

print("OK")
