//! cosmos-network：基于 cosmos 运行时的网络库（tcp / tls / http / websocket）
//!
//! 分层：功能模块（tcp / tls / http / websocket）只描述协议与状态机；
//! 所有直接接触 lua_State 的代码集中在 bridge 模块。

mod bridge;
mod error;
mod http;
mod tcp;
mod websocket;

/// 本 crate 的 Lua 部分：编译期嵌入二进制，启动时由宿主执行（见 cosmos::libs::run_lua_source）。
/// (名称用于报错定位, 源码)
pub const LUA_SOURCES: &[(&str, &str)] = &[("cosmos.network.tcp", include_str!("../lua/tcp.lua"))];

/// 注册本 crate 的全部 userdata 类型（桥接层入口）
pub use bridge::register;
