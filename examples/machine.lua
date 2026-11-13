-- Machine 文件执行用例：确认模块注入、异步调用与 Lua chunk 参数。
local first, second = ...
assert(first == "host" and second == "参数", "Machine 参数传递失败")
local time = require("cosmos.time")
assert(time == cosmos.time)
local network = require("cosmos.network")
assert(network == cosmos.network)
assert(network.tcp.stream == require("cosmos.network.tcp.stream"))
assert(type(network.tcp.stream.reader.lines) == "function", "网络插件未加载 Lua 定义")
local channel = require("cosmos.sync.channel").new(1)
assert(channel:send("ready"))
assert(channel:recv() == "ready")
time.sleep(1)
print("Machine Lua checks passed")
