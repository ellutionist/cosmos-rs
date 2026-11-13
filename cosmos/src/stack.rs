use crate::ffi::*;

/// 取栈顶（-1）的 Lua 错误信息
pub unsafe fn err_message(L: *mut lua_State) -> String {
    let mut len = 0usize;
    let p = lua_tolstring(L, -1, &mut len);
    if p.is_null() {
        "unknown lua error".to_string()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned()
    }
}

/// 把相对栈索引转为绝对索引；registry/upvalue 等伪索引保持不变。
pub unsafe fn abs_index(L: *mut lua_State, idx: c_int) -> c_int {
    if idx < 0 && idx > LUA_REGISTRYINDEX {
        lua_gettop(L) + idx + 1
    } else {
        idx
    }
}
