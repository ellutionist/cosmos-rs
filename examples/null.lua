-- cosmos.null 是可保存的空值；nil 表示字段不存在。
local row = { id = 1, name = cosmos.null }
assert(row.name ~= nil and row.name == cosmos.null)
local values = { cosmos.null, "value", cosmos.null }
assert(#values == 3)
local channel = cosmos.sync.channel.new(1)
assert(channel:send(row))
assert(channel:recv().name == cosmos.null)
collectgarbage("collect")
assert(values[1] == cosmos.null and values[3] == cosmos.null)
print("NULL 字段、数组和通道验证通过")
