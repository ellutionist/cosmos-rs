extern crate self as cosmos;

pub mod builtin;
pub mod coroutine;
pub mod ffi;
pub mod libs;
mod machine;
pub mod stack;
pub mod userdata;

pub use cosmos_macros::{userdata, FromLua, PushLua};
pub use machine::{Machine, Module};
