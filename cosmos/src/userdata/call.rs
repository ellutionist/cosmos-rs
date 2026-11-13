//! Rust 调用适配共用的 C ABI 边界；宏和带状态闭包均不需要生成 C 函数。

use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;

use super::context::ReadScope;
use super::{push, trampoline, Output, PushLua, UserData, Value};
use crate::ffi::*;
use crate::light_thread::{yield_with, PendingFuture};

/// 原生调用适配；泛型 trampoline 在 cosmos 内部单态化，不额外分配回调元数据。
///
/// # Safety
/// 实现必须通过 Call::returns 或 Call::suspend 结束调用，返回准确的输出数量。
/// 从参数或捕获状态借出的守卫不得逃逸到执行操作及 VM 的生命周期以外。
pub unsafe trait LuaCallable {
    fn call(call: &mut Call<'_>) -> Result<i32, String>;
}

pub struct Call<'lua> {
    lua_state: *mut lua_State,
    pending: Option<PendingFuture>,
    scope: ReadScope,
    lifetime: PhantomData<&'lua mut Rc<()>>,
}

impl Call<'_> {
    pub fn argument(&self, index: usize) -> Value<'_> {
        assert!(index > 0, "参数编号从一开始");
        unsafe {
            Value::new(self.lua_state, i32::try_from(index).expect("参数编号过大"))
                .scoped(&self.scope)
        }
    }

    /// 捕获闭包自身的状态，与普通方法参数使用相同的类型转换及借用检查。
    pub fn capture(&self) -> Value<'_> {
        unsafe { Value::new(self.lua_state, LUA_GLOBALSINDEX - 1).scoped(&self.scope) }
    }

    /// # Safety
    /// value 遵守 PushLua 契约；依赖本次参数的借用必须在本次调用内完成输出。
    pub unsafe fn returns<T: PushLua>(&self, value: T) -> i32 {
        Output::new(self.lua_state).push(value)
    }

    pub fn suspend<F>(&mut self, future: F) -> i32
    where
        F: Future + 'static,
        F::Output: PushLua,
    {
        assert!(self.pending.is_none(), "一次调用只能挂起一个操作");
        self.pending = Some(Box::pin(future));
        0
    }
}

pub(super) unsafe extern "C" fn invoke<C: LuaCallable>(lua_state: *mut lua_State) -> c_int {
    trampoline(lua_state, |lua_state| {
        let mut call = Call {
            lua_state,
            pending: None,
            scope: ReadScope::default(),
            lifetime: PhantomData,
        };
        let count = C::call(&mut call)?;
        match call.pending.take() {
            Some(future) => Ok(unsafe { yield_with(lua_state, future) }),
            None => Ok(count),
        }
    })
}

/// 将 userdata 状态与其 Rust 调用适配组合成 Lua 闭包，和宏方法共用 invoke。
pub struct LuaClosure<T>(pub T);

impl<T: UserData + LuaCallable> PushLua for LuaClosure<T> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        push(output.lua_state, self.0);
        lua_pushcclosure(output.lua_state, invoke::<T>, 1);
        1
    }
}
