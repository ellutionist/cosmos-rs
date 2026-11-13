//! 内嵌 Lua 源码的运行时加载工具。

use crate::ffi::*;

/// 执行一段内嵌 Lua 源码（扩展的 Lua 部分经 include_str! 随二进制编译进来）。
/// 必须在 userdata 注册之后、用户脚本之前调用；源码只做定义，不得 yield
/// （主状态上用 lua_pcall，没有协程调度）。
///
/// # Safety
/// lua_state 必须属于存活的 VM，相关类型已注册；源码在主状态同步执行，不得 yield。
pub unsafe fn run_lua_source(
    lua_state: *mut lua_State,
    name: &str,
    src: &str,
) -> Result<(), String> {
    let cname = std::ffi::CString::new(name).map_err(|_| "source name contains NUL")?;

    if luaL_loadbuffer(
        lua_state,
        src.as_ptr() as *const c_char,
        src.len(),
        cname.as_ptr(),
    ) != 0
    {
        let msg = crate::stack::err_message(lua_state);
        lua_settop(lua_state, -2); // 弹出错误对象
        return Err(format!("{name}: {msg}"));
    }
    if lua_pcall(lua_state, 0, 0, 0) != 0 {
        let msg = crate::stack::err_message(lua_state);
        lua_settop(lua_state, -2);
        return Err(format!("{name}: {msg}"));
    }
    Ok(())
}
