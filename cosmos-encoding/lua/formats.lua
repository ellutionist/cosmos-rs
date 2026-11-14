-- 公共形状标记与精确整数复用 JSON 的类型表，不重复注册 userdata。
local json = require('cosmos.encoding.json')
for _, name in ipairs({'toml', 'yaml'}) do
    local format = require('cosmos.encoding.' .. name)
    if not rawget(format, '_formats_loaded') then
        local native_decode = format.decode
        local array_meta = getmetatable(json.array())
        local object_meta = getmetatable(json.object())
        format.array = json.array
        format.object = json.object
        format.number = json.number
        local number_path = 'cosmos.encoding.' .. name .. '.number'
        package.preload[number_path] = function() return json.number end
        package.loaded[number_path] = json.number
        if name == 'yaml' then format.null = cosmos.null end
        function format.decode(text)
            return native_decode(text, array_meta, object_meta)
        end
        format._formats_loaded = true
    end
end
