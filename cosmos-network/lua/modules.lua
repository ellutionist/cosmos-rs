-- 合成模块：tcp 命名空间入口。
-- require("cosmos.network.tcp") 返回 { stream = ..., tls = ... }，
-- 子模块仍可单独 require（cosmos.network.tcp.stream / cosmos.network.tcp.tls）。
-- 启动期由 host 执行（见 cosmos_network::LUA_SOURCES），此时各 userdata 的
-- package.preload 工厂已经注册完毕。
package.preload["cosmos.network.tcp"] = function()
    return {
        stream = require("cosmos.network.tcp.stream"),
        tls = require("cosmos.network.tcp.tls"),
    }
end
