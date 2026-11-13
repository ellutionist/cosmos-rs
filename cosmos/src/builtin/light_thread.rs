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
//! 协程挂起时把待处理的 future 交给 LuaCoroutine 驱动（见 crate::coroutine），
//! 多个任务在同一个 current_thread runtime 的 LocalSet 上并发推进。
//! 未 join 的线程出错会打印到 stderr（避免静默丢失）。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::coroutine::{yield_with, LuaCoroutine};
use crate::ffi::*;
use crate::userdata::{push, register, trampoline, try_check, Nil};

/// 轻线程句柄（Lua userdata）
pub struct LightThread {
    /// join / kill 用；join 之后 take 掉
    handle: Option<JoinHandle<Result<(), String>>>,
    /// wait 用：任务完成时发送一次结果（kill 会让发送端随任务一起 drop）
    done: Option<oneshot::Receiver<Result<(), String>>>,
}

#[cosmos::userdata(name = "LightThread")]
impl LightThread {
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
unsafe extern "C" fn spawn_impl(lua_state: *mut lua_State) -> c_int {
    trampoline(lua_state, |lua_state| unsafe {
        if lua_type(lua_state, 1) != LUA_TFUNCTION {
            return Err("thread.spawn: expected a function".to_string());
        }
        let n = lua_gettop(lua_state);
        let thread = lua_newthread(lua_state); // 线程对象压栈，new() 里 luaL_ref 会取走
        for i in 1..=n {
            lua_pushvalue(lua_state, i);
            lua_xmove(lua_state, thread, 1);
        }
        // 新协程栈：[fn, args...]，Fresh 阶段按 gettop-1 传参
        let co = LuaCoroutine::new(lua_state, thread);
        let (tx, done) = oneshot::channel();
        let handle = tokio::task::spawn_local(async move {
            let res = co.await;
            if let Err(e) = &res {
                eprintln!("cosmos.thread: unhandled error: {e}");
            }
            let _ = tx.send(res.clone());
            res
        });
        push(
            lua_state,
            LightThread {
                handle: Some(handle),
                done: Some(done),
            },
        );
        Ok(1)
    })
}

/// 轮询一组完成信号，返回第一个结束的（下标从 1 开始，与 wait 的参数位置一致）。
/// 只用 oneshot 信号、不碰 JoinHandle，所以 wait 之后线程仍可 join。
struct WaitAny {
    done: Vec<(usize, oneshot::Receiver<Result<(), String>>)>,
}

/// wait 的返回形态：成功给参数下标，失败给 nil、错误串和参数下标。
#[derive(cosmos::PushLua)]
enum WaitOutcome {
    Done(i64),
    Error(Nil, String, i64),
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
unsafe extern "C" fn wait_impl(lua_state: *mut lua_State) -> c_int {
    trampoline(lua_state, |lua_state| unsafe {
        let n = lua_gettop(lua_state);
        if n < 1 {
            return Err("thread.wait: expected at least one thread".to_string());
        }
        let mut done = Vec::with_capacity(n as usize);
        for i in 1..=n {
            let mut thread = try_check::<LightThread>(lua_state, i)?;
            let Some(rx) = thread.done.take() else {
                return Err("thread.wait: thread already waited".to_string());
            };
            done.push((i as usize, rx));
        }
        Ok(yield_with(
            lua_state,
            Box::pin(async move {
                let (idx, res) = WaitAny { done }.await;
                match res {
                    Ok(()) => WaitOutcome::Done(idx as i64),
                    Err(err) => WaitOutcome::Error(Nil, err, idx as i64),
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

/// 启动时注册 LightThread userdata（require("cosmos.LightThread")）
///
/// # Safety
/// lua_state 必须属于已初始化、已打开标准库的存活 VM，并由当前线程独占操作。
pub(super) unsafe fn register_types(lua_state: *mut lua_State) {
    register::<LightThread>(lua_state);
}
