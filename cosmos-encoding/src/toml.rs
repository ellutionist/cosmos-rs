//! TOML 的原生类型转换，日期时间保持独立类型。

use crate::value::{self, Budget, Decoded, DocumentValue, Input, MAX_DEPTH};
use ::toml::{de::DeValue, Value as TomlValue};
use cosmos::userdata::LuaValue;

pub(super) struct Toml;

#[derive(Clone)]
pub(super) struct Datetime(pub(super) ::toml::value::Datetime);

#[cosmos::userdata(name = "encoding.toml.datetime")]
impl Datetime {
    fn new(text: &str) -> Result<Self, String> {
        text.parse()
            .map(Self)
            .map_err(|error: ::toml::value::DatetimeParseError| error.to_string())
    }
    fn text(&self) -> String {
        self.0.to_string()
    }
    fn __tostring(&self) -> String {
        self.0.to_string()
    }
}

#[cosmos::userdata(name = "encoding.toml")]
impl Toml {
    fn encode(input: Input) -> Result<String, String> {
        let input = input.0?;
        if !matches!(input, DocumentValue::Object(_)) {
            return Err("TOML 文档根必须是字符串键表".into());
        }
        value::encoded(::toml::to_string(&to_toml(input)?).map_err(|error| error.to_string())?)
    }

    fn decode(text: &[u8], array_meta: LuaValue, object_meta: LuaValue) -> Result<Decoded, String> {
        // 使用原生解析树，避免 Serde 日期时间私有字段名与普通配置键冲突。
        let parsed =
            ::toml::de::DeTable::parse(value::text(text)?).map_err(|error| error.to_string())?;
        let parsed = DeValue::Table(parsed.into_inner());
        let value = from_toml(parsed, 0, &mut Budget::default())?;
        Ok(Decoded {
            value,
            array_meta,
            object_meta,
            lossless: false,
        })
    }
}

fn to_toml(value: DocumentValue) -> Result<TomlValue, String> {
    Ok(match value {
        DocumentValue::Null => return Err("TOML 不支持 null".into()),
        DocumentValue::Boolean(value) => TomlValue::Boolean(value),
        DocumentValue::Integer(value) => TomlValue::Integer(value),
        DocumentValue::Unsigned(value) => {
            TomlValue::Integer(i64::try_from(value).map_err(|_| "TOML 整数超出 i64 范围")?)
        }
        DocumentValue::Float(value) => TomlValue::Float(value),
        DocumentValue::Exact(value) => TomlValue::Integer(
            i64::try_from(value.as_integer()?).map_err(|_| "TOML 整数超出 i64 范围")?,
        ),
        DocumentValue::Datetime(value) => TomlValue::Datetime(value.0),
        DocumentValue::String(value) => TomlValue::String(value),
        DocumentValue::Array(values) => {
            TomlValue::Array(values.into_iter().map(to_toml).collect::<Result<_, _>>()?)
        }
        DocumentValue::Object(values) => TomlValue::Table(
            values
                .into_iter()
                .map(|(name, value)| Ok((name, to_toml(value)?)))
                .collect::<Result<_, String>>()?,
        ),
    })
}

fn from_toml(
    value: DeValue<'_>,
    depth: usize,
    budget: &mut Budget,
) -> Result<DocumentValue, String> {
    budget.node(depth)?;
    Ok(match value {
        DeValue::String(value) => {
            budget.bytes(value.len())?;
            DocumentValue::String(value.into_owned())
        }
        DeValue::Integer(value) => DocumentValue::Integer(
            i64::from_str_radix(value.as_str(), value.radix())
                .map_err(|_| "TOML 整数超出 i64 范围")?,
        ),
        DeValue::Float(value) => {
            let number = value
                .as_str()
                .parse::<f64>()
                .map_err(|error| error.to_string())?;
            if number.is_infinite() && !value.as_str().contains("inf") {
                return Err("TOML 浮点数超出 f64 范围".into());
            }
            DocumentValue::Float(number)
        }
        DeValue::Boolean(value) => DocumentValue::Boolean(value),
        DeValue::Datetime(value) => DocumentValue::Datetime(Datetime(value)),
        DeValue::Array(values) => {
            if depth >= MAX_DEPTH {
                return Err("文档嵌套深度超限".into());
            }
            DocumentValue::Array(
                values
                    .into_iter()
                    .map(|value| from_toml(value.into_inner(), depth + 1, budget))
                    .collect::<Result<_, _>>()?,
            )
        }
        DeValue::Table(values) => {
            if depth >= MAX_DEPTH {
                return Err("文档嵌套深度超限".into());
            }
            DocumentValue::Object(
                values
                    .into_iter()
                    .map(|(key, value)| {
                        budget.bytes(key.get_ref().len())?;
                        Ok((
                            key.into_inner().into_owned(),
                            from_toml(value.into_inner(), depth + 1, budget)?,
                        ))
                    })
                    .collect::<Result<_, String>>()?,
            )
        }
    })
}
