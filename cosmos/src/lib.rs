//! 可嵌入的 Lua 运行环境；宿主入口为 Machine、Module 与 LightThread 句柄。
//! 原生插件通过 userdata 中的转换协议与宏注册类型，ffi 只公开必要的叶子转换和宏展开原语。
//! VM 生命周期、模块初始化、执行状态及字符串存储表示均由 crate 内部管理。

#![warn(unreachable_pub)]

extern crate self as cosmos;

mod builtin;
pub mod ffi;
#[cfg(test)]
mod libs;
mod light_thread;
mod machine;
mod stack;
pub mod userdata;

pub use cosmos_macros::{userdata, FromLua, PushLua};
pub use light_thread::LightThread;
pub use machine::{Machine, Module};
