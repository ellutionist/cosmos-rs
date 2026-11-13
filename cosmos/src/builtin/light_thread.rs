//! cosmos.thread 的 Lua 接口适配；执行、取消和完成通知统一由 LightThread 管理。

use crate::ffi::*;
use crate::light_thread::{yield_with, LightThread};
use crate::userdata::{push, register, trampoline, FromLua, Nil};

#[cosmos::userdata(name = "LightThread")]
impl LightThread {
    /// 拥有型句柄不跨等待占用 Lua userdata 借用，允许并发 join 和 kill。
    pub async fn join(self) -> Result<(), String> {
        self.completion().await
    }

    pub fn kill(&self) {
        self.cancel();
    }
}

/// cosmos.thread.spawn(fn, ...)：只准备 Lua 函数和参数，再交给统一执行入口。
unsafe extern "C" fn spawn_impl(lua_state: *mut lua_State) -> c_int {
    trampoline(lua_state, |lua_state| unsafe {
        if lua_type(lua_state, 1) != LUA_TFUNCTION {
            return Err("thread.spawn: expected a function".to_string());
        }
        let n = lua_gettop(lua_state);
        let thread = lua_newthread(lua_state);
        for i in 1..=n {
            lua_pushvalue(lua_state, i);
            lua_xmove(lua_state, thread, 1);
        }
        let thread = LightThread::new(lua_state, thread);
        thread.spawn();
        push(lua_state, thread);
        Ok(1)
    })
}

#[derive(cosmos::PushLua)]
enum WaitOutcome {
    Done(i64),
    Error(Nil, String, i64),
}

/// 任一结束即返回其参数下标；等待不消耗完成结果。
unsafe extern "C" fn wait_impl(lua_state: *mut lua_State) -> c_int {
    trampoline(lua_state, |lua_state| unsafe {
        let n = lua_gettop(lua_state);
        if n < 1 {
            return Err("thread.wait: expected at least one thread".to_string());
        }
        let mut threads = Vec::with_capacity(n as usize);
        for i in 1..=n {
            threads.push(LightThread::from_lua(lua_state, i)?);
        }
        Ok(yield_with(
            lua_state,
            Box::pin(async move {
                let (idx, result) = LightThread::wait_any(&threads).await;
                match result {
                    Ok(()) => WaitOutcome::Done(idx as i64),
                    Err(error) => WaitOutcome::Error(Nil, error, idx as i64),
                }
            }),
        ))
    })
}

/// 构造 cosmos.thread 表，由 builtin::open 挂到 cosmos 表下。
///
/// # Safety
/// lua_state 必须是当前线程上的有效状态，其栈可接受新建的模块表。
pub(super) unsafe fn open(lua_state: *mut lua_State) {
    lua_createtable(lua_state, 0, 2);
    lua_pushcclosure(lua_state, spawn_impl, 0);
    lua_setfield(lua_state, -2, c"spawn".as_ptr());
    lua_pushcclosure(lua_state, wait_impl, 0);
    lua_setfield(lua_state, -2, c"wait".as_ptr());
}

/// 注册统一的轻线程 userdata。
///
/// # Safety
/// lua_state 必须属于已初始化、已打开标准库的存活 VM，并由当前线程独占操作。
pub(super) unsafe fn register_types(lua_state: *mut lua_State) {
    register::<LightThread>(lua_state);
}
