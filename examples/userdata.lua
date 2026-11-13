local Counter = require("cosmos.Counter")

local c = Counter.new(0)
c:inc(5)
c:inc(2)
assert(c:get() == 7, "counter math failed")
print("counter value:", c:get())

-- 构造器带参数
local e = Counter.new(42)
assert(e:get() == 42, "constructor arg failed")
print("constructor arg ok:", e:get())

assert(c:greet("lua") == "hello, lua! counter = 7")
print(c:greet("lua"))

assert(c:try_div(2) == 3, "result method failed")
print("try_div ok:", c:try_div(2))

c:set(100)
assert(c:get() == 100)
print("after set:", c:get())

local d = c:dup()
assert(d:get() == 100, "dup failed")
print("dup value:", d:get())

-- 参数类型错误 → lua_error → pcall 捕获
local ok, err = pcall(function() c:inc("bad") end)
assert(not ok, "expected arg type error")
print("arg type error:", err)

-- 构造器参数类型错误
local ok2, err2 = pcall(function() Counter.new("x") end)
assert(not ok2, "expected constructor arg error")
print("constructor arg error:", err2)

-- Result 的 Err → nil + err（标准 Lua 形态）
local v3, err3 = c:try_div(0)
assert(v3 == nil, "expected nil on error")
assert(err3 == "division by zero", "expected err string, got " .. tostring(err3))
print("result nil+err:", v3, err3)

-- dup 出来的是独立对象
d:inc(1)
assert(c:get() == 100 and d:get() == 101, "dup must be independent")

print("OK")
