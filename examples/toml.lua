-- TOML 配置编解码、类型保留与错误恢复。
local toml = require("cosmos.encoding.toml")
local json = require("cosmos.encoding.json")
assert(toml == cosmos.encoding.toml)
assert(toml.datetime == require("cosmos.encoding.toml.datetime"))
assert(toml.number == json.number)
assert(toml.number == require("cosmos.encoding.toml.number"))
package.loaded['cosmos.encoding.toml.number'] = nil
assert(toml.number == require("cosmos.encoding.toml.number"))

local config = assert(toml.decode([=[
title = "Sample configuration"
enabled = true
ports = [80, 443]
empty_array = []
empty_table = {}
maximum = 9223372036854775807
minimum = -9223372036854775808
created = 2026-11-14T12:34:56+08:00
local_time = 12:34:56.123
date = 2026-11-14
local_date_time = 2026-11-14T12:34:56
[database]
host = "localhost"
[[workers]]
name = "one"
[[workers]]
name = "two"
]=]))
assert(config.title == "Sample configuration" and config.enabled and config.ports[2] == 443)
assert(config.database.host == "localhost" and config.workers[2].name == "two")
assert(json.encode(config.empty_array) == '[]' and json.encode(config.empty_table) == '{}')
assert(type(config.maximum) == 'userdata' and config.maximum:text() == '9223372036854775807')
assert(tostring(config.minimum) == '-9223372036854775808')
assert(type(config.created) == 'userdata' and tostring(config.created) == '2026-11-14T12:34:56+08:00')
assert(config.local_time:text() == '12:34:56.123' and config.date:text() == '2026-11-14')
local rebuilt = assert(toml.decode(toml.encode(config)))
assert(rebuilt.maximum:text() == config.maximum:text() and rebuilt.created:text() == config.created:text())
assert(rebuilt.local_date_time:text() == config.local_date_time:text())
assert(json.encode(rebuilt.empty_array) == '[]' and json.encode(rebuilt.empty_table) == '{}')

local native = {when=assert(toml.datetime.new('2026-11-14')), text='2026-11-14', array=toml.array(), object=toml.object()}
local roundtrip = assert(toml.decode(toml.encode(native)))
assert(type(roundtrip.when) == 'userdata' and type(roundtrip.text) == 'string')
assert(toml.datetime.new('not a date') == nil)
assert(toml.encode({number=toml.number.new('18446744073709551615')}) == nil)
assert(toml.encode({number=toml.number.new('1.234567890123456789')}) == nil)
assert(json.encode(native.when) == nil)
local reserved = assert(toml.decode('"$__toml_private_datetime"="ordinary"'))
assert(reserved['$__toml_private_datetime'] == 'ordinary')
assert(toml.decode(toml.encode(reserved))['$__toml_private_datetime'] == 'ordinary')
assert(toml.encode({}) == '' and type(toml.decode('')) == 'table')
local floats = assert(toml.decode('pos=inf\nneg=-inf\nnan=nan\nvalue=1.25'))
assert(floats.pos == math.huge and floats.neg == -math.huge and floats.nan ~= floats.nan and floats.value == 1.25)
local floats_again = assert(toml.decode(toml.encode(floats)))
assert(floats_again.pos == math.huge and floats_again.nan ~= floats_again.nan)

local cyclic = {}; cyclic.self=cyclic
for _, invalid in ipairs({cyclic, {null=cosmos.null}, {array={1,cosmos.null}}, {bad=function() end}, {text='\255'}, {[true]=1}, {[1]=1,name='mixed'}, {[2]='hole'}, toml.array(), true, 'root'}) do
    local value, err = toml.encode(invalid)
    assert(value == nil and type(err) == 'string')
end
assert(toml.encode(nil) == nil)
for _, invalid in ipairs({'a=','a=1\na=2','a=9223372036854775808','a="\255"','a=null','a=[1,2','a="unterminated'}) do
    local value, err = toml.decode(invalid)
    assert(value == nil and type(err) == 'string')
end
local deepest = 'a='..string.rep('[',63)..'0'..string.rep(']',63)
assert(toml.encode(toml.decode(deepest)))
assert(toml.decode('a='..string.rep('[',64)..'0'..string.rep(']',64)) == nil)
assert(toml.decode('a='..string.rep('[',10000)..'0'..string.rep(']',10000)) == nil)
assert(toml.decode(string.rep('a.',10000)..'x=1') == nil)
for i=1,25 do
    assert(toml.decode('invalid=') == nil and toml.encode(cyclic) == nil)
    assert(toml.decode(toml.encode({value='\230\129\162\229\164\141',array=toml.array()})).value == '\230\129\162\229\164\141')
end
print('TOML configuration, datetimes, exact integers, containers, depth limits and error recovery passed')
