//! cosmos 内建模块：统一安装时间函数和轻线程接口。

pub mod light_thread;
mod time;

use crate::ffi::*;

/// 初始化内建模块，并注册 LightThread 类型。
///
/// # Safety
/// lua_state 必须属于已打开标准库并完成 userdata::init 的存活 VM，且由当前线程独占操作。
pub unsafe fn open(lua_state: *mut lua_State) {
    lua_createtable(lua_state, 0, 4);
    time::register(lua_state);
    light_thread::open(lua_state);
    lua_setfield(lua_state, -2, c"thread".as_ptr());
    lua_setfield(lua_state, LUA_GLOBALSINDEX, c"cosmos".as_ptr());
    light_thread::register_types(lua_state);
}
