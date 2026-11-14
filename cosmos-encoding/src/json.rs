//! sonic-rs 编解码；Lua 转换仅组合受控值、表和现有基层类型。

use std::collections::HashSet;

use cosmos::userdata::{FromLua, LuaValue, Null, Output, PushLua, TableIdentity, Value, ValueKind};
use cosmos::Machine;
use sonic_rs::{
    Array, JsonContainerTrait, JsonType, JsonValueTrait, LazyValue, Object, RawNumber,
    Value as JsonValue,
};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_NODES: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub(super) fn register(machine: &mut Machine) -> Result<(), String> {
    machine.register::<Json>();
    machine.register::<JsonNumber>();
    machine.load("cosmos.encoding.json", include_str!("../lua/json.lua"))
}

struct Json;
struct Input(Result<JsonValue, String>);

#[derive(Default)]
struct Budget {
    nodes: usize,
    bytes: usize,
}

impl Budget {
    fn node(&mut self, depth: usize) -> Result<(), String> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err("JSON 嵌套深度或节点数超限".into());
        }
        Ok(())
    }
    fn bytes(&mut self, count: usize) -> Result<(), String> {
        self.bytes = self.bytes.checked_add(count).ok_or("JSON 字节数超限")?;
        if self.bytes > MAX_BYTES {
            return Err("JSON 字节数超限".into());
        }
        Ok(())
    }
}

impl<'lua> FromLua<'lua> for Input {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        // 数据错误通过 encode 的 nil+err 返回，参数转换不在中途抛 Lua 错误。
        Ok(Self(read(
            value,
            0,
            &mut HashSet::new(),
            &mut Budget::default(),
        )))
    }
}

unsafe fn read(
    value: Value<'_>,
    depth: usize,
    path: &mut HashSet<TableIdentity>,
    budget: &mut Budget,
) -> Result<JsonValue, String> {
    budget.node(depth)?;
    match value.kind() {
        ValueKind::Nil | ValueKind::Null => Ok(JsonValue::new_null()),
        ValueKind::Boolean => Ok(JsonValue::from(value.read::<bool>()?)),
        ValueKind::Number => {
            let number = value.read::<f64>()?;
            if number.fract() == 0.0
                && number.abs() <= MAX_SAFE_INTEGER
                && !(number == 0.0 && number.is_sign_negative())
            {
                Ok(JsonValue::from(number as i64))
            } else {
                JsonValue::new_f64(number).ok_or_else(|| "JSON 不支持 NaN 或无穷数".into())
            }
        }
        ValueKind::String => {
            let text = value.read::<&str>()?;
            budget.bytes(text.len())?;
            Ok(JsonValue::from(text))
        }
        ValueKind::UserData => {
            let number = value
                .read::<JsonNumber>()
                .map_err(|_| "JSON 不支持该 userdata")?;
            budget.bytes(number.0.as_str().len())?;
            sonic_rs::to_value(&number.0).map_err(|error| error.to_string())
        }
        ValueKind::Table => {
            if depth >= MAX_DEPTH {
                return Err("JSON 嵌套深度超限".into());
            }
            let table = value.table()?;
            let identity = table.identity();
            if !path.insert(identity) {
                return Err("JSON 不支持循环引用".into());
            }
            let result = read_table(value, depth, path, budget);
            path.remove(&identity);
            result
        }
        _ => Err("JSON 不支持该值类型或缺少实参".into()),
    }
}

unsafe fn read_table(
    value: Value<'_>,
    depth: usize,
    path: &mut HashSet<TableIdentity>,
    budget: &mut Budget,
) -> Result<JsonValue, String> {
    let hint = value.table()?.metatable_field::<String>(c"__jsontype")?;
    if hint
        .as_deref()
        .is_some_and(|kind| kind != "array" && kind != "object")
    {
        return Err("无效的 JSON 表类型标记".into());
    }
    let mut object = Object::new();
    let mut array = Vec::new();
    value.table()?.entries(|key, item| {
        match key.kind() {
            ValueKind::String => {
                if !array.is_empty() || hint.as_deref() == Some("array") {
                    return Err("JSON 数组不能包含字符串键".into());
                }
                let name = key.read::<&str>()?;
                budget.bytes(name.len())?;
                object.insert(name, read(item, depth + 1, path, budget)?);
            }
            ValueKind::Number => {
                if !object.is_empty() || hint.as_deref() == Some("object") {
                    return Err("JSON 对象只接受字符串键".into());
                }
                let index = key.read::<f64>()?;
                if index < 1.0 || index > MAX_NODES as f64 || index.fract() != 0.0 {
                    return Err("JSON 数组下标必须是连续的正整数".into());
                }
                array.push((index as usize, read(item, depth + 1, path, budget)?));
            }
            _ => return Err("JSON 表只接受字符串键或连续整数下标".into()),
        }
        Ok(())
    })?;
    if !array.is_empty() || hint.as_deref() == Some("array") {
        array.sort_unstable_by_key(|(index, _)| *index);
        let mut values = Array::with_capacity(array.len());
        for (position, (index, value)) in array.into_iter().enumerate() {
            if index != position + 1 {
                return Err("JSON 不支持稀疏数组，请使用 cosmos.null 填充".into());
            }
            values.push(value);
        }
        Ok(values.into())
    } else {
        Ok(object.into())
    }
}

#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct DecodeOptions {
    lossless_numbers: bool,
}

struct Decoded {
    value: JsonValue,
    array_meta: LuaValue,
    object_meta: LuaValue,
    lossless: bool,
}

#[cosmos::userdata(name = "encoding.json")]
impl Json {
    fn encode(value: Input) -> Result<String, String> {
        let text = sonic_rs::to_string(&value.0?).map_err(|error| error.to_string())?;
        if text.len() > MAX_BYTES {
            return Err("JSON 字节数超限".into());
        }
        Ok(text)
    }

    fn decode(
        text: &[u8],
        array_meta: LuaValue,
        object_meta: LuaValue,
        options: Option<DecodeOptions>,
    ) -> Result<Decoded, String> {
        if text.len() > MAX_BYTES {
            return Err("JSON 字节数超限".into());
        }
        let text = std::str::from_utf8(text).map_err(|error| error.to_string())?;
        check_depth(text.as_bytes())?;
        let raw = sonic_rs::from_str::<LazyValue<'_>>(text).map_err(|error| error.to_string())?;
        let value = parse(raw, 0, &mut Budget::default())?;
        Ok(Decoded {
            value,
            array_meta,
            object_meta,
            lossless: options.unwrap_or_default().lossless_numbers,
        })
    }
}

// 只在字符串外计数容器，不替代 sonic 的语法与 UTF-8 校验。
// 在构造及输出嵌套值前限制深度，避免耗尽宿主线程栈。
fn check_depth(text: &[u8]) -> Result<(), String> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in text {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err("JSON 嵌套深度超限".into());
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

// sonic 的惰性解析先验证完整文档；随后遍历容器，避免 DOM 解析器的深层递归栈。
fn parse(raw: LazyValue<'_>, depth: usize, budget: &mut Budget) -> Result<JsonValue, String> {
    budget.node(depth)?;
    match raw.get_type() {
        JsonType::Array => {
            let mut array = Array::new();
            for item in raw.into_array_iter().unwrap() {
                array.push(parse(
                    item.map_err(|error| error.to_string())?,
                    depth + 1,
                    budget,
                )?);
            }
            Ok(array.into())
        }
        JsonType::Object => {
            let mut object = Object::new();
            for item in raw.into_object_iter().unwrap() {
                let (name, item) = item.map_err(|error| error.to_string())?;
                object.insert(name.as_ref(), parse(item, depth + 1, budget)?);
            }
            Ok(object.into())
        }
        _ => sonic_rs::from_str(raw.as_raw_str()).map_err(|error| error.to_string()),
    }
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
    value: &'a JsonValue,
    context: &'a Decoded,
}

impl PushLua for Node<'_> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        match self.value.get_type() {
            JsonType::Null => Null.push_lua(output),
            JsonType::Boolean => self.value.as_bool().unwrap().push_lua(output),
            JsonType::String => self.value.as_str().unwrap().push_lua(output),
            JsonType::Number => {
                let raw = self
                    .value
                    .as_raw_number()
                    .expect("任意精度解析应保留数值文本");
                if !self.context.lossless {
                    if let Some(number) = lua_number(raw.as_str()) {
                        return number.push_lua(output);
                    }
                }
                JsonNumber(raw).push_lua(output)
            }
            JsonType::Array => {
                let array = self.value.as_array().unwrap();
                let mut table = output.array(array.len());
                for (index, item) in array.iter().enumerate() {
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
            JsonType::Object => {
                let object = self.value.as_object().unwrap();
                let mut table = output.table(object.len());
                for (name, item) in object.iter() {
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

#[derive(Clone)]
struct JsonNumber(RawNumber);

#[cosmos::userdata(name = "encoding.json.number")]
impl JsonNumber {
    fn new(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err("JSON 字节数超限".into());
        }
        if !matches!(
            text.trim_start().as_bytes().first(),
            Some(b'-' | b'0'..=b'9')
        ) {
            return Err("需要 JSON 数字文本".into());
        }
        let value = sonic_rs::from_str::<JsonValue>(text).map_err(|error| error.to_string())?;
        value
            .as_raw_number()
            .map(Self)
            .ok_or_else(|| "需要 JSON 数字文本".into())
    }
    fn text(&self) -> &str {
        self.0.as_str()
    }
    fn __tostring(&self) -> &str {
        self.0.as_str()
    }
}
