use url::Url;

use super::options::{boolean, decode, host};

pub(super) struct Redis;

#[derive(cosmos::PushLua)]
struct Options {
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    db: i64,
    ssl: bool,
    ssl_verify: Option<bool>,
}

#[cosmos::userdata(name = "database.redis")]
impl Redis {
    fn parse_url(value: &str) -> Result<Options, String> {
        let url = Url::parse(value).map_err(|_| "Redis URL 格式错误")?;
        if !matches!(url.scheme(), "redis" | "rediss") || url.fragment().is_some() {
            return Err("需要不带 fragment 的 redis:// 或 rediss:// URL".into());
        }
        let path = decode(url.path().strip_prefix('/').unwrap_or(url.path()))?;
        let db = if path.is_empty() {
            0
        } else {
            if !path.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("Redis 数据库编号必须是非负整数".into());
            }
            path.parse::<i64>().map_err(|_| "Redis 数据库编号过大")?
        };
        if db > i32::MAX as i64 {
            return Err("Redis 数据库编号过大".into());
        }
        let mut options = Options {
            host: host(&url)?,
            port: url.port().unwrap_or(6379),
            username: decode(url.username())?,
            password: url.password().map(decode).transpose()?,
            db,
            ssl: url.scheme() == "rediss",
            ssl_verify: None,
        };
        if !options.username.is_empty() && options.password.is_none() {
            return Err("Redis URL 的用户名需要配套密码".into());
        }
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "ssl_verify" => options.ssl_verify = Some(boolean(&value)?),
                _ => return Err("Redis URL 包含不支持的选项".into()),
            }
        }
        Ok(options)
    }
}
