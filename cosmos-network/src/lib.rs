//! cosmos-network：基于 cosmos 运行时的网络库（tcp 起步）

mod tcp;

use cosmos::ffi::lua_State;

pub unsafe fn register(L: *mut lua_State) {
    tcp::register(L);
}
