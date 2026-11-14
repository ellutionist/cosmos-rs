//! sonic-rs JSON 编解码；Lua 容器与精确数字复用公共值桥接。
use crate::value::{self, Budget, Decoded, DocumentValue, Input, MAX_BYTES, MAX_DEPTH};
use cosmos::userdata::LuaValue;
use cosmos::Machine;
use sonic_rs::{Array, JsonType, JsonValueTrait, LazyValue, Object, RawNumber, Value as JsonValue};

pub(super) fn register(machine: &mut Machine) -> Result<(), String> {
    machine.register::<Json>();
    machine.register::<Number>();
    machine.load("cosmos.encoding.json", include_str!("../lua/json.lua"))
}
struct Json;
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct DecodeOptions {
    lossless_numbers: bool,
}

#[cosmos::userdata(name = "encoding.json")]
impl Json {
    fn encode(value: Input) -> Result<String, String> {
        value::encoded(sonic_rs::to_string(&to_json(value.0?)?).map_err(|error| error.to_string())?)
    }

    fn decode(
        text: &[u8],
        array_meta: LuaValue,
        object_meta: LuaValue,
        options: Option<DecodeOptions>,
    ) -> Result<Decoded, String> {
        let text = value::text(text)?;
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
                        return Err("JSON nesting depth limit exceeded".into());
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
fn parse(raw: LazyValue<'_>, depth: usize, budget: &mut Budget) -> Result<DocumentValue, String> {
    budget.node(depth)?;
    match raw.get_type() {
        JsonType::Array => {
            let mut array = Vec::new();
            for item in raw.into_array_iter().unwrap() {
                array.push(parse(
                    item.map_err(|error| error.to_string())?,
                    depth + 1,
                    budget,
                )?);
            }
            Ok(DocumentValue::Array(array))
        }
        JsonType::Object => {
            let mut object = Vec::new();
            for item in raw.into_object_iter().unwrap() {
                let (name, item) = item.map_err(|error| error.to_string())?;
                object.push((name.into_owned(), parse(item, depth + 1, budget)?));
            }
            Ok(DocumentValue::Object(object))
        }
        JsonType::Null => Ok(DocumentValue::Null),
        JsonType::Boolean => Ok(DocumentValue::Boolean(raw.as_bool().unwrap())),
        JsonType::String => {
            let text = sonic_rs::from_str::<String>(raw.as_raw_str())
                .map_err(|error| error.to_string())?;
            budget.bytes(text.len())?;
            Ok(DocumentValue::String(text))
        }
        JsonType::Number => Number::parse(raw.as_raw_str()).map(DocumentValue::Exact),
    }
}

fn to_json(value: DocumentValue) -> Result<JsonValue, String> {
    Ok(match value {
        DocumentValue::Null => JsonValue::new_null(),
        DocumentValue::Boolean(value) => JsonValue::from(value),
        DocumentValue::Integer(value) => JsonValue::from(value),
        DocumentValue::Unsigned(value) => JsonValue::from(value),
        DocumentValue::Float(value) => {
            JsonValue::new_f64(value).ok_or("JSON does not support NaN or infinity")?
        }
        DocumentValue::String(value) => JsonValue::from(value.as_str()),
        DocumentValue::Exact(value) => {
            sonic_rs::to_value(&value.0).map_err(|error| error.to_string())?
        }
        DocumentValue::Datetime(_) => {
            return Err(
                "JSON does not support TOML datetimes; convert them to strings first".into(),
            )
        }
        DocumentValue::Array(values) => {
            let mut array = Array::with_capacity(values.len());
            for value in values {
                array.push(to_json(value)?);
            }
            array.into()
        }
        DocumentValue::Object(values) => {
            let mut object = Object::new();
            for (name, value) in values {
                object.insert(&name, to_json(value)?);
            }
            object.into()
        }
    })
}

#[derive(Clone)]
pub(super) struct Number(RawNumber);

#[cosmos::userdata(name = "encoding.json.number")]
impl Number {
    fn new(text: &str) -> Result<Self, String> {
        Self::parse(text)
    }
    pub(super) fn text(&self) -> &str {
        self.0.as_str()
    }
    fn __tostring(&self) -> &str {
        self.0.as_str()
    }
}

impl Number {
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err("JSON byte limit exceeded".into());
        }
        if !matches!(
            text.trim_start().as_bytes().first(),
            Some(b'-' | b'0'..=b'9')
        ) {
            return Err("expected JSON number text".into());
        }
        let value = sonic_rs::from_str::<JsonValue>(text).map_err(|error| error.to_string())?;
        value
            .as_raw_number()
            .map(Self)
            .ok_or_else(|| "expected JSON number text".into())
    }
    pub(super) fn as_integer(&self) -> Result<i128, String> {
        self.0
            .as_str()
            .parse()
            .map_err(|_| "this format only supports exact integer userdata; use Lua numbers for floating-point values".into())
    }
}
