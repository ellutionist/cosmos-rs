//! 通用文件响应：单字节范围、条件请求与 HEAD，不决定文件访问权限或 MIME。
use super::body::{Body, BodyValue};
use super::server::HttpRequest;
use super::{Headers, LuaBytes};
use cosmos::userdata::FileReader;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct HttpFile;
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct Options {
    content_type: Option<String>,
    etag: Option<String>,
    cache_control: Option<String>,
    close_file: Option<bool>,
}
#[derive(cosmos::PushLua)]
struct FileResponse {
    status: u16,
    headers: Headers,
    body: BodyValue,
}
#[derive(Debug, PartialEq, Eq)]
enum Range {
    Full,
    Partial(u64, u64),
    Unsatisfiable,
}
fn range(value: Option<&str>, length: u64) -> Range {
    let Some((unit, value)) = value.and_then(|v| v.split_once('=')) else {
        return Range::Full;
    };
    if !unit.eq_ignore_ascii_case("bytes") {
        return Range::Full;
    }
    if value.contains(',') {
        return Range::Full;
    }
    let Some((start, end)) = value.split_once('-') else {
        return Range::Full;
    };
    let number = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            Some(s.parse::<u64>().unwrap_or(u64::MAX))
        }
    };
    if start.is_empty() {
        return match number(end) {
            Some(0) => Range::Unsatisfiable,
            Some(_) if length == 0 => Range::Unsatisfiable,
            Some(n) => Range::Partial(length.saturating_sub(n), length - 1),
            None => Range::Full,
        };
    }
    let Some(start) = number(start) else {
        return Range::Full;
    };
    let end = if end.is_empty() {
        u64::MAX
    } else {
        let Some(end) = number(end) else {
            return Range::Full;
        };
        end
    };
    if end < start {
        return Range::Full;
    }
    if start >= length {
        Range::Unsatisfiable
    } else {
        Range::Partial(start, end.min(length - 1))
    }
}
fn tag_matches(header: &str, current: Option<&str>, weak: bool) -> bool {
    if header.trim() == "*" {
        return true;
    }
    let Some(current) = current else {
        return false;
    };
    let strip = |value: &str| value.strip_prefix("W/").unwrap_or(value).to_owned();
    let mut quoted = false;
    let mut start = 0;
    for (index, byte) in header.bytes().chain(std::iter::once(b',')).enumerate() {
        if byte == b'"' {
            quoted = !quoted;
        }
        if byte == b',' && !quoted {
            let candidate = header[start..index].trim();
            start = index + 1;
            if weak && strip(candidate) == strip(current) {
                return true;
            }
            if !weak
                && !candidate.starts_with("W/")
                && !current.starts_with("W/")
                && candidate == current
            {
                return true;
            }
        }
    }
    false
}
fn seconds(value: SystemTime) -> Option<u64> {
    value.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}
fn date(value: Option<String>) -> Option<u64> {
    value
        .and_then(|v| httpdate::parse_http_date(&v).ok())
        .and_then(seconds)
}
#[cosmos::userdata(name = "network.http.file")]
impl HttpFile {
    async fn response(
        file: FileReader,
        request: HttpRequest,
        options: Option<Options>,
    ) -> Result<FileResponse, String> {
        let options = options.unwrap_or_default();
        if options.etag.as_ref().is_some_and(|tag| {
            let tag = tag.strip_prefix("W/").unwrap_or(tag);
            tag.len() < 2
                || !tag.starts_with('"')
                || !tag.ends_with('"')
                || tag[1..tag.len() - 1]
                    .bytes()
                    .any(|b| b == b'"' || b < 0x21 || b == 0x7f)
        }) {
            return Err("invalid entity tag".into());
        }
        let (length, modified) = file.info().await?;
        let close = options.close_file.unwrap_or(true);
        let mut headers = vec![
            ("accept-ranges".into(), "bytes".into()),
            (
                "content-type".into(),
                options
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".into()),
            ),
        ];
        let etag = options.etag.or_else(|| {
            modified.map(|time| {
                let stamp = match time.duration_since(UNIX_EPOCH) {
                    Ok(d) => d.as_nanos().to_string(),
                    Err(e) => format!("-{}", e.duration().as_nanos()),
                };
                format!("W/\"{length}-{stamp}\"")
            })
        });
        if let Some(tag) = &etag {
            headers.push(("etag".into(), tag.clone()));
        }
        // Last-Modified 不得晚于产生响应的时间；ETag 仍使用实际元数据。
        let modified = modified.map(|time| time.min(SystemTime::now()));
        let modified_seconds = modified.and_then(seconds);
        if let Some(time) = modified.filter(|t| seconds(*t).is_some_and(|s| s <= 253402300799)) {
            headers.push(("last-modified".into(), httpdate::fmt_http_date(time)));
        }
        if let Some(cache) = options.cache_control {
            headers.push(("cache-control".into(), cache));
        }
        let method = request.method();
        let failed_precondition = match request.header("if-match".into()) {
            Some(header) => !tag_matches(&header, etag.as_deref(), false),
            None => date(request.header("if-unmodified-since".into()))
                .zip(modified_seconds)
                .is_some_and(|(given, mtime)| mtime > given),
        };
        let status = if method != "GET" && method != "HEAD" {
            headers.push(("allow".into(), "GET, HEAD".into()));
            Some(405)
        } else if failed_precondition {
            Some(412)
        } else if let Some(header) = request.header("if-none-match".into()) {
            if tag_matches(&header, etag.as_deref(), true) {
                Some(304)
            } else {
                None
            }
        } else if date(request.header("if-modified-since".into()))
            .zip(modified_seconds)
            .is_some_and(|(given, mtime)| mtime <= given)
        {
            Some(304)
        } else {
            None
        };
        if let Some(status) = status {
            if close {
                file.close();
            }
            return Ok(FileResponse {
                status,
                headers: Headers(headers),
                body: BodyValue::default(),
            });
        }
        let allow_range = match request.header("if-range".into()) {
            None => true,
            Some(value) if value.starts_with('"') || value.starts_with("W/") => {
                etag.as_ref().is_some_and(|tag| {
                    !tag.starts_with("W/") && !value.starts_with("W/") && *tag == value
                })
            }
            // 文件系统 mtime 无法证明一秒内只修改一次，HTTP 日期不能作为强验证器。
            Some(_) => false,
        };
        let requested = if method == "HEAD" || !allow_range {
            Range::Full
        } else {
            range(request.header("range".into()).as_deref(), length)
        };
        let (status, offset, count) = match requested {
            Range::Full => (200, 0, length),
            Range::Partial(start, end) => {
                headers.push((
                    "content-range".into(),
                    format!("bytes {start}-{end}/{length}"),
                ));
                (206, start, end - start + 1)
            }
            Range::Unsatisfiable => {
                headers.push(("content-range".into(), format!("bytes */{length}")));
                if close {
                    file.close();
                }
                return Ok(FileResponse {
                    status: 416,
                    headers: Headers(headers),
                    body: BodyValue::default(),
                });
            }
        };
        headers.push(("content-length".into(), count.to_string()));
        let body = if method == "HEAD" {
            if close {
                file.close();
            }
            BodyValue::Bytes(LuaBytes::default())
        } else {
            BodyValue::Stream(Body::file_region(file, length, offset, Some(count), close)?)
        };
        Ok(FileResponse {
            status,
            headers: Headers(headers),
            body,
        })
    }
}
