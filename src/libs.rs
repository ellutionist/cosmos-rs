use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::coroutine::{yield_with, Output};
use crate::ffi::*;
use libc::{c_char, c_int};

// lua_error 是 longjmp，返回值仅用于满足签名
unsafe fn raise(L: *mut lua_State, msg: &str) -> c_int {
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
        Output::Nil
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
    lua_setfield(L, LUA_GLOBALSINDEX, c"cosmos".as_ptr());
}
