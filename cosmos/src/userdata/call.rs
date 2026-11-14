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

/// 调用时复制的 Lua 源位置；可安全随异步值移动，不持有 Lua 引用。
#[derive(Clone, Debug)]
pub struct CallerLocation {
    source: String,
    line: u32,
}
impl CallerLocation {
    pub fn source(&self) -> &str {
        &self.source
    }
    /// 缺失调试行信息时为 0。
    pub fn line(&self) -> u32 {
        self.line
    }
}

pub struct Call<'lua> {
    lua_state: *mut lua_State,
    pending: Option<PendingFuture>,
    scope: ReadScope,
    lifetime: PhantomData<&'lua mut Rc<()>>,
}

impl Call<'_> {
    /// 返回最近的 Lua 调用帧，跳过 C/桥接帧；文件源保留路径，字符串源使用短标签。
    pub fn caller_location(&self) -> CallerLocation {
        unsafe {
            for level in 0..64 {
                let mut debug = std::mem::MaybeUninit::<lua_Debug>::zeroed();
                if lua_getstack(self.lua_state, level, debug.as_mut_ptr()) == 0 {
                    break;
                }
                if lua_getinfo(self.lua_state, c"Sl".as_ptr(), debug.as_mut_ptr()) == 0 {
                    continue;
                }
                let debug = debug.assume_init();
                if debug.what.is_null()
                    || !matches!(
                        std::ffi::CStr::from_ptr(debug.what).to_bytes(),
                        b"Lua" | b"main"
                    )
                {
                    continue;
                }
                let bytes = if !debug.source.is_null() && matches!(*debug.source as u8, b'@' | b'=')
                {
                    let source = std::ffi::CStr::from_ptr(debug.source).to_bytes();
                    &source[1..source.len().min(4097)]
                } else {
                    std::ffi::CStr::from_ptr(debug.short_src.as_ptr()).to_bytes()
                };
                let source = match std::str::from_utf8(bytes) {
                    Ok(text) => text.to_owned(),
                    Err(_) => bytes.escape_ascii().to_string(),
                };
                return CallerLocation {
                    source,
                    line: debug.currentline.max(0) as u32,
                };
            }
        }
        CallerLocation {
            source: "<native>".into(),
            line: 0,
        }
    }
    pub fn argument(&self, index: usize) -> Value<'_> {
        assert!(index > 0, "argument indices start at 1");
        unsafe {
            Value::new(
                self.lua_state,
                i32::try_from(index).expect("argument index is too large"),
            )
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
        assert!(
            self.pending.is_none(),
            "a call can only suspend one operation"
        );
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
