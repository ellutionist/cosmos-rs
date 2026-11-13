//! cosmos-network：基于 cosmos 运行时的网络库（tcp / tls / http）

mod error;
mod http;
mod tcp;
mod tls;

use cosmos::ffi::lua_State;

/// 本 crate 的 Lua 部分：编译期嵌入二进制，启动时由宿主执行（见 cosmos::libs::run_lua_source）。
/// (名称用于报错定位, 源码)
pub const LUA_SOURCES: &[(&str, &str)] = &[("cosmos.network.tcp", include_str!("../lua/tcp.lua"))];

pub unsafe fn register(L: *mut lua_State) {
    tcp::register(L);
    tls::register(L);
    http::register(L);
}
