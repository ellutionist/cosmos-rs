//! 受控视图转为拥有型字段树；文本键值渲染，不使用 JSON 输出。
use cosmos::userdata::{FromLua, Value, ValueKind};
use std::collections::BTreeMap;
pub(super) type Object = BTreeMap<String, Field>;
#[derive(Clone)]
pub(super) enum Field {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Field>),
    Object(Object),
}
fn key(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    {
        value.into()
    } else {
        format!("{value:?}")
    }
}
pub(super) fn text(fields: &Object) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", key(name), value.text()))
        .collect::<Vec<_>>()
        .join(" ")
}
impl Field {
    fn text(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(v) => v.to_string(),
            Self::Number(v) => v.to_string(),
            Self::String(v) => format!("{v:?}"),
            Self::Array(items) => format!(
                "[{}]",
                items.iter().map(Self::text).collect::<Vec<_>>().join(", ")
            ),
            Self::Object(fields) => format!("({})", text(fields)),
        }
    }
}
pub(super) struct Fields(pub(super) Result<Object, String>);
impl Default for Fields {
    fn default() -> Self {
        Self(Ok(Object::new()))
    }
}
struct Budget {
    nodes: usize,
    bytes: usize,
}
impl Budget {
    fn bytes(&mut self, n: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > 65536 {
            Err("log fields exceed 64 KiB of input strings".into())
        } else {
            Ok(())
        }
    }
    fn node(&mut self, depth: usize) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > 4096 || depth > 16 {
            Err("log fields exceed node or depth limit".into())
        } else {
            Ok(())
        }
    }
}
unsafe fn read(value: Value<'_>, depth: usize, budget: &mut Budget) -> Result<Field, String> {
    budget.node(depth)?;
    match value.kind() {
        ValueKind::Nil | ValueKind::Null => Ok(Field::Null),
        ValueKind::Boolean => Ok(Field::Bool(value.read::<bool>()?)),
        ValueKind::Number => {
            let n = value.read::<f64>()?;
            if !n.is_finite() {
                return Err("log fields must contain finite numbers".into());
            }
            Ok(Field::Number(n))
        }
        ValueKind::String => {
            let s = value.read::<&str>()?;
            budget.bytes(s.len())?;
            Ok(Field::String(s.to_owned()))
        }
        ValueKind::Table => {
            let mut object = Object::new();
            let mut array = BTreeMap::new();
            value.table()?.entries(|key, value| {
                match key.kind() {
                    ValueKind::String => {
                        let name = key.read::<&str>()?;
                        budget.bytes(name.len())?;
                        object.insert(name.to_owned(), read(value, depth + 1, budget)?);
                    }
                    ValueKind::Number => {
                        let n = key.read::<f64>()?;
                        if !n.is_finite() || n.fract() != 0.0 || !(1.0..=4096.0).contains(&n) {
                            return Err("log arrays require consecutive integer keys".into());
                        }
                        array.insert(n as usize, read(value, depth + 1, budget)?);
                    }
                    _ => return Err("log fields require string keys or array indices".into()),
                }
                Ok(())
            })?;
            if !array.is_empty() {
                if !object.is_empty() || array.keys().copied().ne(1..=array.len()) {
                    return Err("log arrays must not contain holes or mixed keys".into());
                }
                Ok(Field::Array(array.into_values().collect()))
            } else {
                Ok(Field::Object(object))
            }
        }
        _ => Err("unsupported log field type".into()),
    }
}
fn size(value: &Field, depth: usize, budget: &mut Budget) -> Result<(), String> {
    budget.node(depth)?;
    match value {
        Field::String(s) => budget.bytes(s.len())?,
        Field::Array(a) => {
            for v in a {
                size(v, depth + 1, budget)?
            }
        }
        Field::Object(o) => {
            for (k, v) in o {
                budget.bytes(k.len())?;
                size(v, depth + 1, budget)?
            }
        }
        _ => {}
    }
    Ok(())
}
pub(super) fn checked(fields: Object) -> Result<Object, String> {
    let mut budget = Budget { nodes: 0, bytes: 0 };
    for (name, value) in &fields {
        budget.bytes(name.len())?;
        size(value, 1, &mut budget)?;
    }
    if text(&fields).len() > 128 * 1024 {
        return Err("encoded log fields exceed 128 KiB".into());
    }
    Ok(fields)
}
impl<'lua> FromLua<'lua> for Fields {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let result = (|| match read(value, 0, &mut Budget { nodes: 0, bytes: 0 })? {
            Field::Object(fields) => checked(fields),
            _ => Err("log fields must be an object".into()),
        })();
        Ok(Self(result))
    }
}
