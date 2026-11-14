-- 编解码插件：二进制安全，错误返回 nil + 错误字符串。
local encoding = require("cosmos.encoding")
local hex = require("cosmos.encoding.hex")
local base64 = require("cosmos.encoding.base64")
local uri = require("cosmos.encoding.uri")
assert(encoding == cosmos.encoding and encoding.hex == hex)
assert(encoding.base64 == base64 and encoding.uri == uri)

local bytes = {}
for i = 0, 255 do bytes[#bytes + 1] = string.char(i) end
local binary = table.concat(bytes)
for _, codec in ipairs({ hex, base64, uri }) do
    assert(codec.decode(codec.encode(binary)) == binary)
    assert(codec.decode(codec.encode("")) == "")
end
assert(hex.encode("a\0b\255") == "610062ff")
assert(hex.decode("FF00") == "\255\0")
for plain, encoded in pairs({ [""] = "", f = "Zg==", fo = "Zm8=", foo = "Zm9v", foob = "Zm9vYg==", fooba = "Zm9vYmE=", foobar = "Zm9vYmFy" }) do
    assert(base64.encode(plain) == encoded)
    assert(base64.decode(encoded) == plain)
end
assert(base64.encode_urlsafe("\251\255") == "-_8")
assert(base64.decode_urlsafe("-_8") == "\251\255")
assert(uri.encode("AZaz09-._~") == "AZaz09-._~")
assert(uri.encode("a b+c/\0") == "a%20b%2Bc%2F%00")
assert(uri.decode("a+b") == "a+b")
for _, case in ipairs({ {hex, "0"}, {hex, "gg"}, {base64, "!"}, {base64, "Zg"}, {uri, "%"}, {uri, "%G0"} }) do
    local value, err = case[1].decode(case[2])
    assert(value == nil and type(err) == "string")
end
print("Hex、Base64、URI 的标准向量、二进制与错误处理通过")
