//! Rust 自定义类型 → Lua userdata 桥接。
//!
//! 类型注册走 #[cosmos::userdata] 属性宏：宏为 impl 块里的每个方法生成
//! extern "C" trampoline（参数转换、panic 捕获、返回值压栈）并生成
//! UserData impl（见 cosmos-macros crate）。
//!
//! 运行时侧：
//! - push::<T> 把 T placement-write 进 lua_newuserdata 的槽（Lua GC 托管，
//!   不额外堆分配），metatable 在首次 push 时懒构建
//! - metatable 兼任类型表：__index = 自身（静态构造 + 实例方法共用一张表）、
//!   __gc = drop_in_place 统一生成，经 package.preload 用 require 引入
//!   （local Counter = require("cosmos.Counter")）
//! - 类型校验用 luaL_checkudata（registry 按 type_name 找 metatable 后比对）
//!
//! 线程模型：全部约束在单线程（同 coroutine.rs 的 TLS 通道）；
//! 方法 trampoline 在 lua_resume 的保护帧内执行，lua_error 长跳安全。

use std::any::TypeId;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use crate::ffi::*;

/// #[cosmos::userdata] 宏为每个类型生成该 impl
pub trait UserData: 'static {
    fn type_name() -> &'static str {
        std::any::type_name::<Self>()
    }
    /// Lua 侧 require 引入用的类型名（local Counter = require("cosmos.Counter")），
    /// 宏默认生成类型短名（路径最后一段）
    fn lua_name() -> &'static str {
        std::any::type_name::<Self>()
    }
    fn add_methods(_methods: &mut MethodTable<Self>)
    where
        Self: Sized,
    {
    }
}

pub struct MethodTable<T> {
    // CStr 而非 &str：lua_setfield 按 C 字符串读，&str 字面量不保证 NUL 结尾
    entries: Vec<(&'static std::ffi::CStr, lua_CFunction)>,
    _marker: PhantomData<fn(T)>,
}

impl<T> MethodTable<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            _marker: PhantomData,
        }
    }

    pub fn add(&mut self, name: &'static std::ffi::CStr, f: lua_CFunction) {
        self.entries.push((name, f));
    }
}

// 已注册类型缓存，键是主线程状态。Lua 5.1 的协程 L 拿不到主状态指针
// （LuaJIT 也没有 lua_mainthread），所以 init 时把主状态以 lightuserdata
// 锚定在 registry（registry 对所有协程可见），之后经 main_state 取回
thread_local! {
    static REGISTERED: RefCell<HashMap<*mut lua_State, HashSet<TypeId>>> =
        RefCell::new(HashMap::new());
}

const MAIN_STATE_KEY: &std::ffi::CStr = c"__cosmos_main_state";

/// 状态初始化：把主状态指针锚定进 registry，必须早于任何 push 调用
pub unsafe fn init(L: *mut lua_State) {
    lua_pushlightuserdata(L, L as *mut c_void);
    lua_setfield(L, LUA_REGISTRYINDEX, MAIN_STATE_KEY.as_ptr());
}

unsafe fn main_state(L: *mut lua_State) -> *mut lua_State {
    lua_getfield(L, LUA_REGISTRYINDEX, MAIN_STATE_KEY.as_ptr());
    let p = lua_touserdata(L, -1);
    lua_settop(L, -2);
    if p.is_null() {
        L // 未 init 时退化为调用方状态（主状态直接调用 push 的场景）
    } else {
        p as *mut lua_State
    }
}

// 每个类型的 CString 名字只构建一次（leak 进 'static，类型总数有限）
fn type_cstring<T: 'static>() -> &'static CString {
    thread_local! {
        static NAMES: RefCell<HashMap<TypeId, &'static CString>> = RefCell::new(HashMap::new());
    }
    NAMES.with(|names| {
        *names
            .borrow_mut()
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                Box::leak(Box::new(CString::new(std::any::type_name::<T>()).unwrap()))
            })
    })
}

fn ensure_metatable<T: UserData>(main: *mut lua_State, L: *mut lua_State) {
    let registered = REGISTERED.with(|m| {
        m.borrow()
            .get(&main)
            .map(|s| s.contains(&TypeId::of::<T>()))
            .unwrap_or(false)
    });
    if registered {
        return;
    }
    unsafe {
        let name = type_cstring::<T>();
        luaL_newmetatable(L, name.as_ptr()); // registry[type_name] = mt，同时压栈

        // 静态构造和实例方法共用这一张表：__index 指向自己，
        // obj:method() → mt.method，Counter.new() → 命名空间字段直接索引 mt
        lua_pushvalue(L, -1);
        lua_setfield(L, -2, c"__index".as_ptr());

        let mut methods = MethodTable::new();
        T::add_methods(&mut methods);
        for (mname, f) in methods.entries {
            lua_pushcclosure(L, f, 0);
            lua_setfield(L, -2, mname.as_ptr());
        }

        // __gc 统一生成：drop_in_place 对无 Drop 的类型也是合法空操作，
        // 宏不需要探测 Drop 的存在
        lua_pushcclosure(L, gc_trampoline::<T>, 0);
        lua_setfield(L, -2, c"__gc".as_ptr());

        // 类型表经 require 引入：package.preload["cosmos.Counter"] = 工厂函数。
        // require 约定 preload 条目必须是函数（返回非函数值会被静默丢弃，
        // 见 LuaJIT lib_package.c lj_cf_package_require），工厂返回 mt 自身
        let preload_key = format!("cosmos.{}", T::lua_name());
        lua_getfield(L, LUA_GLOBALSINDEX, c"package".as_ptr()); // [mt, package]
        lua_getfield(L, -1, c"preload".as_ptr()); // [mt, package, preload]
        lua_pushlstring(L, preload_key.as_ptr() as *const c_char, preload_key.len());
        lua_pushvalue(L, -4); // [mt, package, preload, key, mt]
        lua_pushcclosure(L, preload_factory, 1); // mt 副本收为 upvalue → [.., key, factory]
        lua_settable(L, -3); // preload[key] = factory
        lua_settop(L, -3); // 栈恢复

        REGISTERED.with(|m| {
            m.borrow_mut().entry(main).or_default().insert(TypeId::of::<T>());
        });
    }
}

/// 提前注册类型（构建 metatable + 挂 require 入口），
/// 让脚本可以在任何实例产生之前 require("cosmos.Counter")。
/// 不调用也能工作：首次 push 时会懒注册，只是 require 会晚于第一个实例
pub fn register<T: UserData>(L: *mut lua_State) {
    unsafe {
        let main = main_state(L);
        ensure_metatable::<T>(main, L);
    }
}

/// 构造 userdata 并把 T 压上 Lua 栈（栈顶 +1 个值）。
/// L 可以是协程线程，metatable 注册统一记在主状态名下
pub fn push<T: UserData>(L: *mut lua_State, value: T) {
    unsafe {
        let main = main_state(L);
        ensure_metatable::<T>(main, L);
        // LuaJIT userdata 对齐保证是 8 字节，更高对齐的类型直接断言拒绝
        debug_assert!(std::mem::align_of::<T>() <= 8);
        let p = lua_newuserdata(L, std::mem::size_of::<T>()) as *mut T;
        ptr::write(p, value);
        // 5.1 的 luaL_getmetatable 是宏（registry 上的 lua_getfield），直接用 lua_getfield
        lua_getfield(L, LUA_REGISTRYINDEX, type_cstring::<T>().as_ptr());
        lua_setmetatable(L, -2);
    }
}

/// 方法 trampoline 里取 &mut T。luaL_checkudata 校验 metatable，不符直接 lua_error。
/// 返回的引用只在本次调用内有效（userdata 因参数压栈而被锚定，不会被 GC）
pub unsafe fn check<T: UserData>(L: *mut lua_State, idx: c_int) -> &'static mut T {
    let p = luaL_checkudata(L, idx, type_cstring::<T>().as_ptr());
    &mut *(p as *mut T)
}

pub unsafe fn check_shared<T: UserData>(L: *mut lua_State, idx: c_int) -> &'static T {
    let p = luaL_checkudata(L, idx, type_cstring::<T>().as_ptr());
    &*(p as *const T)
}

unsafe extern "C" fn gc_trampoline<T: UserData>(L: *mut lua_State) -> c_int {
    let p = lua_touserdata(L, 1) as *mut T;
    if !p.is_null() {
        // Drop 里 panic 不能穿过 extern "C" 帧，吞掉
        let _ = catch_unwind(AssertUnwindSafe(|| ptr::drop_in_place(p)));
    }
    0
}

/// require 的 preload 工厂：upvalue 1 就是 metatable，直接返回。
/// require 会用返回值填充 package.loaded[name]
unsafe extern "C" fn preload_factory(L: *mut lua_State) -> c_int {
    lua_pushvalue(L, LUA_GLOBALSINDEX - 1); // lua_upvalueindex(1)
    1
}

/// 方法 trampoline 外壳：捕获 panic、把 Err 转成 lua_error。
/// 只允许在 lua_resume 的保护帧内调用（即从 Lua 方法路径进入），
/// 否则 lua_error 没有 setjmp 目标会 PANIC
pub fn trampoline<F>(L: *mut lua_State, f: F) -> c_int
where
    F: FnOnce(*mut lua_State) -> Result<i32, String>,
{
    match catch_unwind(AssertUnwindSafe(|| f(L))) {
        Ok(Ok(n)) => n,
        Ok(Err(msg)) => unsafe { raise(L, &msg) },
        Err(p) => {
            let detail = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_string());
            unsafe { raise(L, &format!("rust panic: {detail}")) }
        }
    }
}

unsafe fn raise(L: *mut lua_State, msg: &str) -> c_int {
    lua_pushlstring(L, msg.as_ptr() as *const c_char, msg.len());
    lua_error(L) // longjmp，不返回
}

/// lua_close 前调用，清掉该状态的注册缓存（否则线程本地 map 会残留死键）
pub fn cleanup(L: *mut lua_State) {
    unsafe {
        let main = main_state(L);
        REGISTERED.with(|m| {
            m.borrow_mut().remove(&main);
        });
    }
}

/// 方法参数：从 Lua 栈取值
pub trait FromLua: Sized {
    /// # Safety
    /// L 是当前执行中的状态，idx 在有效栈范围内
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String>;
}

impl FromLua for f64 {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        Ok(lua_tonumber(L, idx))
    }
}

impl FromLua for i64 {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        Ok(lua_tointeger(L, idx) as i64)
    }
}

impl FromLua for bool {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TBOOLEAN {
            return Err(format!("argument {idx}: expected boolean"));
        }
        Ok(lua_toboolean(L, idx) != 0)
    }
}

impl FromLua for String {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(L, idx, &mut len);
        Ok(String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned())
    }
}

/// 统一的压栈协议：把 Rust 值压上 Lua 栈，返回压入个数。
///
/// 两条路径共用这一个协议：
/// - 同步方法返回：trampoline 内直接压栈，userdata 走 blanket impl 零装箱
/// - 异步 op 输出：边界类型 AsyncOutput（coroutine.rs）也实现本 trait，
///   poll loop 用同一入口压栈，各变体委托回本文件的标量 impl——
///   lua_pushxxx 全代码库只出现一份
///
/// 安全约定：压栈在 lua_resume 之间也合法；实现里不得调用 lua_error
/// （那需要 lua_resume 的保护帧，错误应返回 Err 由 trampoline 转换）
pub trait PushLua {
    fn push_lua(self, L: *mut lua_State) -> i32;
}

impl PushLua for () {
    fn push_lua(self, _L: *mut lua_State) -> i32 {
        0
    }
}
impl PushLua for i64 {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushinteger(L, self as lua_Integer) };
        1
    }
}
impl PushLua for f64 {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushnumber(L, self) };
        1
    }
}
impl PushLua for bool {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushboolean(L, self as c_int) };
        1
    }
}
impl PushLua for String {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushlstring(L, self.as_ptr() as *const c_char, self.len()) };
        1
    }
}
impl<T: PushLua> PushLua for Option<T> {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        match self {
            Some(v) => v.push_lua(L),
            None => 0,
        }
    }
}
/// userdata 返回：直接构造压栈，零装箱（不走 AsyncOutput::Custom）
impl<T: UserData> PushLua for T {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        push::<T>(L, self);
        1
    }
}
