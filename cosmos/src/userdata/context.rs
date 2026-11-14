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
    Null,
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
            let scope = self.scope.ok_or_else(|| {
                "borrowing a table field requires an active call scope".to_string()
            })?;
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
            LUA_TLIGHTUSERDATA
                if unsafe { lua_touserdata(self.lua_state, self.index).is_null() } =>
            {
                ValueKind::Null
            }
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
        assert!(
            self.index > 0 || offset == 0,
            "captured state cannot read adjacent values"
        );
        Self {
            index: self
                .index
                .checked_add(i32::try_from(offset).expect("return value offset is too large"))
                .expect("return value offset overflow"),
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

/// 当前转换期间用于识别共享表与循环引用，不提供地址或底层状态访问。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableIdentity(usize);

impl<'lua> TableReader<'lua> {
    pub fn identity(&self) -> TableIdentity {
        TableIdentity(unsafe { lua_topointer(self.value.lua_state, self.value.index) } as usize)
    }

    /// 原始读取元表字段，不调用 __index；借用转换沿用当前调用的锚点。
    ///
    /// # Safety
    /// T 遵守 FromLua 契约，不保存字段视图；借用结果仅在当前调用作用域使用。
    pub unsafe fn metatable_field<T: FromLua<'lua>>(
        self,
        name: &CStr,
    ) -> Result<Option<T>, String> {
        let lua_state = self.value.lua_state;
        let _restore = StackRestore::new(lua_state);
        if lua_checkstack(lua_state, 3) == 0 {
            return Err("insufficient Lua stack space".into());
        }
        if lua_getmetatable(lua_state, self.value.index) == 0 {
            return Ok(None);
        }
        lua_pushlstring(
            lua_state,
            name.to_bytes().as_ptr().cast(),
            name.to_bytes().len(),
        );
        lua_rawget(lua_state, -2);
        if lua_type(lua_state, -1) == LUA_TNIL {
            return Ok(None);
        }
        Value {
            index: lua_gettop(lua_state),
            temporary: true,
            ..self.value
        }
        .read()
        .map(Some)
    }

    /// # Safety
    /// 回调遵守 Value::read 的借用约束，不能修改正在遍历的表或使 VM 失效。
    /// 不能保存定位当前字段的 Value 视图；借出的字符串由调用作用域锚定，不依赖字段继续存在。
    pub unsafe fn visit(
        self,
        mut field: impl FnMut(&[u8], Value<'lua>) -> Result<(), String>,
    ) -> Result<(), String> {
        self.entries(|key, value| {
            if key.kind() == ValueKind::String {
                let mut len = 0;
                let pointer = lua_tolstring(key.lua_state, key.index, &mut len);
                field(std::slice::from_raw_parts(pointer.cast::<u8>(), len), value)?;
            }
            Ok(())
        })
    }

    /// 遍历全部键值，保留数值键和其他键的类型，不做隐式字符串转换。
    ///
    /// # Safety
    /// 回调不修改正在遍历的表，不保存键或值的字段视图；借用结果遵守调用作用域约束。
    pub unsafe fn entries(
        self,
        mut field: impl FnMut(Value<'lua>, Value<'lua>) -> Result<(), String>,
    ) -> Result<(), String> {
        let lua_state = self.value.lua_state;
        let _restore = StackRestore::new(lua_state);
        if lua_checkstack(lua_state, 3) == 0 {
            return Err("insufficient Lua stack space".into());
        }
        lua_pushnil(lua_state);
        while lua_next(lua_state, self.value.index) != 0 {
            let value = Value {
                index: lua_gettop(lua_state),
                temporary: true,
                ..self.value
            };
            let key = Value {
                index: value.index - 1,
                ..value
            };
            field(key, value)?;
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
        self.new_table(0, fields)
    }

    pub fn array(&mut self, elements: usize) -> TableWriter<'_, 'lua> {
        self.new_table(elements, 0)
    }

    fn new_table(&mut self, elements: usize, fields: usize) -> TableWriter<'_, 'lua> {
        unsafe {
            let restore = StackRestore::new(self.lua_state);
            assert_ne!(
                lua_checkstack(self.lua_state, 4),
                0,
                "insufficient Lua stack space"
            );
            lua_createtable(
                self.lua_state,
                i32::try_from(elements).expect("too many array elements"),
                i32::try_from(fields).expect("too many table fields"),
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
    /// value 遵守 PushLua 契约，输出一个值；下标从 1 开始。
    pub unsafe fn index<T: PushLua>(&mut self, index: usize, value: T) {
        let index = i32::try_from(index).expect("array index is too large");
        assert!(index > 0, "array indices start at 1");
        assert_eq!(
            self.output.push(value),
            1,
            "array elements must produce exactly one value"
        );
        lua_rawseti(self.output.lua_state, -2, index);
    }

    /// # Safety
    /// value 遵守 PushLua 契约，输出一张元表或 nil。设置元表后立即结束构造，避免触发字段元方法。
    pub unsafe fn finish_with_metatable<T: PushLua>(self, value: T) -> i32 {
        assert_eq!(
            self.output.push(value),
            1,
            "metatables must produce exactly one value"
        );
        assert!(
            matches!(lua_type(self.output.lua_state, -1), LUA_TTABLE | LUA_TNIL),
            "metatable must be a table or nil"
        );
        lua_setmetatable(self.output.lua_state, -2);
        self.finish()
    }
    /// # Safety
    /// value 遵守 PushLua 契约；非单值输出会被拒绝并撤销表构造。
    pub unsafe fn field<T: PushLua>(&mut self, name: &CStr, value: T) {
        let count = self.output.push(value);
        assert_eq!(count, 1, "table fields must produce exactly one value");
        lua_setfield(self.output.lua_state, -2, name.as_ptr());
    }

    /// 支持包含 NUL 的动态字符串键，供字符串 map 与派生字段共用表构造。
    ///
    /// # Safety
    /// value 遵守 PushLua 契约；非单值输出会被拒绝并撤销表构造。
    pub unsafe fn entry<T: PushLua>(&mut self, name: &str, value: T) {
        let lua_state = self.output.lua_state;
        lua_pushlstring(lua_state, name.as_ptr().cast(), name.len());
        assert_eq!(
            self.output.push(value),
            1,
            "table fields must produce exactly one value"
        );
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
