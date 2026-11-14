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
            return Err("database URL contains invalid percent escapes".into());
        }
    }
    let decoded = percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| "database URL fields must be valid UTF-8")?;
    if decoded.contains('\0') {
        return Err("database URL fields must not contain NUL".into());
    }
    Ok(decoded.into_owned())
}

pub(super) fn host(url: &Url) -> Result<String, String> {
    Ok(match url.host().ok_or("database URL is missing a host")? {
        Host::Domain(host) => host.to_owned(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => host.to_string(),
    })
}

pub(super) fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err("database URL TLS option must be true/false or 1/0".into()),
    }
}
