use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use libc::c_int;

use crate::ffi::*;
use crate::stack;
use crate::userdata::{FromLua, PushLua, RegistryRef};

/// 擦除异步操作的具体类型，在完成的当次轮询中直接压栈。
/// 输出仍是原生 Rust 值，只要求实现 PushLua，无需单独装箱返回值。
pub(crate) trait PendingOp {
    /// Pending 时不压栈；Ready 返回压入的值数，交给下一次 lua_resume。
    /// 与 PushLua 相同，转换过程中不得调用 lua_error。
    ///
    /// # Safety
    /// lua_state 必须是当前线程上仍存活的协程，其栈空间及 VM 归属满足输出值的 PushLua 契约。
    unsafe fn poll_push(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        lua_state: *mut lua_State,
    ) -> Poll<i32>;
}

impl<F: Future> PendingOp for F
where
    F::Output: PushLua,
{
    unsafe fn poll_push(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        lua_state: *mut lua_State,
    ) -> Poll<i32> {
        self.poll(cx)
            .map(|value| unsafe { value.push_lua(&mut crate::userdata::Output::new(lua_state)) })
    }
}

/// 协程挂起期间等待的异步操作。
/// 无 Send 约束：LightThread 本身 !Send（线程亲和），整个链路不会跨线程，
/// 放宽后 op 可以捕获 !Send 的数据（如带 Rc 的用户对象）
pub(crate) type PendingFuture = Pin<Box<dyn PendingOp>>;

// yield 通道：C 函数把 PendingFuture 塞进 TLS 槽再 lua_yield(lua_state, 0)，
// 同一线程上的 poll 侧从槽里取回（见 yield_with / Execution::poll_step）。
//
// 相比 lightuserdata + Box::into_raw 的堆指针通道，每次 yield 少一次分配，
// 代价是绑定单线程 runtime：写槽和取槽必须发生在同一条线程上。
// lua_State 本身线程亲和；LightThread 与 registry 锚点均为 !Send，执行不会跨线程迁移。
thread_local! {
    static PENDING_OP: RefCell<Option<PendingFuture>> = const { RefCell::new(None) };
}

/// C 函数侧协议：op 进槽 + yield 0 个值（结果等 resume 时再由 poll 侧压栈）。
/// lua_yield 的返回值必须作为 C 函数返回值交给 VM
/// （5.1 语义：常规 yield 路径返回 -1 即 LUA_YIELD，不 longjmp）
///
/// # Safety
/// lua_state 必须是当前 Lua C 回调中可 yield 的有效协程，fut 只能在同一线程执行。
/// 返回值必须直接交给 Lua VM；宿主关闭 VM 前必须先回收挂起任务。
pub(crate) unsafe fn yield_with(lua_state: *mut lua_State, fut: PendingFuture) -> c_int {
    PENDING_OP.with(|slot| {
        debug_assert!(slot.borrow().is_none(), "previous pending op was not taken");
        *slot.borrow_mut() = Some(fut);
    });
    unsafe { lua_yield(lua_state, 0) }
}

/// 执行状态的两个阶段，决定本轮 resume 的 nargs 从哪来：
/// - Fresh：协程从未启动，初始参数在栈上，nargs = gettop - 1
/// - Awaiting(fut)：挂起的异步操作，完成后其结果压栈，nargs = 压栈个数
enum Phase {
    Fresh,
    Awaiting(PendingFuture),
}

pub(super) enum Step {
    Yielded,
    Finished,
}

/// LightThread 私有的执行状态；不独立调度，也不实现 Future。
/// 挂起操作先于 registry 锚点释放，确保借用守卫不访问已经回收的 userdata。
pub(super) struct Execution {
    lua_state: *mut lua_State,
    phase: Phase,
    _anchor: RegistryRef,
}

impl Execution {
    /// 把 lua_newthread 出来的协程锚定到 registry（luaL_ref 会从栈上取走它），
    /// 防止挂起期间被 GC 回收。
    /// 初始参数在 new 之前压到协程栈上（位于 chunk 函数之上），
    /// 首次 resume 会按 gettop - 1 自动传参（对应脚本里的 `...`）
    ///
    /// # Safety
    /// main 与 lua_state 必须属于同一个存活 VM，main 栈顶是协程对象，协程栈上已准备好函数和参数。
    /// 在本对象及其挂起操作释放前，宿主不得关闭 VM，且必须始终在当前线程轮询。
    pub(super) unsafe fn new(main: *mut lua_State, lua_state: *mut lua_State) -> Self {
        Self {
            lua_state,
            phase: Phase::Fresh,
            _anchor: RegistryRef::new(main),
        }
    }

    /// 每步最多恢复一次 Lua，将继续推进的调度与取消检查留给 LightThread。
    pub(super) fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<Result<Step, String>> {
        let nargs = match &mut self.phase {
            Phase::Fresh => unsafe { lua_gettop(self.lua_state) - 1 },
            Phase::Awaiting(future) => {
                match unsafe { future.as_mut().poll_push(cx, self.lua_state) } {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(nargs) => nargs,
                }
            }
        };
        // 已完成的异步操作先释放借用，再让 Lua 继续访问 userdata。
        self.phase = Phase::Fresh;
        match unsafe { lua_resume(self.lua_state, nargs) } {
            LUA_YIELD => {
                let Some(future) = PENDING_OP.with(|slot| slot.borrow_mut().take()) else {
                    return Poll::Ready(Err(
                        "coroutine yielded without a pending future".to_string()
                    ));
                };
                self.phase = Phase::Awaiting(future);
                Poll::Ready(Ok(Step::Yielded))
            }
            0 => Poll::Ready(Ok(Step::Finished)),
            LUA_ERRRUN | LUA_ERRSYNTAX | LUA_ERRMEM | LUA_ERRERR => {
                Poll::Ready(Err(unsafe { stack::err_message(self.lua_state) }))
            }
            status => Poll::Ready(Err(format!("lua_resume: unexpected status {status}"))),
        }
    }

    /// 锚点仍有效时转换返回值，调用方随后才可释放本执行状态。
    pub(super) fn read_result<R: for<'lua> FromLua<'lua>>(&self) -> Result<R, String> {
        unsafe { crate::userdata::Value::new(self.lua_state, 1).read::<R>() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::userdata::{FromLua, Nil};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Waker;

    #[test]
    #[allow(
        clippy::await_holding_refcell_ref,
        reason = "此回归专门验证 userdata 借用跨 await 保活，并在完成后释放"
    )]
    fn pending_operation_pushes_only_when_ready_and_releases_borrow() {
        unsafe {
            let lua_state = luaL_newstate();
            assert!(!lua_state.is_null());
            let state = Rc::new(RefCell::new(7i64));
            let owner = state.clone();
            let mut op: PendingFuture = Box::pin(async move {
                let guard = owner.borrow_mut();
                let mut pending = true;
                std::future::poll_fn(|cx| {
                    if pending {
                        pending = false;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
                (*guard, vec![0, 255, 1])
            });
            let mut cx = Context::from_waker(Waker::noop());
            assert_eq!(op.as_mut().poll_push(&mut cx, lua_state), Poll::Pending);
            assert_eq!(lua_gettop(lua_state), 0);
            assert!(state.try_borrow_mut().is_err());

            assert_eq!(op.as_mut().poll_push(&mut cx, lua_state), Poll::Ready(2));
            assert!(state.try_borrow_mut().is_ok());
            assert_eq!(lua_gettop(lua_state), 2);
            assert_eq!(
                i64::from_lua(crate::userdata::Value::new(lua_state, 1)).unwrap(),
                7
            );
            assert_eq!(
                Vec::<u8>::from_lua(crate::userdata::Value::new(lua_state, 2)).unwrap(),
                [0, 255, 1]
            );
            drop(op);
            assert_eq!(Rc::strong_count(&state), 1);
            lua_close(lua_state);
        }
    }

    #[test]
    fn async_return_counts_distinguish_unit_none_and_errors() {
        unsafe {
            let lua_state = luaL_newstate();
            assert!(!lua_state.is_null());
            let mut cx = Context::from_waker(Waker::noop());
            let mut operations: Vec<PendingFuture> = vec![
                Box::pin(async {}),
                Box::pin(async { None::<i64> }),
                Box::pin(async { Err::<(), _>("失败") }),
            ];
            for (op, expected) in operations.iter_mut().zip([0, 1, 2]) {
                lua_settop(lua_state, 0);
                assert_eq!(
                    op.as_mut().poll_push(&mut cx, lua_state),
                    Poll::Ready(expected)
                );
                assert_eq!(lua_gettop(lua_state), expected);
                if expected > 0 {
                    assert_eq!(lua_type(lua_state, 1), LUA_TNIL);
                }
            }
            assert_eq!(
                String::from_lua(crate::userdata::Value::new(lua_state, 2)).unwrap(),
                "失败"
            );
            drop(operations);
            lua_close(lua_state);
        }
    }

    #[test]
    fn cancelling_pending_operation_drops_captured_values_without_pushing() {
        unsafe {
            let lua_state = luaL_newstate();
            assert!(!lua_state.is_null());
            let dropped = Rc::new(Cell::new(false));
            struct DropFlag(Rc<Cell<bool>>);
            impl Drop for DropFlag {
                fn drop(&mut self) {
                    self.0.set(true);
                }
            }
            let flag = DropFlag(dropped.clone());
            let mut op: PendingFuture = Box::pin(async move {
                std::future::pending::<()>().await;
                drop(flag);
                Nil
            });
            let mut cx = Context::from_waker(Waker::noop());
            assert_eq!(op.as_mut().poll_push(&mut cx, lua_state), Poll::Pending);
            drop(op);
            assert!(dropped.get());
            assert_eq!(lua_gettop(lua_state), 0);
            lua_close(lua_state);
        }
    }
}
