//! 二进制编解码与结构化文本转换插件，通过 Machine 注入。
#![warn(unreachable_pub)]

mod json;
mod toml;
mod value;
mod xml;
mod yaml;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use cosmos::{Machine, Module};
use percent_encoding::{percent_decode, percent_encode, AsciiSet, NON_ALPHANUMERIC};

const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// 注册 cosmos.encoding 下的编解码模块。
pub struct Encoding;

impl Module for Encoding {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Hex>();
        machine.register::<Base64>();
        machine.register::<Uri>();
        json::register(machine)?;
        machine.register::<toml::Toml>();
        machine.register::<toml::Datetime>();
        machine.register::<yaml::Yaml>();
        xml::register(machine);
        machine.load(
            "cosmos.encoding.formats",
            include_str!("../lua/formats.lua"),
        )
    }
}

struct Hex;

#[cosmos::userdata(name = "encoding.hex")]
impl Hex {
    fn encode(data: &[u8]) -> String {
        hex::encode(data)
    }

    fn decode(data: &[u8]) -> Result<Vec<u8>, String> {
        hex::decode(data).map_err(|error| error.to_string())
    }
}

struct Base64;

#[cosmos::userdata(name = "encoding.base64")]
impl Base64 {
    fn encode(data: &[u8]) -> String {
        STANDARD.encode(data)
    }

    fn decode(data: &[u8]) -> Result<Vec<u8>, String> {
        STANDARD.decode(data).map_err(|error| error.to_string())
    }

    fn encode_urlsafe(data: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(data)
    }

    fn decode_urlsafe(data: &[u8]) -> Result<Vec<u8>, String> {
        URL_SAFE_NO_PAD
            .decode(data)
            .map_err(|error| error.to_string())
    }
}

struct Uri;

#[cosmos::userdata(name = "encoding.uri")]
impl Uri {
    /// 编码 URI 组件，仅保留 RFC 3986 的 unreserved 字符。
    fn encode(data: &[u8]) -> String {
        percent_encode(data, URI_COMPONENT).to_string()
    }

    /// 严格校验百分号转义；加号是字面值，不作为表单空格处理。
    fn decode(data: &[u8]) -> Result<Vec<u8>, String> {
        let bytes = data;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'%'
                && !bytes
                    .get(index + 1..index + 3)
                    .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
            {
                return Err(format!("invalid percent escape at byte {}", index + 1));
            }
        }
        Ok(percent_decode(bytes).collect())
    }
}
