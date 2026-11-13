-- cosmos-network 的 Lua 部分：随 crate 一起编译进二进制的混合方法示例。
--
-- require 拿到的是 Rust 侧生成的 metatable 本身（package.preload 工厂返回 mt，
-- __index 指向自己），所以这里定义的函数对实例直接可见，无需任何绑定层。

local Reader = require("cosmos.TcpReader")

-- 行迭代器：组合 Rust 实现的 receive("*l") 成泛型 for。
-- 干净 EOF 时 receive 返回 nil，迭代自然结束；按行组装这类控制流用 Lua 写更顺手。
function Reader:lines()
    return function()
        return self:receive("*l")
    end
end
