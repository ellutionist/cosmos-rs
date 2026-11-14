//! 两种数据库共用 URL 叶子转换，错误不回显带凭据的原始地址。

use percent_encoding::percent_decode_str;
use url::{Host, Url};

pub(super) fn decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
        {
            return Err("数据库 URL 包含非法百分号转义".into());
        }
    }
    let decoded = percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| "数据库 URL 字段必须是 UTF-8")?;
    if decoded.contains('\0') {
        return Err("数据库 URL 字段不能包含 NUL".into());
    }
    Ok(decoded.into_owned())
}

pub(super) fn host(url: &Url) -> Result<String, String> {
    Ok(match url.host().ok_or("数据库 URL 缺少主机")? {
        Host::Domain(host) => host.to_owned(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => host.to_string(),
    })
}

pub(super) fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err("数据库 URL 的 TLS 开关必须是 true/false 或 1/0".into()),
    }
}
