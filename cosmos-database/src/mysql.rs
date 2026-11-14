use percent_encoding::percent_decode_str;
use url::{Host, Url};

pub(super) struct Mysql;

#[derive(cosmos::PushLua)]
struct Options {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    charset: String,
    ssl: bool,
    ssl_verify: Option<bool>,
}

fn decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
        {
            return Err("MySQL URL 包含非法百分号转义".into());
        }
    }
    let decoded = percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| "MySQL URL 字段必须是 UTF-8")?;
    if decoded.contains('\0') {
        return Err("MySQL URL 字段不能包含 NUL".into());
    }
    Ok(decoded.into_owned())
}

#[cosmos::userdata(name = "database.mysql")]
impl Mysql {
    fn parse_url(value: &str) -> Result<Options, String> {
        // 错误不携带原始 URL，避免密码进入日志。
        let url = Url::parse(value).map_err(|_| "MySQL URL 格式错误")?;
        if url.scheme() != "mysql" || url.fragment().is_some() {
            return Err("需要不带 fragment 的 mysql:// URL".into());
        }
        let host = match url.host().ok_or("MySQL URL 缺少主机")? {
            Host::Domain(host) => host.to_owned(),
            Host::Ipv4(host) => host.to_string(),
            Host::Ipv6(host) => host.to_string(),
        };
        let database = decode(url.path().strip_prefix('/').unwrap_or(url.path()))?;
        if database.contains('/') {
            return Err("MySQL URL 只接受一级数据库名".into());
        }
        let mut options = Options {
            host,
            port: url.port().unwrap_or(3306),
            user: decode(url.username())?,
            password: decode(url.password().unwrap_or(""))?,
            database,
            charset: "utf8mb4".into(),
            ssl: false,
            ssl_verify: None,
        };
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "charset" => options.charset = value.into_owned(),
                "ssl" | "ssl_verify" => {
                    let enabled = match value.as_ref() {
                        "true" | "1" => true,
                        "false" | "0" => false,
                        _ => return Err("MySQL URL 的 TLS 开关必须是 true/false 或 1/0".into()),
                    };
                    if key == "ssl" {
                        options.ssl = enabled;
                    } else {
                        options.ssl_verify = Some(enabled);
                    }
                }
                _ => return Err("MySQL URL 包含不支持的选项".into()),
            }
        }
        Ok(options)
    }
}
