// LuaJIT (Lua 5.1 语义) C API 子集，手写声明，按需补充
#![allow(non_camel_case_types)]
pub use libc::{c_char, c_int, c_void, size_t};

pub type lua_State = c_void;
pub type lua_Number = libc::c_double;
pub type lua_Integer = libc::ptrdiff_t;
pub type lua_CFunction = unsafe extern "C" fn(*mut lua_State) -> c_int;

pub const LUA_YIELD: c_int = 1;
pub const LUA_ERRRUN: c_int = 2;
pub const LUA_ERRSYNTAX: c_int = 3;
pub const LUA_ERRMEM: c_int = 4;
pub const LUA_ERRERR: c_int = 5;

// lua_type 返回值（5.1 语义）
pub const LUA_TNIL: c_int = 0;
pub const LUA_TBOOLEAN: c_int = 1;
pub const LUA_TNUMBER: c_int = 3;
pub const LUA_TSTRING: c_int = 4;
pub const LUA_TTABLE: c_int = 5;
pub const LUA_TFUNCTION: c_int = 6;
pub const LUA_TUSERDATA: c_int = 7;

pub const LUA_GLOBALSINDEX: c_int = -10002;
pub const LUA_REGISTRYINDEX: c_int = -10000;

extern "C" {
    pub fn luaL_newstate() -> *mut lua_State;
    pub fn luaL_openlibs(L: *mut lua_State);
    pub fn luaL_loadstring(L: *mut lua_State, s: *const c_char) -> c_int;
    pub fn lua_close(L: *mut lua_State);

    pub fn lua_newthread(L: *mut lua_State) -> *mut lua_State;
    pub fn lua_resume(L: *mut lua_State, nargs: c_int) -> c_int;
    pub fn lua_yield(L: *mut lua_State, nresults: c_int) -> c_int;

    pub fn lua_gettop(L: *mut lua_State) -> c_int;
    pub fn lua_type(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tonumber(L: *mut lua_State, idx: c_int) -> lua_Number;
    pub fn lua_tointeger(L: *mut lua_State, idx: c_int) -> lua_Integer;
    pub fn lua_toboolean(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tolstring(L: *mut lua_State, idx: c_int, len: *mut size_t) -> *const c_char;

    pub fn lua_pushnumber(L: *mut lua_State, n: lua_Number);
    pub fn lua_pushinteger(L: *mut lua_State, n: lua_Integer);
    pub fn lua_pushboolean(L: *mut lua_State, b: c_int);
    pub fn lua_pushnil(L: *mut lua_State);
    pub fn lua_pushlstring(L: *mut lua_State, s: *const c_char, len: size_t);
    pub fn lua_pushvalue(L: *mut lua_State, idx: c_int);
    pub fn lua_pushlightuserdata(L: *mut lua_State, p: *mut c_void);
    pub fn lua_pushcclosure(L: *mut lua_State, f: lua_CFunction, n: c_int);
    pub fn lua_createtable(L: *mut lua_State, narr: c_int, nrec: c_int);
    pub fn lua_getfield(L: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_setfield(L: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_settable(L: *mut lua_State, idx: c_int);
    pub fn lua_settop(L: *mut lua_State, idx: c_int);

    pub fn lua_newuserdata(L: *mut lua_State, size: size_t) -> *mut c_void;
    pub fn lua_touserdata(L: *mut lua_State, idx: c_int) -> *mut c_void;
    pub fn lua_setmetatable(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn luaL_newmetatable(L: *mut lua_State, tname: *const c_char) -> c_int;
    pub fn luaL_checkudata(L: *mut lua_State, idx: c_int, tname: *const c_char) -> *mut c_void;

    pub fn luaL_ref(L: *mut lua_State, t: c_int) -> c_int;
    pub fn luaL_unref(L: *mut lua_State, t: c_int, ref_: c_int);
    pub fn lua_error(L: *mut lua_State) -> c_int;
}
