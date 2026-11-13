//! cosmos 内建模块：统一安装时间函数、轻线程和协程同步接口。

mod light_thread;
mod sync;
mod time;

use crate::ffi::*;
use crate::userdata::{push_module, register};

/// 初始化内建模块，并注册时间、LightThread 与同步原语类型。
///
/// # Safety
/// lua_state 必须属于已打开标准库并完成 userdata::init 的存活 VM，且由当前线程独占操作。
pub(crate) unsafe fn open(lua_state: *mut lua_State) {
    register::<time::Time>(lua_state);
    light_thread::register_types(lua_state);
    sync::register_types(lua_state);
    push_module(lua_state, "cosmos");
    light_thread::open(lua_state);
    lua_setfield(lua_state, -2, c"thread".as_ptr());
    lua_settop(lua_state, -2);
}
