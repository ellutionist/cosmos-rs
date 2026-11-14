//! 文本编解码共用的 Lua 值转换；只组合受控桥接，不操作裸栈。

use crate::json::Number;
use crate::toml::Datetime;
use cosmos::userdata::{FromLua, LuaValue, Null, Output, PushLua, TableIdentity, Value, ValueKind};
use std::collections::HashSet;

pub(super) const MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_NODES: usize = 1_000_000;
pub(super) const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub(super) enum DocumentValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    Exact(Number),
    Datetime(Datetime),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

pub(super) struct Input(pub(super) Result<DocumentValue, String>);

#[derive(Default)]
pub(super) struct Budget {
    nodes: usize,
    bytes: usize,
}
impl Budget {
    pub(super) fn node(&mut self, depth: usize) -> Result<(), String> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err("文档嵌套深度或节点数超限".into());
        }
        Ok(())
    }
    pub(super) fn bytes(&mut self, count: usize) -> Result<(), String> {
        self.bytes = self.bytes.checked_add(count).ok_or("文档字节数超限")?;
        if self.bytes > MAX_BYTES {
            return Err("文档字节数超限".into());
        }
        Ok(())
    }
}

impl<'lua> FromLua<'lua> for Input {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        Ok(Self(read(
            value,
            0,
            &mut HashSet::new(),
            &mut Budget::default(),
        )))
    }
}

pub(super) fn text(input: &[u8]) -> Result<&str, String> {
    if input.len() > MAX_BYTES {
        return Err("文档字节数超限".into());
    }
    std::str::from_utf8(input).map_err(|error| error.to_string())
}

pub(super) fn encoded(text: String) -> Result<String, String> {
    if text.len() > MAX_BYTES {
        return Err("文档字节数超限".into());
    }
    Ok(text)
}

unsafe fn read(
    value: Value<'_>,
    depth: usize,
    path: &mut HashSet<TableIdentity>,
    budget: &mut Budget,
) -> Result<DocumentValue, String> {
    budget.node(depth)?;
    match value.kind() {
        ValueKind::Nil | ValueKind::Null => Ok(DocumentValue::Null),
        ValueKind::Boolean => Ok(DocumentValue::Boolean(value.read::<bool>()?)),
        ValueKind::Number => {
            let number = value.read::<f64>()?;
            if number.fract() == 0.0
                && number.abs() <= MAX_SAFE_INTEGER
                && !(number == 0.0 && number.is_sign_negative())
            {
                Ok(DocumentValue::Integer(number as i64))
            } else {
                Ok(DocumentValue::Float(number))
            }
        }
        ValueKind::String => {
            let text = value.read::<&str>()?;
            budget.bytes(text.len())?;
            Ok(DocumentValue::String(text.to_owned()))
        }
        ValueKind::UserData => {
            if let Ok(number) = value.read::<Number>() {
                budget.bytes(number.text().len())?;
                Ok(DocumentValue::Exact(number))
            } else if let Ok(datetime) = value.read::<Datetime>() {
                Ok(DocumentValue::Datetime(datetime))
            } else {
                Err("不支持该 userdata".into())
            }
        }
        ValueKind::Table => {
            if depth >= MAX_DEPTH {
                return Err("文档嵌套深度超限".into());
            }
            let table = value.table()?;
            let identity = table.identity();
            if !path.insert(identity) {
                return Err("不支持循环引用".into());
            }
            let result = read_table(value, depth, path, budget);
            path.remove(&identity);
            result
        }
        _ => Err("不支持该值类型或缺少实参".into()),
    }
}

unsafe fn read_table(
    value: Value<'_>,
    depth: usize,
    path: &mut HashSet<TableIdentity>,
    budget: &mut Budget,
) -> Result<DocumentValue, String> {
    let hint = value.table()?.metatable_field::<String>(c"__jsontype")?;
    if hint
        .as_deref()
        .is_some_and(|kind| kind != "array" && kind != "object")
    {
        return Err("无效的容器类型标记".into());
    }
    let mut object = Vec::new();
    let mut array = Vec::new();
    value.table()?.entries(|key, item| {
        match key.kind() {
            ValueKind::String => {
                if !array.is_empty() || hint.as_deref() == Some("array") {
                    return Err("数组不能包含字符串键".into());
                }
                let name = key.read::<&str>()?;
                budget.bytes(name.len())?;
                object.push((name.to_owned(), read(item, depth + 1, path, budget)?));
            }
            ValueKind::Number => {
                if !object.is_empty() || hint.as_deref() == Some("object") {
                    return Err("对象只接受字符串键".into());
                }
                let index = key.read::<f64>()?;
                if index < 1.0 || index > MAX_NODES as f64 || index.fract() != 0.0 {
                    return Err("数组下标必须是连续的正整数".into());
                }
                array.push((index as usize, read(item, depth + 1, path, budget)?));
            }
            _ => return Err("表只接受字符串键或连续整数下标".into()),
        }
        Ok(())
    })?;
    if !array.is_empty() || hint.as_deref() == Some("array") {
        array.sort_unstable_by_key(|(index, _)| *index);
        let mut values = Vec::with_capacity(array.len());
        for (position, (index, value)) in array.into_iter().enumerate() {
            if index != position + 1 {
                return Err("不支持稀疏数组，请使用 cosmos.null 填充".into());
            }
            values.push(value);
        }
        Ok(DocumentValue::Array(values))
    } else {
        Ok(DocumentValue::Object(object))
    }
}

pub(super) struct Decoded {
    pub(super) value: DocumentValue,
    pub(super) array_meta: LuaValue,
    pub(super) object_meta: LuaValue,
    pub(super) lossless: bool,
}

impl PushLua for Decoded {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        Node {
            value: &self.value,
            context: &self,
        }
        .push_lua(output)
    }
}
struct Node<'a> {
    value: &'a DocumentValue,
    context: &'a Decoded,
}
impl PushLua for Node<'_> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        match self.value {
            DocumentValue::Null => Null.push_lua(output),
            DocumentValue::Boolean(value) => (*value).push_lua(output),
            DocumentValue::String(value) => value.as_str().push_lua(output),
            DocumentValue::Integer(value)
                if !self.context.lossless && (*value as f64).abs() <= MAX_SAFE_INTEGER =>
            {
                (*value).push_lua(output)
            }
            DocumentValue::Unsigned(value)
                if !self.context.lossless && *value <= MAX_SAFE_INTEGER as u64 =>
            {
                (*value as f64).push_lua(output)
            }
            DocumentValue::Integer(value) => {
                Number::parse(&value.to_string()).unwrap().push_lua(output)
            }
            DocumentValue::Unsigned(value) => {
                Number::parse(&value.to_string()).unwrap().push_lua(output)
            }
            DocumentValue::Float(value) => (*value).push_lua(output),
            DocumentValue::Exact(value) => {
                if !self.context.lossless {
                    if let Some(number) = lua_number(value.text()) {
                        return number.push_lua(output);
                    }
                }
                value.clone().push_lua(output)
            }
            DocumentValue::Datetime(value) => value.clone().push_lua(output),
            DocumentValue::Array(values) => {
                let mut table = output.array(values.len());
                for (index, item) in values.iter().enumerate() {
                    table.index(
                        index + 1,
                        Node {
                            value: item,
                            context: self.context,
                        },
                    );
                }
                table.finish_with_metatable(&self.context.array_meta)
            }
            DocumentValue::Object(values) => {
                let mut table = output.table(values.len());
                for (name, item) in values {
                    table.entry(
                        name,
                        Node {
                            value: item,
                            context: self.context,
                        },
                    );
                }
                table.finish_with_metatable(&self.context.object_meta)
            }
        }
    }
}

fn lua_number(text: &str) -> Option<f64> {
    let number = text.parse::<f64>().ok()?;
    if !number.is_finite() {
        return None;
    }
    if number == 0.0
        && text
            .split(['e', 'E'])
            .next()?
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9'))
    {
        return None;
    }
    if !text.contains(['.', 'e', 'E']) && number.abs() > MAX_SAFE_INTEGER {
        return None;
    }
    Some(number)
}
