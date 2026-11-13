//! 内建时间功能：协程休眠与单调时钟。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::coroutine::yield_with;
use crate::ffi::*;

unsafe extern "C" fn sleep_impl(lua_state: *mut lua_State) -> c_int {
    crate::userdata::trampoline(lua_state, |lua_state| unsafe {
        if lua_gettop(lua_state) < 1 {
            return Err("sleep: expected 1 argument (ms)".to_string());
        }
        let ms = lua_tonumber(lua_state, 1);
        // 上限防止 Duration::from_secs_f64 溢出 panic（panic 穿过 extern "C" 会 abort）
        if ms < 0.0 || !ms.is_finite() || ms > 1e12 {
            return Err(format!("sleep: invalid duration {ms}"));
        }

        // 异步操作塞进 TLS 槽后 yield，由绑定的 LuaCoroutine future 取出并 poll（见 crate::coroutine）
        // 优化：改为每协程复用一个 tokio::time::Sleep（reset 到期时间），省掉 wheel entry 分配
        let fut = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
        });
        // 5.1 语义：lua_yield 返回 -1（= LUA_YIELD），必须作为 C 函数的返回值交给 VM，
        // VM 据此让 lua_resume 返回 LUA_YIELD（常规 yield 路径不长跳转）
        Ok(yield_with(lua_state, fut))
    })
}

static START: OnceLock<Instant> = OnceLock::new();

unsafe extern "C" fn now_ms_impl(lua_state: *mut lua_State) -> c_int {
    let start = START.get_or_init(Instant::now);
    lua_pushnumber(lua_state, start.elapsed().as_secs_f64() * 1000.0);
    1
}

/// 微秒级单调时钟（压测/单请求计时用，f64 在 2^53µs ≈ 285 年内精确）
unsafe extern "C" fn now_us_impl(lua_state: *mut lua_State) -> c_int {
    let start = START.get_or_init(Instant::now);
    lua_pushnumber(lua_state, start.elapsed().as_micros() as f64);
    1
}

/// 将时间函数注册到栈顶的 cosmos 表。
///
/// # Safety
/// lua_state 必须是当前线程上的有效状态，栈顶必须是可写入的 cosmos 表。
pub(super) unsafe fn register(lua_state: *mut lua_State) {
    lua_pushcclosure(lua_state, sleep_impl, 0);
    lua_setfield(lua_state, -2, c"sleep".as_ptr());
    lua_pushcclosure(lua_state, now_ms_impl, 0);
    lua_setfield(lua_state, -2, c"now_ms".as_ptr());
    lua_pushcclosure(lua_state, now_us_impl, 0);
    lua_setfield(lua_state, -2, c"now_us".as_ptr());
}
