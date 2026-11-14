// LuaJIT (Lua 5.1 语义) C API 子集，手写声明，按需补充
#![allow(non_camel_case_types)]
pub(crate) use libc::c_void;
pub(crate) use libc::{c_char, c_int, size_t};

pub(crate) type lua_State = c_void;
pub(crate) type lua_Number = libc::c_double;
pub(crate) type lua_Integer = libc::ptrdiff_t;
pub(crate) type lua_CFunction = unsafe extern "C" fn(*mut lua_State) -> c_int;

pub(crate) const LUA_YIELD: c_int = 1;
pub(crate) const LUA_ERRRUN: c_int = 2;
pub(crate) const LUA_ERRSYNTAX: c_int = 3;
pub(crate) const LUA_ERRMEM: c_int = 4;
pub(crate) const LUA_ERRERR: c_int = 5;

// lua_type 返回值（5.1 语义；LUA_TNONE 表示栈上无此位置 = 调用方未传该实参）
pub(crate) const LUA_TNONE: c_int = -1;
pub(crate) const LUA_TNIL: c_int = 0;
pub(crate) const LUA_TBOOLEAN: c_int = 1;
pub(crate) const LUA_TLIGHTUSERDATA: c_int = 2;
pub(crate) const LUA_TNUMBER: c_int = 3;
pub(crate) const LUA_TSTRING: c_int = 4;
pub(crate) const LUA_TTABLE: c_int = 5;
pub(crate) const LUA_TFUNCTION: c_int = 6;
pub(crate) const LUA_TUSERDATA: c_int = 7;

pub(crate) const LUA_GLOBALSINDEX: c_int = -10002;
pub(crate) const LUA_REGISTRYINDEX: c_int = -10000;
pub(crate) const LUA_REFNIL: c_int = -1;

extern "C" {
    pub(crate) fn luaL_newstate() -> *mut lua_State;
    pub(crate) fn luaL_openlibs(lua_state: *mut lua_State);
    #[cfg(test)]
    pub(crate) fn luaL_loadstring(lua_state: *mut lua_State, s: *const c_char) -> c_int;
    pub(crate) fn luaL_loadbuffer(
        lua_state: *mut lua_State,
        buff: *const c_char,
        sz: size_t,
        name: *const c_char,
    ) -> c_int;
    #[cfg(test)]
    pub(crate) fn lua_pcall(
        lua_state: *mut lua_State,
        nargs: c_int,
        nresults: c_int,
        errfunc: c_int,
    ) -> c_int;
    pub(crate) fn lua_close(lua_state: *mut lua_State);

    pub(crate) fn lua_newthread(lua_state: *mut lua_State) -> *mut lua_State;
    pub(crate) fn lua_xmove(from: *mut lua_State, to: *mut lua_State, n: c_int);
    pub(crate) fn lua_resume(lua_state: *mut lua_State, nargs: c_int) -> c_int;
    pub(crate) fn lua_yield(lua_state: *mut lua_State, nresults: c_int) -> c_int;

    pub(crate) fn lua_gettop(lua_state: *mut lua_State) -> c_int;
    pub(crate) fn lua_type(lua_state: *mut lua_State, idx: c_int) -> c_int;
    pub(crate) fn lua_tonumber(lua_state: *mut lua_State, idx: c_int) -> lua_Number;
    pub(crate) fn lua_tointeger(lua_state: *mut lua_State, idx: c_int) -> lua_Integer;
    pub(crate) fn lua_toboolean(lua_state: *mut lua_State, idx: c_int) -> c_int;
    pub(crate) fn lua_tolstring(
        lua_state: *mut lua_State,
        idx: c_int,
        len: *mut size_t,
    ) -> *const c_char;

    pub(crate) fn lua_pushnumber(lua_state: *mut lua_State, n: lua_Number);
    pub(crate) fn lua_pushinteger(lua_state: *mut lua_State, n: lua_Integer);
    pub(crate) fn lua_pushboolean(lua_state: *mut lua_State, b: c_int);
    pub(crate) fn lua_pushnil(lua_state: *mut lua_State);
    pub(crate) fn lua_pushlstring(lua_state: *mut lua_State, s: *const c_char, len: size_t);
    pub(crate) fn lua_pushvalue(lua_state: *mut lua_State, idx: c_int);
    pub(crate) fn lua_rawgeti(lua_state: *mut lua_State, idx: c_int, n: c_int);
    pub(crate) fn lua_next(lua_state: *mut lua_State, idx: c_int) -> c_int;
    pub(crate) fn lua_pushlightuserdata(lua_state: *mut lua_State, p: *mut c_void);
    pub(crate) fn lua_pushcclosure(lua_state: *mut lua_State, f: lua_CFunction, n: c_int);
    pub(crate) fn lua_createtable(lua_state: *mut lua_State, narr: c_int, nrec: c_int);
    pub(crate) fn lua_getfield(lua_state: *mut lua_State, idx: c_int, k: *const c_char);
    pub(crate) fn lua_setfield(lua_state: *mut lua_State, idx: c_int, k: *const c_char);
    pub(crate) fn lua_settable(lua_state: *mut lua_State, idx: c_int);
    pub(crate) fn lua_settop(lua_state: *mut lua_State, idx: c_int);
    pub(crate) fn lua_replace(lua_state: *mut lua_State, idx: c_int);

    pub(crate) fn lua_newuserdata(lua_state: *mut lua_State, size: size_t) -> *mut c_void;
    pub(crate) fn lua_touserdata(lua_state: *mut lua_State, idx: c_int) -> *mut c_void;
    pub(crate) fn lua_getmetatable(lua_state: *mut lua_State, idx: c_int) -> c_int;
    pub(crate) fn lua_rawequal(lua_state: *mut lua_State, idx1: c_int, idx2: c_int) -> c_int;
    pub(crate) fn lua_setmetatable(lua_state: *mut lua_State, idx: c_int) -> c_int;

    pub(crate) fn luaL_ref(lua_state: *mut lua_State, t: c_int) -> c_int;
    pub(crate) fn luaL_unref(lua_state: *mut lua_State, t: c_int, ref_: c_int);
    pub(crate) fn lua_error(lua_state: *mut lua_State) -> c_int;
}
