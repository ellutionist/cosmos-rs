//! 受控值读取与输出；原始状态、索引固定和栈恢复只在桥接内部处理。

use std::cell::{Ref, RefCell, RefMut};
use std::ffi::CStr;
use std::marker::PhantomData;
use std::rc::Rc;

use super::{try_check, try_check_shared, FromLua, PushLua, RegistryRef};
use crate::ffi::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Missing,
    Nil,
    Boolean,
    Number,
    String,
    Table,
    Function,
    UserData,
    Other,
}

/// 值的受控视图；不能由外部构造，也不提供原始状态或任意栈操作。
#[derive(Clone, Copy)]
pub struct Value<'lua> {
    pub(super) lua_state: *mut lua_State,
    pub(super) index: i32,
    scope: Option<&'lua ReadScope>,
    temporary: bool,
    lifetime: PhantomData<&'lua Rc<()>>,
}

impl<'lua> Value<'lua> {
    pub(crate) unsafe fn new(lua_state: *mut lua_State, index: i32) -> Self {
        Self {
            lua_state,
            index: crate::stack::abs_index(lua_state, index),
            scope: None,
            temporary: false,
            lifetime: PhantomData,
        }
    }

    pub(super) fn scoped(mut self, scope: &'lua ReadScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 表字段可能在重入回调中被删除；借出的字符串必须独立保活到调用结束。
    pub(super) unsafe fn anchor_borrow(self) -> Result<(), String> {
        if self.temporary {
            let scope = self
                .scope
                .ok_or_else(|| "表字段借用需要有效的调用作用域".to_string())?;
            lua_pushvalue(self.lua_state, self.index);
            scope
                .anchors
                .borrow_mut()
                .push(RegistryRef::new(self.lua_state));
        }
        Ok(())
    }

    pub fn kind(self) -> ValueKind {
        match unsafe { lua_type(self.lua_state, self.index) } {
            LUA_TNONE => ValueKind::Missing,
            LUA_TNIL => ValueKind::Nil,
            LUA_TBOOLEAN => ValueKind::Boolean,
            LUA_TNUMBER => ValueKind::Number,
            LUA_TSTRING => ValueKind::String,
            LUA_TTABLE => ValueKind::Table,
            LUA_TFUNCTION => ValueKind::Function,
            LUA_TUSERDATA => ValueKind::UserData,
            _ => ValueKind::Other,
        }
    }

    /// 读取相邻返回值，偏移从零开始；不向调用方暴露实际栈索引。
    pub fn offset(self, offset: usize) -> Self {
        assert!(self.index > 0 || offset == 0, "捕获状态不能读取相邻值");
        Self {
            index: self
                .index
                .checked_add(i32::try_from(offset).expect("返回值偏移过大"))
                .expect("返回值偏移溢出"),
            ..self
        }
    }

    /// 通过统一协议读取，正常返回、失败或 Rust 展开时均恢复栈高度。
    ///
    /// # Safety
    /// 借用结果仅可在当前同步调用内使用；异步参数及跨调用结果必须拥有数据或自行锚定。
    ///
    /// ```compile_fail
    /// use cosmos::userdata::Value;
    /// fn escape(value: Value<'_>) -> &'static str {
    ///     unsafe { value.read().unwrap() }
    /// }
    /// ```
    pub unsafe fn read<T: FromLua<'lua>>(self) -> Result<T, String> {
        let _restore = StackRestore::new(self.lua_state);
        T::from_lua(self)
    }

    /// # Safety
    /// 守卫仅供本次调用或其挂起操作使用，不能保存到 VM 生命周期以外。
    /// userdata 必须由调用帧、闭包捕获或独立 LuaValue 锚点保活；可变表字段本身不能保证跨挂起存活。
    pub unsafe fn borrow_mut<T: super::UserData>(self) -> Result<RefMut<'static, T>, String> {
        try_check(self.lua_state, self.index)
    }

    /// # Safety
    /// 守卫仅供本次调用或其挂起操作使用，不能保存到 VM 生命周期以外。
    /// userdata 必须由调用帧、闭包捕获或独立 LuaValue 锚点保活；可变表字段本身不能保证跨挂起存活。
    pub unsafe fn borrow<T: super::UserData>(self) -> Result<Ref<'static, T>, String> {
        try_check_shared(self.lua_state, self.index)
    }

    pub fn table(self) -> Result<TableReader<'lua>, String> {
        if self.kind() != ValueKind::Table {
            return Err(format!("argument {}: expected table", self.index));
        }
        Ok(TableReader { value: self })
    }

    pub fn expected(self, expected: &str) -> String {
        format!(
            "argument {}: expected {expected} (got {:?})",
            self.index,
            self.kind()
        )
    }
}

/// 表遍历统一处理非字符串键、绝对索引和失败恢复；不分配键的副本。
pub struct TableReader<'lua> {
    value: Value<'lua>,
}

impl<'lua> TableReader<'lua> {
    /// # Safety
    /// 回调遵守 Value::read 的借用约束，不能修改正在遍历的表或使 VM 失效。
    /// 不能保存定位当前字段的 Value 视图；借出的字符串由调用作用域锚定，不依赖字段继续存在。
    pub unsafe fn visit(
        self,
        mut field: impl FnMut(&[u8], Value<'lua>) -> Result<(), String>,
    ) -> Result<(), String> {
        let lua_state = self.value.lua_state;
        let _restore = StackRestore::new(lua_state);
        lua_pushnil(lua_state);
        while lua_next(lua_state, self.value.index) != 0 {
            if lua_type(lua_state, -2) == LUA_TSTRING {
                let mut len = 0;
                let key = lua_tolstring(lua_state, -2, &mut len);
                field(
                    std::slice::from_raw_parts(key.cast::<u8>(), len),
                    Value {
                        index: lua_gettop(lua_state),
                        temporary: true,
                        ..self.value
                    },
                )?;
            }
            lua_settop(lua_state, -2);
        }
        Ok(())
    }
}

/// 只在读取借用表字段时分配锚点；普通标量、拥有型字段和直接字符串参数不额外分配。
#[derive(Default)]
pub(super) struct ReadScope {
    anchors: RefCell<Vec<RegistryRef>>,
}

/// 结果输出上下文；只能组合已有 PushLua 值和表构造，不能访问原始栈。
pub struct Output<'lua> {
    pub(super) lua_state: *mut lua_State,
    lifetime: PhantomData<&'lua mut Rc<()>>,
}

impl<'lua> Output<'lua> {
    pub(crate) unsafe fn new(lua_state: *mut lua_State) -> Self {
        Self {
            lua_state,
            lifetime: PhantomData,
        }
    }

    /// # Safety
    /// 输出中的 Lua 引用必须属于当前 VM；调用期间所有借用及所属 VM 均有效。
    pub unsafe fn push<T: PushLua>(&mut self, value: T) -> i32 {
        value.push_lua(self)
    }

    pub fn table(&mut self, fields: usize) -> TableWriter<'_, 'lua> {
        unsafe {
            let restore = StackRestore::new(self.lua_state);
            lua_createtable(
                self.lua_state,
                0,
                i32::try_from(fields).expect("表字段过多"),
            );
            TableWriter {
                output: self,
                restore,
            }
        }
    }
}

/// 表字段必须恰好输出一个值；构造失败时移除尚未完成的表。
pub struct TableWriter<'out, 'lua> {
    output: &'out mut Output<'lua>,
    restore: StackRestore,
}

impl TableWriter<'_, '_> {
    /// # Safety
    /// value 遵守 PushLua 契约；非单值输出会被拒绝并撤销表构造。
    pub unsafe fn field<T: PushLua>(&mut self, name: &CStr, value: T) {
        let count = self.output.push(value);
        assert_eq!(count, 1, "表字段必须恰好输出一个值");
        lua_setfield(self.output.lua_state, -2, name.as_ptr());
    }

    /// 支持包含 NUL 的动态字符串键，供字符串 map 与派生字段共用表构造。
    ///
    /// # Safety
    /// value 遵守 PushLua 契约；非单值输出会被拒绝并撤销表构造。
    pub unsafe fn entry<T: PushLua>(&mut self, name: &str, value: T) {
        let lua_state = self.output.lua_state;
        lua_pushlstring(lua_state, name.as_ptr().cast(), name.len());
        assert_eq!(self.output.push(value), 1, "表字段必须恰好输出一个值");
        lua_settable(lua_state, -3);
    }

    pub fn finish(mut self) -> i32 {
        self.restore.top += 1;
        1
    }
}

struct StackRestore {
    lua_state: *mut lua_State,
    top: i32,
}

impl StackRestore {
    unsafe fn new(lua_state: *mut lua_State) -> Self {
        Self {
            lua_state,
            top: lua_gettop(lua_state),
        }
    }
}

impl Drop for StackRestore {
    fn drop(&mut self) {
        unsafe { lua_settop(self.lua_state, self.top) };
    }
}
