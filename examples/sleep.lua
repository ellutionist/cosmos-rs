local time = require("cosmos.time")
assert(time == cosmos.time, "time 模块表与 require 不一致")
assert(type(time.sleep) == "function")
assert(type(time.now_ms) == "function" and type(time.now_us) == "function")

-- 参数错误在进入异步等待之前抛出，之后的调用仍可正常进行。
for _, invalid in ipairs({ -1, math.huge, -math.huge, 0/0, 1e12 + 1, "1", true, {} }) do
    assert(not pcall(time.sleep, invalid), "sleep 应拒绝非法时长")
end
assert(not pcall(time.sleep), "sleep 缺少参数应报错")
assert(not pcall(time.sleep, nil), "sleep 不接受 nil")
assert(select("#", time.sleep(0)) == 0, "sleep 成功应返回零个值")
assert(select("#", time.sleep(0.25)) == 0, "sleep 应接受小数毫秒")

-- now 返回真实系统时间，以 Lua 标准库的 Unix 秒时间戳校验单位和时间范围。
local seconds_before = os.time()
local ms = time.now_ms()
local us = time.now_us()
local seconds_after = os.time()
assert(ms % 1 == 0 and us % 1 == 0, "时间戳应为整数")
assert(ms >= seconds_before * 1000 and ms < (seconds_after + 1) * 1000,
    "now_ms 应返回当前 Unix 毫秒时间戳")
assert(us >= seconds_before * 1000000 and us < (seconds_after + 1) * 1000000,
    "now_us 应返回当前 Unix 微秒时间戳")

local t0 = time.now_ms()
print(string.format("start at %.2fms", t0))

time.sleep(200)
local t1 = time.now_ms()
local dt1 = t1 - t0
print(string.format("after first sleep: %.2fms elapsed", dt1))
assert(dt1 >= 199, "first sleep returned too early: " .. dt1)

time.sleep(50)
local t2 = time.now_ms()
print(string.format("after second sleep: %.2fms total", t2 - t0))
assert(t2 - t0 >= 249, "second sleep returned too early")

-- 休眠仍只挂起当前协程，长时长等待可被正常取消。
local sleeper = cosmos.thread.spawn(function() time.sleep(1e12) end)
time.sleep(0)
sleeper:kill()
local value, err = sleeper:join()
assert(value == nil and err:find("cancelled", 1, true), "sleep 取消失败")

print("OK")
