//! 协程管理（模仿 OpenResty ngx.thread）
//!
//! Lua 侧：
//!   local t = cosmos.thread.spawn(fn, ...)  -- 启动一个轻线程
//!   t:join()                                -- 等待结束；失败返回 nil + err
//!   t:kill()                                -- 取消（对应 ngx.thread.kill）
//!   cosmos.thread.wait(t1, t2, ...)         -- 等任一结束，返回其参数下标；
//!                                             失败返回 nil, err, 下标
//!
//! 每个轻线程是一个 lua_State 协程 + 一个 tokio 本地任务：
//! 协程挂起时把待处理的 future 交给 LuaCoroutine 驱动（见 coroutine.rs），
//! 多个任务在同一个 current_thread runtime 的 LocalSet 上并发推进。
//! 未 join 的线程出错会打印到 stderr（避免静默丢失）。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::coroutine::{yield_with, AsyncOutput, LuaCoroutine};
use crate::ffi::*;
use crate::libs::raise;
use crate::userdata::{check, push, register};

/// 轻线程句柄（Lua userdata）
pub struct Thread {
    /// join / kill 用；join 之后 take 掉
    handle: Option<JoinHandle<Result<(), String>>>,
    /// wait 用：任务完成时发送一次结果（kill 会让发送端随任务一起 drop）
    done: Option<oneshot::Receiver<Result<(), String>>>,
}

#[cosmos::userdata]
impl Thread {
    /// 等待线程结束；线程内 Lua 错误通过 nil + err 返回
    async fn join(&mut self) -> Result<(), String> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        match handle.await {
            Ok(res) => res,
            Err(e) => Err(format!("thread: {e}")),
        }
    }

    /// 取消线程（对应 ngx.thread.kill）；之后 join 返回 cancelled
    fn kill(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

/// cosmos.thread.spawn(fn, ...)：把 fn 及其参数搬到新 lua_State 协程上执行
unsafe extern "C" fn spawn_impl(L: *mut lua_State) -> c_int {
    if lua_type(L, 1) != LUA_TFUNCTION {
        return raise(L, "thread.spawn: expected a function");
    }
    let n = lua_gettop(L);
    let thread = lua_newthread(L); // 线程对象压栈，new() 里 luaL_ref 会取走
    for i in 1..=n {
        lua_pushvalue(L, i);
        lua_xmove(L, thread, 1);
    }
    // 新协程栈：[fn, args...]，Fresh 阶段按 gettop-1 传参
    let co = LuaCoroutine::new(L, thread);
    let (tx, done) = oneshot::channel();
    let handle = tokio::task::spawn_local(async move {
        let res = co.await;
        if let Err(e) = &res {
            eprintln!("cosmos.thread: unhandled error: {e}");
        }
        let _ = tx.send(res.clone());
        res
    });
    push(L, Thread {
        handle: Some(handle),
        done: Some(done),
    });
    1
}

/// 轮询一组完成信号，返回第一个结束的（下标从 1 开始，与 wait 的参数位置一致）。
/// 只用 oneshot 信号、不碰 JoinHandle，所以 wait 之后线程仍可 join。
struct WaitAny {
    done: Vec<(usize, oneshot::Receiver<Result<(), String>>)>,
}

impl Future for WaitAny {
    type Output = (usize, Result<(), String>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut i = 0;
        while i < this.done.len() {
            match Pin::new(&mut this.done[i].1).poll(cx) {
                Poll::Ready(Ok(res)) => {
                    let (idx, _) = this.done.swap_remove(i);
                    return Poll::Ready((idx, res));
                }
                // 发送端随任务被 abort 一起 drop：视为已取消
                Poll::Ready(Err(_)) => {
                    let (idx, _) = this.done.swap_remove(i);
                    return Poll::Ready((idx, Err("thread: cancelled".to_string())));
                }
                Poll::Pending => i += 1,
            }
        }
        Poll::Pending
    }
}

/// cosmos.thread.wait(t1, t2, ...)：任一结束即返回其参数下标；其余线程继续运行
unsafe extern "C" fn wait_impl(L: *mut lua_State) -> c_int {
    let n = lua_gettop(L);
    if n < 1 {
        return raise(L, "thread.wait: expected at least one thread");
    }
    let mut done = Vec::with_capacity(n as usize);
    for i in 1..=n {
        let mut thread = check::<Thread>(L, i); // 类型不符直接 lua_error
        let Some(rx) = thread.done.take() else {
            return raise(L, "thread.wait: thread already waited");
        };
        done.push((i as usize, rx));
    }
    yield_with(
        L,
        Box::pin(async move {
            let (idx, res) = WaitAny { done }.await;
            match res {
                Ok(()) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                    lua_pushinteger(L, idx as lua_Integer);
                    1
                })),
                Err(err) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                    lua_pushnil(L);
                    lua_pushlstring(L, err.as_ptr() as *const c_char, err.len());
                    lua_pushinteger(L, idx as lua_Integer);
                    3
                })),
            }
        }),
    )
}

/// 构造 cosmos.thread 表（open_cosmos 里挂到 cosmos 表下）
pub unsafe fn open(L: *mut lua_State) {
    lua_createtable(L, 0, 2);
    lua_pushcclosure(L, spawn_impl, 0);
    lua_setfield(L, -2, c"spawn".as_ptr());
    lua_pushcclosure(L, wait_impl, 0);
    lua_setfield(L, -2, c"wait".as_ptr());
}

/// 启动时注册 Thread userdata（require("cosmos.Thread")）
pub unsafe fn register_types(L: *mut lua_State) {
    register::<Thread>(L);
}
