use crate::{append, bridge::List, input, integer, utf8, work};
use cosmos::userdata::{LuaBytes, LuaFunction, Nil};
use cosmos::Machine;
use regex::{Regex, RegexBuilder};
use std::{
    ops::Range,
    sync::{atomic::AtomicBool, Arc},
};
const MAX_PATTERN: usize = 64 * 1024;
const MAX_MATCHES: usize = 10_000;
const MAX_SLOTS: usize = 1_000_000;
#[derive(Clone)]
struct Pattern(Arc<Regex>);
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct Options {
    case_insensitive: bool,
    multi_line: bool,
    dot_matches_new_line: bool,
    ignore_whitespace: bool,
    swap_greed: bool,
}
#[derive(cosmos::PushLua)]
struct CaptureName {
    index: u32,
    name: Option<String>,
}
#[derive(cosmos::FromLua)]
enum Selector {
    #[lua(type = "number")]
    Index(f64),
    #[lua(type = "string")]
    Name(String),
}
#[derive(cosmos::FromLua)]
enum Replacement {
    #[lua(type = "string")]
    Template(LuaBytes),
    #[lua(type = "function")]
    Callback(LuaFunction),
}
#[derive(cosmos::FromLua)]
enum Reply {
    #[lua(type = "string")]
    Text(LuaBytes),
    #[lua(type = "boolean")]
    Keep(bool),
    #[lua(type = "nil")]
    Nil(Nil, Option<String>),
    #[lua(type = "none")]
    Nothing,
}
#[derive(Clone)]
struct Match(Arc<Matched>);
struct Matched {
    text: Arc<str>,
    pattern: Arc<Regex>,
    groups: Box<[Option<Range<usize>>]>,
}
struct Matches {
    items: Vec<Match>,
    cursor: usize,
}
pub(super) fn register(machine: &mut Machine) {
    machine.register::<Pattern>();
    machine.register::<Match>();
    machine.register::<Matches>();
}
fn limit(value: Option<f64>) -> Result<usize, String> {
    integer(value.unwrap_or(0.0), 0, MAX_MATCHES, "match limit")
}
fn collect(
    pattern: Arc<Regex>,
    text: Arc<str>,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<Matches, String> {
    let mut items = Vec::new();
    for captures in pattern.captures_iter(&text) {
        work::check(cancel)?;
        if items.len() == MAX_MATCHES || (items.len() + 1) * pattern.captures_len() > MAX_SLOTS {
            return Err("regex results exceed match or capture slot limit".into());
        }
        items.push(Match(Arc::new(Matched {
            text: text.clone(),
            pattern: pattern.clone(),
            groups: captures
                .iter()
                .map(|group| group.map(|m| m.range()))
                .collect(),
        })));
        if limit > 0 && items.len() == limit {
            break;
        }
    }
    Ok(Matches { items, cursor: 0 })
}
impl Match {
    fn index(&self, selector: Option<Selector>) -> Result<usize, String> {
        match selector {
            None => Ok(0),
            Some(Selector::Index(value)) => {
                integer(value, 0, self.0.groups.len() - 1, "capture index")
            }
            Some(Selector::Name(name)) => self
                .0
                .pattern
                .capture_names()
                .position(|n| n == Some(name.as_str()))
                .ok_or_else(|| "unknown capture name".into()),
        }
    }
    fn slice(&self, index: usize) -> Option<&str> {
        self.0
            .groups
            .get(index)?
            .as_ref()
            .map(|r| &self.0.text[r.clone()])
    }
    fn whole(&self) -> &Range<usize> {
        self.0.groups[0]
            .as_ref()
            .expect("full match must be present")
    }
    fn expand(&self, template: &str, out: &mut String) -> Result<(), String> {
        let mut overflow = false;
        // 复用正则引擎自身的模板语法，在捕获组展开前检查预算。
        // 模板中的字面量最多额外写入 MAX_PATTERN，展开后再次检查。
        regex_automata::util::interpolate::string(
            template,
            |index, out| {
                if let Some(value) = self.slice(index) {
                    if append(out, value).is_err() {
                        overflow = true;
                    }
                }
            },
            |name| self.0.pattern.capture_names().position(|n| n == Some(name)),
            out,
        );
        if overflow || out.len() > crate::MAX_OUTPUT {
            return Err("text output exceeds 16 MiB".into());
        }
        Ok(())
    }
}
#[cosmos::userdata(name = "text.regex.match")]
impl Match {
    fn text(&self) -> &str {
        self.slice(0).expect("full match must be present")
    }
    fn group(&self, selector: Option<Selector>) -> Result<Option<&str>, String> {
        Ok(self.slice(self.index(selector)?))
    }
    fn range(&self, selector: Option<Selector>) -> Result<Option<(u32, u32)>, String> {
        Ok(self.0.groups[self.index(selector)?]
            .as_ref()
            .map(|r| ((r.start + 1) as u32, r.end as u32)))
    }
    fn capture_count(&self) -> u32 {
        (self.0.groups.len() - 1) as u32
    }
}
#[cosmos::userdata(name = "text.regex.matches")]
impl Matches {
    fn len(&self) -> u32 {
        self.items.len() as u32
    }
    fn get(&self, index: f64) -> Result<Option<Match>, String> {
        let index = integer(index, 1, MAX_MATCHES, "match index")?;
        Ok(self.items.get(index - 1).cloned())
    }
    fn next(&mut self) -> Option<Match> {
        let value = self.items.get(self.cursor).cloned();
        if value.is_some() {
            self.cursor += 1;
        }
        value
    }
}
#[cosmos::userdata(name = "text.regex")]
impl Pattern {
    async fn new(pattern: LuaBytes, options: Option<Options>) -> Result<Self, String> {
        if pattern.len() > MAX_PATTERN {
            return Err("regex pattern exceeds 64 KiB".into());
        }
        let opts = options.unwrap_or_default();
        work::run(move || {
            let bytes = pattern.to_vec();
            move |_| {
                let pattern = utf8(bytes)?;
                let regex = RegexBuilder::new(&pattern)
                    .case_insensitive(opts.case_insensitive)
                    .multi_line(opts.multi_line)
                    .dot_matches_new_line(opts.dot_matches_new_line)
                    .ignore_whitespace(opts.ignore_whitespace)
                    .swap_greed(opts.swap_greed)
                    .size_limit(8 * 1024 * 1024)
                    .dfa_size_limit(2 * 1024 * 1024)
                    .nest_limit(128)
                    .build()
                    .map_err(|err| format!("invalid regex: {err}"))?;
                if regex.captures_len() > 256 {
                    return Err("regex exceeds 256 capture slots".into());
                }
                Ok(Self(Arc::new(regex)))
            }
        })
        .await
    }
    fn pattern(&self) -> &str {
        self.0.as_str()
    }
    fn capture_names(&self) -> List<CaptureName> {
        List(
            self.0
                .capture_names()
                .enumerate()
                .map(|(index, name)| CaptureName {
                    index: index as u32,
                    name: name.map(str::to_owned),
                })
                .collect(),
        )
    }
    async fn escape(text: LuaBytes) -> Result<String, String> {
        input(&text)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |_| {
                let out = regex::escape(&utf8(bytes)?);
                if out.len() > crate::MAX_OUTPUT {
                    return Err("text output exceeds 16 MiB".into());
                }
                Ok(out)
            }
        })
        .await
    }
    // 异步桥接克隆拥有型句柄，不能让 userdata 借用跨越 await。
    #[allow(clippy::wrong_self_convention)]
    async fn is_match(self, text: LuaBytes) -> Result<bool, String> {
        input(&text)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |_| Ok(self.0.is_match(&utf8(bytes)?))
        })
        .await
    }
    async fn find(self, text: LuaBytes, start: Option<f64>) -> Result<Option<Match>, String> {
        input(&text)?;
        let start = integer(start.unwrap_or(1.0), 1, text.len() + 1, "start position")? - 1;
        work::run(move || {
            let bytes = text.to_vec();
            move |_| {
                let text: Arc<str> = utf8(bytes)?.into();
                if !text.is_char_boundary(start) {
                    return Err("start position must be a UTF-8 boundary".into());
                }
                Ok(self.0.captures_at(&text, start).map(|captures| {
                    Match(Arc::new(Matched {
                        text: text.clone(),
                        pattern: self.0.clone(),
                        groups: captures
                            .iter()
                            .map(|group| group.map(|m| m.range()))
                            .collect(),
                    }))
                }))
            }
        })
        .await
    }
    async fn find_all(self, text: LuaBytes, max_matches: Option<f64>) -> Result<Matches, String> {
        input(&text)?;
        let max = limit(max_matches)?;
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| collect(self.0, utf8(bytes)?.into(), max, cancel)
        })
        .await
    }
    async fn replace(
        self,
        text: LuaBytes,
        replacement: Replacement,
        max_matches: Option<f64>,
    ) -> Result<(String, u32), String> {
        input(&text)?;
        let max = limit(max_matches)?;
        match replacement {
            Replacement::Template(template) => {
                if template.len() > MAX_PATTERN {
                    return Err("replacement template exceeds 64 KiB".into());
                }
                work::run(move || {
                    let bytes = text.to_vec();
                    let template = template.to_vec();
                    move |cancel| {
                        let text: Arc<str> = utf8(bytes)?.into();
                        let template = utf8(template)?;
                        let matches = collect(self.0, text.clone(), max, cancel)?;
                        let mut out = String::new();
                        let mut end = 0;
                        for item in &matches.items {
                            work::check(cancel)?;
                            append(&mut out, &text[end..item.whole().start])?;
                            item.expand(&template, &mut out)?;
                            end = item.whole().end;
                        }
                        append(&mut out, &text[end..])?;
                        Ok((out, matches.items.len() as u32))
                    }
                })
                .await
            }
            Replacement::Callback(callback) => {
                let (text, matches) = work::run(move || {
                    let bytes = text.to_vec();
                    move |cancel| {
                        let text: Arc<str> = utf8(bytes)?.into();
                        let matches = collect(self.0, text.clone(), max, cancel)?;
                        Ok((text, matches))
                    }
                })
                .await?;
                let mut out = String::new();
                let mut end = 0;
                let mut replaced = 0;
                for (index, item) in matches.items.into_iter().enumerate() {
                    append(&mut out, &text[end..item.whole().start])?;
                    let reply: Reply = callback.call(item.clone()).await?;
                    match reply {
                        Reply::Text(value) => {
                            if value.len() > crate::MAX_OUTPUT.saturating_sub(out.len()) {
                                return Err("text output exceeds 16 MiB".into());
                            }
                            let value = std::str::from_utf8(&value)
                                .map_err(|_| "replacement must be valid UTF-8")?;
                            append(&mut out, value)?;
                            replaced += 1;
                        }
                        Reply::Nil(_, Some(err)) => return Err(err),
                        Reply::Keep(true) => {
                            return Err(
                                "replacement callback must return a string, nil or false".into()
                            )
                        }
                        _ => append(&mut out, item.text())?,
                    }
                    end = item.whole().end;
                    if index % 128 == 127 {
                        tokio::task::yield_now().await;
                    }
                }
                append(&mut out, &text[end..])?;
                Ok((out, replaced))
            }
        }
    }
    async fn split(self, text: LuaBytes, max_parts: Option<f64>) -> Result<List<String>, String> {
        input(&text)?;
        let max = integer(max_parts.unwrap_or(0.0), 0, MAX_MATCHES, "split limit")?;
        work::run(move || {
            let bytes = text.to_vec();
            move |cancel| {
                let text = utf8(bytes)?;
                let mut out = Vec::new();
                let parts = self
                    .0
                    .splitn(&text, if max == 0 { MAX_MATCHES + 1 } else { max });
                for part in parts {
                    work::check(cancel)?;
                    if out.len() == MAX_MATCHES {
                        return Err("regex split exceeds 10000 parts".into());
                    }
                    out.push(part.to_owned());
                }
                Ok(List(out))
            }
        })
        .await
    }
}
