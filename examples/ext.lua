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

print("OK")
