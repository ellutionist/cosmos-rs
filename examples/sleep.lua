local t0 = cosmos.now_ms()
print(string.format("start at %.2fms", t0))

cosmos.sleep(200)
local t1 = cosmos.now_ms()
local dt1 = t1 - t0
print(string.format("after first sleep: %.2fms elapsed", dt1))
assert(dt1 >= 199, "first sleep returned too early: " .. dt1)

cosmos.sleep(50)
local t2 = cosmos.now_ms()
print(string.format("after second sleep: %.2fms total", t2 - t0))
assert(t2 - t0 >= 249, "second sleep returned too early")

print("OK")
