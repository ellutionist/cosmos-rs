-- 共享形状元表，解码每个容器时不额外分配元表。
local json = require('cosmos.encoding.json')
if rawget(json, '_sonic_loaded') then return end
local native_decode = json.decode
local array_meta = {__jsontype='array'}
local object_meta = {__jsontype='object'}
json.null = cosmos.null
local function mark(value, meta)
    if value == nil then value = {} end
    if type(value) ~= 'table' then return nil, 'JSON 容器必须是 table' end
    local previous = getmetatable(value)
    if previous ~= nil and previous ~= array_meta and previous ~= object_meta then
        return nil, '表已有自定义元表，请显式设置 __jsontype 标记'
    end
    return setmetatable(value, meta)
end
function json.array(value) return mark(value, array_meta) end
function json.object(value) return mark(value, object_meta) end
function json.decode(text, options)
    return native_decode(text, array_meta, object_meta, options)
end
json._sonic_loaded = true
