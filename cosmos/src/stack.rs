use crate::ffi::*;

/// 取栈顶（-1）的 Lua 错误信息
///
/// # Safety
/// lua_state 必须是当前线程上的有效状态，栈顶保留待读取的 Lua 错误值。
pub unsafe fn err_message(lua_state: *mut lua_State) -> String {
    let mut len = 0usize;
    let p = lua_tolstring(lua_state, -1, &mut len);
    if p.is_null() {
        "unknown lua error".to_string()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned()
    }
}

/// 把相对栈索引转为绝对索引；registry/upvalue 等伪索引保持不变。
///
/// # Safety
/// lua_state 必须是当前线程上的有效状态，idx 是栈索引或合法伪索引。
pub unsafe fn abs_index(lua_state: *mut lua_State, idx: c_int) -> c_int {
    if idx < 0 && idx > LUA_REGISTRYINDEX {
        lua_gettop(lua_state) + idx + 1
    } else {
        idx
    }
}
