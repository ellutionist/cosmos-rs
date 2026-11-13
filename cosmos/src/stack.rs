use crate::ffi::*;

/// 取栈顶（-1）的 Lua 错误信息
pub unsafe fn err_message(L: *mut lua_State) -> String {
    let mut len = 0usize;
    let p = lua_tolstring(L, -1, &mut len);
    if p.is_null() {
        "unknown lua error".to_string()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned()
    }
}

/// 协程返回值的 Lua 形态（按栈 1.. 分类）
pub enum ReturnKind {
    /// 没有返回值
    Nothing,
    /// 栈 1 是 table
    Table,
    /// 栈 1 是字符串
    String,
    /// 栈 1 是 nil：带错误串时形如 nil, err
    Nil(Option<String>),
    /// 其他类型（Lua 类型编号）
    Other(c_int),
}

/// 读协程返回值形态：功能层据此分流，不直接看栈
pub unsafe fn return_kind(L: *mut lua_State) -> ReturnKind {
    let top = lua_gettop(L);
    if top == 0 {
        return ReturnKind::Nothing;
    }
    match lua_type(L, 1) {
        LUA_TTABLE => ReturnKind::Table,
        LUA_TSTRING => ReturnKind::String,
        LUA_TNIL => ReturnKind::Nil(if top >= 2 && lua_type(L, 2) == LUA_TSTRING {
            Some(string_at(L, 2))
        } else {
            None
        }),
        other => ReturnKind::Other(other),
    }
}

unsafe fn string_at(L: *mut lua_State, idx: c_int) -> String {
    let mut len = 0usize;
    let p = lua_tolstring(L, idx, &mut len);
    if p.is_null() {
        String::new()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned()
    }
}
