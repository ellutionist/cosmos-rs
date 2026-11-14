//! YAML 通过受限的 Serde 访问器转换，不隐式字符串化映射键。

use crate::value::{self, Budget, Decoded, DocumentValue, Input, MAX_BYTES, MAX_DEPTH, MAX_NODES};
use cosmos::userdata::LuaValue;
use serde::{
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    ser::{SerializeMap, SerializeSeq},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::fmt;

pub(super) struct Yaml;

#[cosmos::userdata(name = "encoding.yaml")]
impl Yaml {
    fn encode(input: Input) -> Result<String, String> {
        value::encoded(
            serde_saphyr::to_string(&YamlValue(&input.0?)).map_err(|error| error.to_string())?,
        )
    }

    fn decode(text: &[u8], array_meta: LuaValue, object_meta: LuaValue) -> Result<Decoded, String> {
        let text = value::text(text)?;
        check_scalars(text)?;
        let options = serde_saphyr::options! {
            budget: serde_saphyr::budget! {
                max_depth: MAX_DEPTH,
                max_nodes: MAX_NODES,
                max_events: MAX_NODES * 3,
                max_documents: 1,
                max_aliases: 10_000,
                max_anchors: 10_000,
                max_total_scalar_bytes: MAX_BYTES,
            },
            alias_limits: serde_saphyr::alias_limits! {
                max_total_replayed_events: MAX_NODES,
                max_replay_stack_depth: MAX_DEPTH,
            },
            duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
            strict_booleans: true,
            reject_unsupported_tags: true,
            emit_comments: false,
            with_snippet: false,
        };
        let value = serde_saphyr::from_str_with_options::<DocumentValue>(text, options)
            .map_err(|error| error.to_string())?;
        Ok(Decoded {
            value,
            array_meta,
            object_meta,
            lossless: false,
        })
    }
}

// typeless Serde 对超范围整数可能回退到 f64；先检查原始标量，避免精度静默丢失。
fn check_scalars(text: &str) -> Result<(), String> {
    use serde_saphyr::granit_parser::{self, Event, ScalarStyle};
    let options = granit_parser::options! {
        emit_comments: false,
        flow_nesting_limit: MAX_DEPTH,
        block_nesting_limit: MAX_DEPTH,
    };
    let mut parser = granit_parser::Parser::new_from_str_with_options(text, options);
    let mut nodes = 0;
    while let Some(event) = parser.next_event() {
        match event.map_err(|error| error.to_string())?.0 {
            Event::Scalar(text, style, anchor, tag) => {
                nodes += 1;
                if nodes > MAX_NODES || anchor > 10_000 {
                    return Err("YAML node or anchor limit exceeded".into());
                }
                let typed = tag.as_ref().is_some_and(|tag| {
                    tag.suffix_in_namespace("tag:yaml.org,2002:")
                        .is_some_and(|name| matches!(name.as_ref(), "str" | "float"))
                });
                let integer_tag = tag.as_ref().is_some_and(|tag| {
                    tag.suffix_in_namespace("tag:yaml.org,2002:")
                        .is_some_and(|name| name == "int")
                });
                if (style != ScalarStyle::Plain && !integer_tag) || typed {
                    continue;
                }
                let text = text.trim();
                let (negative, text) = if let Some(text) = text.strip_prefix('-') {
                    (true, text)
                } else {
                    (false, text.strip_prefix('+').unwrap_or(text))
                };
                let (radix, digits) = if let Some(text) = text.strip_prefix("0x") {
                    (16, text)
                } else if let Some(text) = text.strip_prefix("0o") {
                    (8, text)
                } else if let Some(text) = text.strip_prefix("0b") {
                    (2, text)
                } else {
                    (10, text)
                };
                if !digits.is_empty()
                    && digits
                        .split('_')
                        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_digit(radix)))
                {
                    let magnitude =
                        u128::from_str_radix(&digits.replace('_', ""), radix).map_err(|_| {
                            "YAML integer is out of i64/u64 range; use a string instead"
                        })?;
                    let maximum = if negative {
                        (i64::MAX as u128) + 1
                    } else {
                        u64::MAX as u128
                    };
                    if magnitude > maximum {
                        return Err(
                            "YAML integer is out of i64/u64 range; use a string instead".into()
                        );
                    }
                }
            }
            Event::SequenceStart(_, anchor, _) | Event::MappingStart(_, anchor, _) => {
                nodes += 1;
                if anchor > 10_000 {
                    return Err("YAML anchor limit exceeded".into());
                }
            }
            _ => {}
        }
        if nodes > MAX_NODES {
            return Err("YAML node limit exceeded".into());
        }
    }
    Ok(())
}

struct YamlValue<'a>(&'a DocumentValue);

impl Serialize for YamlValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            DocumentValue::Null => serializer.serialize_unit(),
            DocumentValue::Boolean(value) => serializer.serialize_bool(*value),
            DocumentValue::Integer(value) => serializer.serialize_i64(*value),
            DocumentValue::Unsigned(value) => serializer.serialize_u64(*value),
            DocumentValue::Float(value) => {
                if !value.is_finite() {
                    return Err(serde::ser::Error::custom(
                        "YAML does not support non-finite floats",
                    ));
                }
                serializer.serialize_f64(*value)
            }
            DocumentValue::String(value) => serializer.serialize_str(value),
            DocumentValue::Datetime(value) => serializer.serialize_str(&value.0.to_string()),
            DocumentValue::Exact(value) => {
                let integer = value.as_integer().map_err(serde::ser::Error::custom)?;
                if let Ok(value) = i64::try_from(integer) {
                    serializer.serialize_i64(value)
                } else {
                    serializer
                        .serialize_u64(u64::try_from(integer).map_err(serde::ser::Error::custom)?)
                }
            }
            DocumentValue::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&YamlValue(value))?;
                }
                seq.end()
            }
            DocumentValue::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, &YamlValue(value))?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for DocumentValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Seed {
            budget: &mut Budget::default(),
            depth: 0,
        }
        .deserialize(deserializer)
    }
}

struct Seed<'a> {
    budget: &'a mut Budget,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = DocumentValue;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.budget.node(self.depth).map_err(de::Error::custom)?;
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = DocumentValue;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a YAML value representable in Lua")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(DocumentValue::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(DocumentValue::Null)
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(DocumentValue::Boolean(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(DocumentValue::Integer(value))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(DocumentValue::Unsigned(value))
    }
    fn visit_i128<E: de::Error>(self, value: i128) -> Result<Self::Value, E> {
        if let Ok(value) = i64::try_from(value) {
            Ok(DocumentValue::Integer(value))
        } else {
            u64::try_from(value)
                .map(DocumentValue::Unsigned)
                .map_err(E::custom)
        }
    }
    fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
        u64::try_from(value)
            .map(DocumentValue::Unsigned)
            .map_err(E::custom)
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(DocumentValue::Float(value))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        self.budget.bytes(value.len()).map_err(E::custom)?;
        Ok(DocumentValue::String(value.to_owned()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        self.budget.bytes(value.len()).map_err(E::custom)?;
        Ok(DocumentValue::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        if self.depth >= MAX_DEPTH {
            return Err(de::Error::custom("document nesting depth limit exceeded"));
        }
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            budget: self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(DocumentValue::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        if self.depth >= MAX_DEPTH {
            return Err(de::Error::custom("document nesting depth limit exceeded"));
        }
        let mut values = Vec::new();
        while let Some(key) = map.next_key_seed(Key(self.budget))? {
            let value = map.next_value_seed(Seed {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.push((key, value));
        }
        Ok(DocumentValue::Object(values))
    }
}

struct Key<'a>(&'a mut Budget);
impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = String;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_any(self)
    }
}
impl Visitor<'_> for Key<'_> {
    type Value = String;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string mapping key")
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        self.0.bytes(value.len()).map_err(E::custom)?;
        Ok(value.to_owned())
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
        self.0.bytes(value.len()).map_err(E::custom)?;
        Ok(value)
    }
}
