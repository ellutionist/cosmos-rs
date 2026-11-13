//! 可嵌入的 Lua 运行环境；宿主入口为 Machine、Module 与 LightThread 句柄。
//! 原生插件通过 userdata 中的受控值、输出和调用上下文注册类型；底层 ffi 完全私有。
//! VM 生命周期、模块初始化、执行状态及字符串存储表示均由 crate 内部管理。
//!
//! 底层 Lua C API 不属于外部接口：
//! ```compile_fail,E0603
//! use cosmos::ffi;
//! ```

#![warn(unreachable_pub)]

extern crate self as cosmos;

mod builtin;
mod ffi;
#[cfg(test)]
mod libs;
mod light_thread;
mod machine;
mod stack;
pub mod userdata;

pub use cosmos_macros::{userdata, FromLua, PushLua};
pub use light_thread::LightThread;
pub use machine::{Machine, Module};
