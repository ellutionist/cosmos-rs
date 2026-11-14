//! 不可变原文与紧凑节点索引；词法位置独立保存，避免合并文本时丢失 CDATA 边界。
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use xmlparser::{ElementEnd, Token, Tokenizer};

pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 64;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Document,
    Element,
    Text,
    Cdata,
    Comment,
    Pi,
}
impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Element => "element",
            Self::Text => "text",
            Self::Cdata => "cdata",
            Self::Comment => "comment",
            Self::Pi => "pi",
        }
    }
}
#[derive(Clone, cosmos::PushLua)]
pub(super) struct Attribute {
    pub(super) name: String,
    pub(super) local_name: String,
    pub(super) prefix: Option<String>,
    pub(super) namespace: Option<String>,
    pub(super) value: String,
}
// 命名空间 URI 在内部驻留共享，避免巨大的继承 URI 被每个节点复制。
pub(super) struct Attr {
    pub(super) name: String,
    pub(super) local: String,
    pub(super) namespace: Option<Arc<str>>,
    pub(super) value: String,
}
pub(super) type Bindings = BTreeMap<String, Arc<str>>;
pub(super) struct Record {
    pub(super) kind: Kind,
    pub(super) range: Range<usize>,
    pub(super) open_end: usize,
    pub(super) parent: Option<usize>,
    pub(super) children: Vec<usize>,
    pub(super) name: String,
    pub(super) value: String,
    pub(super) namespace: Option<Arc<str>>,
    pub(super) attrs: Vec<Attr>,
    pub(super) bindings: Bindings,
}
pub(super) struct Source {
    pub(super) text: String,
    pub(super) nodes: Vec<Record>,
    pub(super) synthetic: bool,
    pub(super) hidden_root: bool,
}
pub(super) fn append(out: &mut String, text: &str) -> Result<(), String> {
    if out.len().saturating_add(text.len()) > MAX_BYTES {
        return Err("XML output exceeds 16 MiB".into());
    }
    out.push_str(text);
    Ok(())
}
pub(super) fn escape(text: &str, attribute: bool) -> Result<String, String> {
    let mut out = String::new();
    for ch in text.chars() {
        if !matches!(ch,'\u{9}'|'\u{a}'|'\u{d}'|'\u{20}'..='\u{d7ff}'|'\u{e000}'..='\u{fffd}'|'\u{10000}'..='\u{10ffff}')
        {
            return Err("invalid XML 1.0 character".into());
        }
        let mut bytes = [0; 4];
        append(
            &mut out,
            match ch {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' if attribute => "&quot;",
                '\r' => "&#xD;",
                '\n' if attribute => "&#xA;",
                '\t' if attribute => "&#x9;",
                _ => ch.encode_utf8(&mut bytes),
            },
        )?;
    }
    Ok(out)
}
fn normalized(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
fn decoded(text: &str) -> Result<String, String> {
    let text = normalized(text);
    let mut rest = text.as_str();
    let mut out = String::new();
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let end = rest.find(';').ok_or("unterminated XML reference")?;
        let entity = &rest[..end];
        let value = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "apos" => '\'',
            "quot" => '"',
            _ => {
                let number = if let Some(v) = entity.strip_prefix("#x") {
                    u32::from_str_radix(v, 16).ok()
                } else {
                    entity.strip_prefix('#').and_then(|v| v.parse().ok())
                };
                number
                    .and_then(char::from_u32)
                    .ok_or("invalid XML reference")?
            }
        };
        out.push(value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
fn record(
    kind: Kind,
    range: Range<usize>,
    name: String,
    value: String,
    parent: Option<usize>,
) -> Record {
    Record {
        kind,
        range,
        open_end: 0,
        parent,
        children: Vec::new(),
        name,
        value,
        namespace: None,
        attrs: Vec::new(),
        bindings: Bindings::new(),
    }
}
fn add(nodes: &mut Vec<Record>, parent: usize, node: Record) -> Result<usize, String> {
    if nodes.len() > MAX_NODES {
        return Err("XML node limit exceeded".into());
    }
    let index = nodes.len();
    nodes[parent].children.push(index);
    nodes.push(node);
    Ok(index)
}
pub(super) fn parse(text: String, synthetic: bool) -> Result<Arc<Source>, String> {
    if text.len() > MAX_BYTES {
        return Err("XML input exceeds 16 MiB".into());
    }
    let mut nodes = vec![record(
        Kind::Document,
        0..text.len(),
        String::new(),
        String::new(),
        None,
    )];
    let mut stack = vec![0];
    let mut attributes = 0;
    // 先做流式大小/深度/DTD 检查，再让原生文档解析器验证完整结构和命名空间。
    for token in Tokenizer::from(text.as_str()) {
        let token = token.map_err(|err| format!("invalid XML: {err}"))?;
        match token {
            Token::DtdStart { .. }
            | Token::EmptyDtd { .. }
            | Token::EntityDeclaration { .. }
            | Token::DtdEnd { .. } => {
                return Err("XML DTD and custom entities are prohibited".into())
            }
            Token::Declaration {
                version, encoding, ..
            } => {
                if version.as_str() != "1.0"
                    || encoding.is_some_and(|e| !e.as_str().eq_ignore_ascii_case("utf-8"))
                {
                    return Err("XML requires version 1.0 and UTF-8 encoding".into());
                }
            }
            Token::ElementStart {
                prefix,
                local,
                span,
            } => {
                if stack.len() > MAX_DEPTH {
                    return Err("XML depth limit exceeded".into());
                }
                let name = if prefix.is_empty() {
                    local.to_string()
                } else {
                    format!("{prefix}:{local}")
                };
                let parent = *stack.last().ok_or("invalid XML nesting")?;
                let index = add(
                    &mut nodes,
                    parent,
                    record(
                        Kind::Element,
                        span.range(),
                        name,
                        String::new(),
                        Some(parent),
                    ),
                )?;
                stack.push(index);
            }
            Token::Attribute {
                prefix,
                local,
                value,
                ..
            } => {
                if prefix.as_str() == "xmlns" && local.as_str() == "xmlns" {
                    return Err("the xmlns prefix cannot be declared".into());
                }
                if prefix.as_str() == "xmlns" && value.is_empty() {
                    return Err("XML namespace prefixes cannot be undeclared".into());
                }
                attributes += 1;
                if attributes > MAX_NODES {
                    return Err("XML attribute limit exceeded".into());
                }
            }
            Token::ElementEnd { end, span } => {
                let index = *stack.last().ok_or("invalid XML nesting")?;
                match end {
                    ElementEnd::Open => nodes[index].open_end = span.end(),
                    ElementEnd::Empty => {
                        nodes[index].open_end = span.end();
                        nodes[index].range.end = span.end();
                        stack.pop();
                    }
                    ElementEnd::Close(..) => {
                        nodes[index].range.end = span.end();
                        stack.pop();
                    }
                }
            }
            token => {
                let range = token.span().range();
                let (kind, name, value) = match token {
                    Token::Text { text } => (Kind::Text, String::new(), decoded(text.as_str())?),
                    Token::Cdata { text, .. } => {
                        (Kind::Cdata, String::new(), normalized(text.as_str()))
                    }
                    Token::Comment { text, .. } => {
                        (Kind::Comment, String::new(), normalized(text.as_str()))
                    }
                    Token::ProcessingInstruction {
                        target, content, ..
                    } => {
                        if content.is_some_and(|value| value.start() == target.end()) {
                            return Err(
                                "XML PI content requires whitespace after its target".into()
                            );
                        }
                        if target.as_str().contains(':') {
                            return Err("XML PI target must be an NCName".into());
                        }
                        if target.as_str().eq_ignore_ascii_case("xml") {
                            return Err("XML PI target cannot be xml".into());
                        }
                        (
                            Kind::Pi,
                            target.to_string(),
                            content.map(|v| normalized(v.as_str())).unwrap_or_default(),
                        )
                    }
                    _ => unreachable!("all XML token variants are handled"),
                };
                let parent = *stack.last().ok_or("invalid XML nesting")?;
                add(
                    &mut nodes,
                    parent,
                    record(kind, range, name, value, Some(parent)),
                )?;
            }
        }
    }
    let document = roxmltree::Document::parse_with_options(
        &text,
        roxmltree::ParsingOptions {
            nodes_limit: MAX_NODES as u32 + 1,
            ..Default::default()
        },
    )
    .map_err(|err| format!("invalid XML: {err}"))?;
    let elements: HashMap<_, _> = document
        .descendants()
        .filter(|n| n.is_element())
        .map(|n| (n.range().start, n))
        .collect();
    let mut uris: HashMap<&str, Arc<str>> = HashMap::new();
    for node in nodes.iter_mut().filter(|n| n.kind == Kind::Element) {
        let native = elements
            .get(&node.range.start)
            .ok_or("XML element position mismatch")?;
        node.namespace = native
            .tag_name()
            .namespace()
            .filter(|v| !v.is_empty())
            .map(|v| uris.entry(v).or_insert_with(|| Arc::from(v)).clone());
        for attr in native.attributes() {
            node.attrs.push(Attr {
                name: text[attr.range_qname()].to_owned(),
                local: attr.name().into(),
                namespace: attr
                    .namespace()
                    .map(|v| uris.entry(v).or_insert_with(|| Arc::from(v)).clone()),
                value: attr.value().into(),
            });
        }
        // 只保存当前标签声明的绑定，不为每个后代复制全部继承命名空间。
        for token in Tokenizer::from_fragment(&text, node.range.start..node.open_end) {
            if let Token::Attribute { prefix, local, .. } =
                token.map_err(|err| format!("invalid XML: {err}"))?
            {
                let name = if prefix.as_str() == "xmlns" {
                    Some(local.as_str())
                } else if prefix.is_empty() && local.as_str() == "xmlns" {
                    Some("")
                } else {
                    None
                };
                if let Some(name) = name {
                    let uri = if name.is_empty() {
                        native.default_namespace().unwrap_or("")
                    } else {
                        native.lookup_namespace_uri(Some(name)).unwrap_or("")
                    };
                    node.bindings.insert(
                        name.into(),
                        uris.entry(uri).or_insert_with(|| Arc::from(uri)).clone(),
                    );
                }
            }
        }
    }
    drop(document);
    Ok(Arc::new(Source {
        text,
        nodes,
        synthetic,
        hidden_root: false,
    }))
}
impl Source {
    pub(super) fn bindings(&self, index: usize) -> Bindings {
        let mut chain = Vec::new();
        let mut next = Some(index);
        while let Some(index) = next {
            chain.push(index);
            next = self.nodes[index].parent;
        }
        let mut bindings = Bindings::new();
        bindings.insert(String::new(), Arc::from(""));
        bindings.insert(
            "xml".into(),
            Arc::from("http://www.w3.org/XML/1998/namespace"),
        );
        for index in chain.into_iter().rev() {
            bindings.extend(
                self.nodes[index]
                    .bindings
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        bindings
    }
    pub(super) fn fragment(&self, index: usize, destination: &Bindings) -> Result<String, String> {
        let node = &self.nodes[index];
        let raw = &self.text[node.range.clone()];
        if node.kind != Kind::Element {
            return Ok(raw.into());
        }
        let mut declarations = String::new();
        for (prefix, uri) in self.bindings(index) {
            if prefix == "xml" {
                continue;
            }
            let dest = destination.get(&prefix).map(|v| v.as_ref()).unwrap_or("");
            if !node.bindings.contains_key(&prefix) && dest != uri.as_ref() {
                append(
                    &mut declarations,
                    if prefix.is_empty() {
                        " xmlns=\""
                    } else {
                        " xmlns:"
                    },
                )?;
                if !prefix.is_empty() {
                    append(&mut declarations, &prefix)?;
                    append(&mut declarations, "=\"")?;
                }
                append(&mut declarations, &escape(&uri, true)?)?;
                append(&mut declarations, "\"")?;
            }
        }
        if declarations.is_empty() {
            return Ok(raw.into());
        }
        let position = node.open_end
            - node.range.start
            - if self.text[..node.open_end].ends_with("/>") {
                2
            } else {
                1
            };
        let mut out = String::new();
        append(&mut out, &raw[..position])?;
        append(&mut out, &declarations)?;
        append(&mut out, &raw[position..])?;
        Ok(out)
    }
}
