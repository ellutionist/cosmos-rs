//! WHATWG URL 值；修改返回新值，HTTP 请求与 Lua 接口复用解析/合并语义。
use crate::bridge::{UrlItemSize, UrlItems, UrlList};
use cosmos::userdata::LuaBytes;
use std::sync::Arc;
use url::{Origin, Url as NativeUrl};
pub(crate) const MAX_URL_BYTES: usize = 64 * 1024;
pub(crate) const MAX_URL_ITEMS: usize = 1024;
#[derive(Clone)]
pub(crate) struct HttpUrl(Arc<NativeUrl>);
#[derive(cosmos::FromLua)]
struct InputPair {
    name: LuaBytes,
    value: LuaBytes,
}
impl UrlItemSize for InputPair {
    fn url_bytes(&self) -> usize {
        self.name.len().saturating_add(self.value.len())
    }
}
#[derive(cosmos::PushLua)]
struct Pair {
    name: String,
    value: String,
}
fn check_input(text: &str) -> Result<(), String> {
    if text.len() > MAX_URL_BYTES {
        return Err("URL input exceeds 64 KiB".into());
    }
    Ok(())
}
fn text(value: &LuaBytes) -> Result<&str, String> {
    if value.len() > MAX_URL_BYTES {
        return Err("URL input exceeds 64 KiB".into());
    }
    std::str::from_utf8(value).map_err(|_| "URL input must be valid UTF-8".into())
}
fn checked(url: NativeUrl) -> Result<NativeUrl, String> {
    if url.as_str().len() > MAX_URL_BYTES {
        return Err("serialized URL exceeds 64 KiB".into());
    }
    Ok(url)
}
pub(super) fn parse(text: &str) -> Result<NativeUrl, String> {
    check_input(text)?;
    checked(NativeUrl::parse(text).map_err(|err| format!("invalid URL: {err}"))?)
}
pub(super) fn join(base: &NativeUrl, reference: &str) -> Result<NativeUrl, String> {
    check_input(reference)?;
    checked(
        base.join(reference)
            .map_err(|err| format!("invalid URL reference: {err}"))?,
    )
}
impl HttpUrl {
    fn changed(
        &self,
        edit: impl FnOnce(&mut NativeUrl) -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut url = (*self.0).clone();
        edit(&mut url)?;
        checked(url).map(|value| Self(Arc::new(value)))
    }
}
#[cosmos::userdata(name = "network.http.url")]
impl HttpUrl {
    fn parse(value: LuaBytes) -> Result<Self, String> {
        parse(text(&value)?).map(|value| Self(Arc::new(value)))
    }
    fn string(&self) -> &str {
        self.0.as_str()
    }
    fn scheme(&self) -> &str {
        self.0.scheme()
    }
    fn username(&self) -> &str {
        self.0.username()
    }
    fn password(&self) -> Option<&str> {
        self.0.password()
    }
    fn host(&self) -> Option<&str> {
        self.0.host_str()
    }
    fn port(&self) -> Option<u16> {
        self.0.port()
    }
    fn port_or_default(&self) -> Option<u16> {
        self.0.port_or_known_default()
    }
    fn path(&self) -> &str {
        self.0.path()
    }
    fn query(&self) -> Option<&str> {
        self.0.query()
    }
    fn fragment(&self) -> Option<&str> {
        self.0.fragment()
    }
    fn can_be_base(&self) -> bool {
        !self.0.cannot_be_a_base()
    }
    fn origin(&self) -> Option<String> {
        match self.0.origin() {
            origin @ Origin::Tuple(..) => Some(origin.ascii_serialization()),
            Origin::Opaque(_) => None,
        }
    }
    fn same_origin(&self, other: HttpUrl) -> bool {
        matches!((self.0.origin(),other.0.origin()),(a @ Origin::Tuple(..),b @ Origin::Tuple(..)) if a==b)
    }
    fn join(&self, reference: LuaBytes) -> Result<Self, String> {
        join(&self.0, text(&reference)?).map(|value| Self(Arc::new(value)))
    }
    fn path_segments(&self) -> Result<UrlList<String>, String> {
        let segments = self
            .0
            .path_segments()
            .ok_or("URL has no hierarchical path")?;
        let mut out = Vec::new();
        for segment in segments {
            if out.len() == MAX_URL_ITEMS {
                return Err("URL path exceeds 1024 segments".into());
            }
            out.push(segment.to_owned());
        }
        Ok(UrlList(out))
    }
    fn query_pairs(&self) -> Result<UrlList<Pair>, String> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        for (name, value) in self.0.query_pairs() {
            if out.len() == MAX_URL_ITEMS {
                return Err("URL query exceeds 1024 pairs".into());
            }
            bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
            if bytes > MAX_URL_BYTES {
                return Err("decoded URL query exceeds 64 KiB".into());
            }
            out.push(Pair {
                name: name.into_owned(),
                value: value.into_owned(),
            });
        }
        Ok(UrlList(out))
    }
    fn with_scheme(&self, scheme: LuaBytes) -> Result<Self, String> {
        let scheme = text(&scheme)?;
        self.changed(|url| {
            url.set_scheme(scheme)
                .map_err(|_| "invalid URL scheme change".into())
        })
    }
    fn with_host(&self, host: Option<LuaBytes>) -> Result<Self, String> {
        let host = host.as_ref().map(text).transpose()?;
        self.changed(|url| {
            url.set_host(host)
                .map_err(|err| format!("invalid URL host: {err}"))
        })
    }
    fn with_port(&self, port: Option<f64>) -> Result<Self, String> {
        let port = port
            .map(|value| {
                if !value.is_finite() || value.fract() != 0.0 || !(0.0..=65535.0).contains(&value) {
                    return Err("URL port must be an integer in 0..=65535".to_owned());
                }
                Ok(value as u16)
            })
            .transpose()?;
        self.changed(|url| {
            url.set_port(port)
                .map_err(|_| "URL cannot have a port".into())
        })
    }
    fn with_username(&self, username: LuaBytes) -> Result<Self, String> {
        let username = text(&username)?;
        self.changed(|url| {
            url.set_username(username)
                .map_err(|_| "URL cannot have a username".into())
        })
    }
    fn with_password(&self, password: Option<LuaBytes>) -> Result<Self, String> {
        let password = password.as_ref().map(text).transpose()?;
        self.changed(|url| {
            url.set_password(password)
                .map_err(|_| "URL cannot have a password".into())
        })
    }
    fn with_path(&self, path: LuaBytes) -> Result<Self, String> {
        let path = text(&path)?;
        self.changed(|url| {
            url.set_path(path);
            Ok(())
        })
    }
    fn with_path_segments(&self, segments: UrlItems<LuaBytes>) -> Result<Self, String> {
        let items = segments.0?;
        let segments = items.iter().map(text).collect::<Result<Vec<_>, _>>()?;
        self.changed(|url| {
            url.path_segments_mut()
                .map_err(|_| "URL has no hierarchical path")?
                .clear()
                .extend(segments);
            Ok(())
        })
    }
    fn with_query(&self, query: Option<LuaBytes>) -> Result<Self, String> {
        let query = query.as_ref().map(text).transpose()?;
        self.changed(|url| {
            url.set_query(query);
            Ok(())
        })
    }
    fn with_query_pairs(&self, pairs: UrlItems<InputPair>) -> Result<Self, String> {
        let items = pairs.0?;
        let pairs = items
            .iter()
            .map(|p| Ok((text(&p.name)?, text(&p.value)?)))
            .collect::<Result<Vec<_>, String>>()?;
        self.changed(|url| {
            url.query_pairs_mut().clear().extend_pairs(pairs);
            Ok(())
        })
    }
    fn append_query_pair(&self, name: LuaBytes, value: LuaBytes) -> Result<Self, String> {
        let name = text(&name)?;
        let value = text(&value)?;
        self.changed(|url| {
            url.query_pairs_mut().append_pair(name, value);
            Ok(())
        })
    }
    fn with_fragment(&self, fragment: Option<LuaBytes>) -> Result<Self, String> {
        let fragment = fragment.as_ref().map(text).transpose()?;
        self.changed(|url| {
            url.set_fragment(fragment);
            Ok(())
        })
    }
}
