use url::Url;

use super::options::{boolean, decode, host};

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

#[cosmos::userdata(name = "database.mysql")]
impl Mysql {
    fn parse_url(value: &str) -> Result<Options, String> {
        // 错误不携带原始 URL，避免密码进入日志。
        let url = Url::parse(value).map_err(|_| "MySQL URL 格式错误")?;
        if url.scheme() != "mysql" || url.fragment().is_some() {
            return Err("需要不带 fragment 的 mysql:// URL".into());
        }
        let host = host(&url)?;
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
                    let enabled = boolean(&value)?;
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
