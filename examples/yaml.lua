-- YAML 配置、空值、锚点及受限解析。
local yaml = require("cosmos.encoding.yaml")
local json = require("cosmos.encoding.json")
assert(yaml == cosmos.encoding.yaml and yaml.null == cosmos.null)
assert(yaml.number == json.number)
assert(yaml.number == require("cosmos.encoding.yaml.number"))
package.loaded['cosmos.encoding.yaml.number'] = nil
assert(yaml.number == require("cosmos.encoding.yaml.number"))
local config = assert(yaml.decode([[
title: Sample configuration
enabled: true
ports: [80, 443]
empty_array: []
empty_object: {}
nothing: null
maximum: 18446744073709551615
minimum: -9223372036854775808
defaults: &defaults
  host: localhost
  port: 1234
service:
  <<: *defaults
  port: 5678
text: |
  first
  second
]]))
assert(config.title == 'Sample configuration' and config.enabled and config.ports[2] == 443)
assert(config.nothing == cosmos.null and config.text == 'first\nsecond\n')
assert(config.service.host == 'localhost' and config.service.port == 5678)
assert(config.maximum:text() == '18446744073709551615' and config.minimum:text() == '-9223372036854775808')
local rebuilt = assert(yaml.decode(yaml.encode(config)))
assert(rebuilt.maximum:text() == config.maximum:text() and rebuilt.minimum:text() == config.minimum:text())
assert(json.encode(rebuilt.empty_array) == '[]' and json.encode(rebuilt.empty_object) == '{}')
assert(rebuilt.nothing == cosmos.null and rebuilt.service.port == 5678)
assert(yaml.decode('null') == cosmos.null and yaml.decode('~') == cosmos.null)
assert(yaml.decode('false') == false and yaml.decode('') == cosmos.null)
assert(yaml.decode('yes') == 'yes' and yaml.decode('off') == 'off')
assert(yaml.decode('"18446744073709551616"') == '18446744073709551616')
assert(yaml.decode('!!str 18446744073709551616') == '18446744073709551616')
assert(yaml.decode('!!int "18446744073709551616"') == nil)
assert(yaml.decode('!!float .inf') == nil)
assert(yaml.decode('!!int "9007199254740992"'):text() == '9007199254740992')
assert(yaml.decode(yaml.encode({['<<']='literal'}))['<<'] == 'literal')
assert(yaml.decode('0x10') == 16 and yaml.decode('0o10') == 8)
assert(yaml.decode(yaml.encode('123')) == '123')
assert(yaml.decode('text: "${HOME}"').text == '${HOME}')
assert(json.encode(yaml.decode(yaml.encode(yaml.array()))) == '[]')
assert(json.encode(yaml.decode(yaml.encode(yaml.object()))) == '{}')
assert(yaml.decode(yaml.encode('\228\184\173\230\150\135\0\1\n\r\t"\\')) == '\228\184\173\230\150\135\0\1\n\r\t"\\')
assert(yaml.decode(yaml.encode({1,cosmos.null,3}))[2] == cosmos.null)

for _, invalid in ipairs({'x: [1,','a: 1\na: 2','1: numeric-key','true: boolean-key','? [a,b]\n: complex-key','!unknown value','&a [*a]','*missing','---\na\n---\nb','"\255"','.nan','.inf','1e999','18446744073709551616','-9223372036854775809'}) do
    local value, err = yaml.decode(invalid)
    assert(value == nil and type(err) == 'string', invalid)
end
local cyclic = {}; cyclic.next=cyclic
for _, invalid in ipairs({cyclic, {[1]=1,name='mixed'}, {[2]='hole'}, {[true]=1}, function() end, '\255', math.huge, -math.huge, 0/0, yaml.number.new('1.234567890123456789'), yaml.number.new('18446744073709551616')}) do
    local value, err = yaml.encode(invalid)
    assert(value == nil and type(err) == 'string')
end
local deep = string.rep('[',64)..'0'..string.rep(']',64)
assert(yaml.encode(yaml.decode(deep)))
assert(yaml.decode(string.rep('[',65)..'0'..string.rep(']',65)) == nil)
assert(yaml.decode(string.rep('[',10000)..'0'..string.rep(']',10000)) == nil)
assert(yaml.decode('base: &a [1]\nrefs: ['..string.rep('*a,',10000)..'*a]') == nil)
for i=1,25 do
    assert(yaml.decode('bad: [') == nil and yaml.encode(cyclic) == nil)
    assert(yaml.decode(yaml.encode({value='\230\129\162\229\164\141',array=yaml.array()})).value == '\230\129\162\229\164\141')
end
print('YAML configuration, NULL, exact integers, anchor merges, containers, resource limits and error recovery passed')
