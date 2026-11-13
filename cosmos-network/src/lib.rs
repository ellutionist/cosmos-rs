//! cosmos-network：基于 cosmos 运行时的网络库（tcp / tls / http / websocket）
//!
//! 分层：功能模块（tcp / tls / http / websocket）只描述协议与状态机；
//! bridge 仅组合 cosmos 的受控转换协议；本 crate 不接触 Lua C API 或状态指针。

#![warn(unreachable_pub)]

mod bridge;
mod error;
mod http;
mod tcp;
mod websocket;

/// 网络插件：注册协议类型，并加载编译进来的 Lua 定义。
pub struct Network;

impl cosmos::Module for Network {
    fn register(&self, machine: &mut cosmos::Machine) -> Result<(), String> {
        bridge::register(machine);
        machine.load("cosmos.network.tcp", include_str!("../lua/tcp.lua"))
    }
}
