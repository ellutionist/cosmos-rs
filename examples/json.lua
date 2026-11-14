-- sonic-rs JSON：普通表自动判别形状，空容器与精确数字可以显式指定。
local json = require('cosmos.encoding.json')
assert(json == cosmos.encoding.json and json.number == require('cosmos.encoding.json.number'))
assert(json.null == cosmos.null)
assert(json.encode(nil) == 'null' and json.encode(json.null) == 'null')
assert(json.encode(true) == 'true' and json.encode(false) == 'false')
assert(json.encode(42) == '42' and json.decode('42') == 42)
assert(json.decode('false') == false and json.decode('null') == cosmos.null)
local text = '\228\184\173\230\150\135😀\0\1\n\r\t"\\'
assert(json.decode(json.encode(text)) == text)
assert(json.decode('"\\ud83d\\ude00"') == '😀')
assert(json.decode('-0') == 0 and 1/json.decode('-0') == -math.huge)

assert(json.encode({}) == '{}')
assert(json.encode(json.array()) == '[]')
assert(json.encode(json.object()) == '{}')
assert(json.encode(json.decode('[]')) == '[]')
assert(json.encode(json.decode('{}')) == '{}')
local array = {}; array[3]=3; array[1]=1; array[2]=2
assert(json.encode(array) == '[1,2,3]')
assert(json.encode({1,cosmos.null,3}) == '[1,null,3]')
local document = assert(json.decode('{"array":[],"object":{},"null":null,"a\\u0000b":"value","nested":[true,false,null]}'))
assert(#document.nested == 3 and document.nested[3] == cosmos.null)
assert(document['a\0b'] == 'value' and document.null == cosmos.null)
local rebuilt = assert(json.decode(json.encode(document)))
assert(json.encode(rebuilt.array) == '[]' and json.encode(rebuilt.object) == '{}')
assert(json.decode('{"same":1,"same":2}').same == 2)
local reserved = {__jsontype='array', ['$sonic_rs::private::JsonNumber']='not a number'}
assert(json.decode(json.encode(reserved))['$sonic_rs::private::JsonNumber'] == 'not a number')

-- 默认仅将安全整数转换为 Lua number；大整数或溢出的浮点数保留数字文本。
assert(type(json.decode('9007199254740991')) == 'number')
for _, number in ipairs({'9007199254740992','18446744073709551615','-9223372036854775808','123456789012345678901234567890','1e400','1e-400'}) do
    local value = assert(json.decode(number))
    assert(type(value) == 'userdata' and tostring(value) == number and value:text() == number)
    assert(json.encode(value) == number)
end
assert(json.decode('1.25') == 1.25)
local exact = assert(json.number.new('1.2345678901234567890123456789'))
assert(json.encode({exact}) == '[1.2345678901234567890123456789]')
local precise = assert(json.decode('[1,1.2300,1e+03,-0]', {lossless_numbers=true}))
assert(type(precise[1]) == 'userdata')
assert(json.encode(precise) == '[1,1.2300,1e+03,-0]')
collectgarbage('collect')
assert(json.encode(exact) == exact:text())
for _, invalid in ipairs({'"123"','true','null','[1]','1 2','01','NaN','1e'}) do
    local value, err = json.number.new(invalid)
    assert(value == nil and type(err) == 'string')
end

local cyclic = {}; cyclic.self = cyclic
local indirect = {}; indirect.next = {parent=indirect}
local shared = {value=1}
local duplicate = assert(json.decode(json.encode({shared,shared})))
assert(duplicate[1].value == 1 and duplicate[2].value == 1)
for _, invalid in ipairs({cyclic, indirect, {[2]='hole'}, {[0]='zero'}, {[-1]='negative'},
                         {[1]='number',name='string'}, {[true]='boolean'},
                         json.object({1}), json.array({name='not an array'}),
                         function() end, cosmos.sync.mutex.new(), '\255', math.huge, -math.huge, 0/0}) do
    local value, err = json.encode(invalid)
    assert(value == nil and type(err) == 'string')
end
for _, invalid in ipairs({'',' ','{','[1,]','{"a":1,}','true false','01','"\\ud800"','"\255"','null\0'}) do
    local value, err = json.decode(invalid)
    assert(value == nil and type(err) == 'string')
end

-- 读取元表标签和表字段都不能执行用户元方法。
local methods = 0
local custom = setmetatable({value=1}, {__index=function() methods=methods+1; error('must not execute') end})
assert(json.decode(json.encode(custom)).value == 1 and methods == 0)
assert(json.array(custom) == nil)
assert(json.encode(setmetatable({}, {__jsontype='unknown'})) == nil)
assert(json.encode(setmetatable({}, {__jsontype='array'})) == '[]')

-- 深层容器需要运行时扩展 Lua 栈；超限后原 VM 应继续可用。
local deep_text = string.rep('[',64)..'0'..string.rep(']',64)
local deep = assert(json.decode(deep_text))
assert(json.encode(deep) == deep_text)
assert(json.decode(string.rep('[',65)..'0'..string.rep(']',65)) == nil)
assert(json.decode(string.rep('[',10000)..'0'..string.rep(']',10000)) == nil)
assert(json.number.new(string.rep('[',10000)..'0'..string.rep(']',10000)) == nil)
local empty_text = string.rep('[',64)..string.rep(']',64)
assert(json.encode(json.decode(empty_text)) == empty_text)
local object_text = string.rep('{"a":',64)..'null'..string.rep('}',64)
assert(json.encode(json.decode(object_text)) == object_text)
local quoted = '"'..string.rep('[{',100)..'\\\"'..string.rep('}[]',100)..'"'
assert(json.encode(json.decode(quoted)) == quoted)
local root, cursor = {}, nil; cursor = root
for i=1,64 do local child={}; cursor[1]=child; cursor=child end
assert(json.encode(root) == nil)
for i=1,100 do
    assert(json.encode(cyclic) == nil)
    assert(json.decode('[invalid]') == nil)
    assert(json.decode(json.encode({value='\230\129\162\229\164\141',array=json.array()})).value == '\230\129\162\229\164\141')
end
local channel = cosmos.sync.channel.new(1)
assert(channel:send(precise))
local received = assert(channel:recv())
assert(json.encode(received) == '[1,1.2300,1e+03,-0]')
print('JSON scalars, containers, NULL, exact numbers, invalid input and deep-stack recovery passed')
