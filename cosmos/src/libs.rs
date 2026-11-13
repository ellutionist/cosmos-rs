use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::coroutine::{yield_with, AsyncOutput};
use crate::ffi::*;
use libc::{c_char, c_int};

// lua_error 是 longjmp，返回值仅用于满足签名
pub(crate) unsafe fn raise(L: *mut lua_State, msg: &str) -> c_int {
    lua_pushlstring(L, msg.as_ptr() as *const c_char, msg.len());
    lua_error(L)
}

unsafe extern "C" fn sleep_impl(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return raise(L, "sleep: expected 1 argument (ms)");
    }
    let ms = lua_tonumber(L, 1);
    // 上限防止 Duration::from_secs_f64 溢出 panic（panic 穿过 extern "C" 会 abort）
    if ms < 0.0 || !ms.is_finite() || ms > 1e12 {
        return raise(L, &format!("sleep: invalid duration {ms}"));
    }

    // 异步操作塞进 TLS 槽后 yield，由绑定的 LuaCoroutine future 取出并 poll（见 coroutine.rs）
    // 优化：改为每协程复用一个 tokio::time::Sleep（reset 到期时间），省掉 wheel entry 分配
    let fut = Box::pin(async move {
        tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
        AsyncOutput::Nil
    });
    // 5.1 语义：lua_yield 返回 -1（= LUA_YIELD），必须作为 C 函数的返回值交给 VM，
    // VM 据此让 lua_resume 返回 LUA_YIELD（常规 yield 路径不长跳转）
    yield_with(L, fut)
}

static START: OnceLock<Instant> = OnceLock::new();

unsafe extern "C" fn now_ms_impl(L: *mut lua_State) -> c_int {
    let start = START.get_or_init(Instant::now);
    lua_pushnumber(L, start.elapsed().as_secs_f64() * 1000.0);
    1
}

pub unsafe fn open_cosmos(L: *mut lua_State) {
    lua_createtable(L, 0, 2);
    lua_pushcclosure(L, sleep_impl, 0);
    lua_setfield(L, -2, c"sleep".as_ptr());
    lua_pushcclosure(L, now_ms_impl, 0);
    lua_setfield(L, -2, c"now_ms".as_ptr());
    crate::thread::open(L);
    lua_setfield(L, -2, c"thread".as_ptr());
    lua_setfield(L, LUA_GLOBALSINDEX, c"cosmos".as_ptr());
}

/// 执行一段内嵌 Lua 源码（扩展的 Lua 部分经 include_str! 随二进制编译进来）。
/// 必须在 userdata 注册之后、用户脚本之前调用；源码只做定义，不得 yield
/// （主状态上用 lua_pcall，没有协程调度）。
pub unsafe fn run_lua_source(L: *mut lua_State, name: &str, src: &str) -> Result<(), String> {
    let cname = std::ffi::CString::new(name).map_err(|_| "source name contains NUL")?;

    if luaL_loadbuffer(L, src.as_ptr() as *const c_char, src.len(), cname.as_ptr()) != 0 {
        let msg = crate::stack::err_message(L);
        lua_settop(L, -2); // 弹出错误对象
        return Err(format!("{name}: {msg}"));
    }
    if lua_pcall(L, 0, 0, 0) != 0 {
        let msg = crate::stack::err_message(L);
        lua_settop(L, -2);
        return Err(format!("{name}: {msg}"));
    }
    Ok(())
}
