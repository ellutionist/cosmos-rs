//! XML 原文文档与节点；编辑返回新文档，旧节点继续读取旧版本。
mod bridge;
mod model;
use bridge::{InputList, InputSize, List};
use cosmos::userdata::{LuaBytes, LuaStr};
use model::{append, escape, parse, Attribute, Bindings, Kind, Source, MAX_BYTES};
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

pub(super) fn register(machine: &mut cosmos::Machine) {
    machine.register::<Xml>();
    machine.register::<Document>();
    machine.register::<Node>();
}
struct Xml;
#[derive(Clone)]
struct Document(Arc<Source>);
#[derive(Clone)]
struct Node {
    source: Arc<Source>,
    index: usize,
}
#[derive(cosmos::FromLua)]
struct AttributeInput {
    name: LuaStr,
    value: LuaStr,
}
impl InputSize for AttributeInput {
    fn input_size(&self) -> usize {
        self.name.len().saturating_add(self.value.len())
    }
}
#[derive(cosmos::FromLua)]
enum Child {
    #[lua(type = "string")]
    Text(LuaStr),
    #[lua(type = "userdata")]
    Node(Node),
}
impl InputSize for Child {
    fn input_size(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Node(node) => node.data().range.len(),
        }
    }
}
enum PreparedChild {
    Text(String),
    Node(Node),
}
#[derive(cosmos::PushLua)]
struct Namespace {
    prefix: String,
    uri: String,
}
static QUEUE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static ACTIVE: OnceLock<Arc<Semaphore>> = OnceLock::new();
async fn work<R: Send + 'static, J: FnOnce() -> Result<R, String> + Send + 'static>(
    prepare: impl FnOnce() -> J,
) -> Result<R, String> {
    let queued = QUEUE
        .get_or_init(|| Arc::new(Semaphore::new(16)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "XML worker queue is full")?;
    let active = ACTIVE
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "XML workers are closed")?;
    let job = prepare();
    let task = tokio::task::spawn_blocking(move || {
        let (_queued, _active) = (queued, active);
        job()
    });
    struct Abort(tokio::task::AbortHandle);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _abort = Abort(task.abort_handle());
    task.await
        .map_err(|err| format!("XML worker failed: {err}"))?
}
fn qname(name: &str) -> Result<(), String> {
    if name.len() > 1024 {
        return Err("XML name exceeds 1024 bytes".into());
    }
    let text = format!("<{name}/>");
    let mut tokens = xmlparser::Tokenizer::from(text.as_str());
    match tokens.next() {
        Some(Ok(xmlparser::Token::ElementStart { prefix, local, .. }))
            if if prefix.is_empty() {
                local.as_str() == name
            } else {
                format!("{prefix}:{local}") == name
            } => {}
        _ => return Err("invalid XML qualified name".into()),
    }
    if !matches!(
        tokens.next(),
        Some(Ok(xmlparser::Token::ElementEnd {
            end: xmlparser::ElementEnd::Empty,
            ..
        }))
    ) || tokens.next().is_some()
    {
        return Err("invalid XML qualified name".into());
    }
    Ok(())
}
fn root(source: Arc<Source>) -> Node {
    let index = source.nodes[0]
        .children
        .iter()
        .copied()
        .find(|&i| source.nodes[i].kind == Kind::Element)
        .expect("validated XML has a document element");
    Node { source, index }
}
async fn leaf(value: LuaStr, kind: Kind, target: Option<LuaStr>) -> Result<Node, String> {
    if value.len() > MAX_BYTES || target.as_ref().is_some_and(|v| v.len() > 1024) {
        return Err("XML node input exceeds size limit".into());
    }
    work(move || {
        let value = value.to_string();
        let target = target.map(|v| v.to_string());
        move || {
            if value.len() > MAX_BYTES {
                return Err("XML node content exceeds 16 MiB".into());
            }
            let raw = match kind {
                Kind::Text => escape(&value, false)?,
                Kind::Cdata => {
                    if value.contains("]]>") {
                        return Err("CDATA content must not contain ]]>".into());
                    }
                    format!("<![CDATA[{value}]]>")
                }
                Kind::Comment => format!("<!--{value}-->"),
                Kind::Pi => {
                    let target = target.ok_or("missing XML PI target")?;
                    qname(&target)?;
                    format!("<?{target} {value}?>")
                }
                _ => unreachable!("XML leaf constructor requires a leaf kind"),
            };
            let mut text = String::from("<fragment>");
            append(&mut text, &raw)?;
            append(&mut text, "</fragment>")?;
            let mut source = parse(text, true)?;
            Arc::get_mut(&mut source)
                .expect("new XML source is unique")
                .hidden_root = true;
            // 空文本没有词法 token，但仍是可构造的合法空节点。
            if raw.is_empty() && kind == Kind::Text {
                let source = Arc::get_mut(&mut source).expect("new XML source is unique");
                let parent = 1;
                let index = source.nodes.len();
                source.nodes.push(model::Record {
                    kind,
                    range: 10..10,
                    open_end: 0,
                    parent: Some(parent),
                    children: Vec::new(),
                    name: String::new(),
                    value: String::new(),
                    namespace: None,
                    attrs: Vec::new(),
                    bindings: Bindings::new(),
                });
                source.nodes[parent].children.push(index);
            }
            let parent = root(source.clone());
            let children = &source.nodes[parent.index].children;
            if children.len() != 1 || source.nodes[children[0]].kind != kind {
                return Err("invalid XML node content".into());
            }
            Ok(Node {
                index: children[0],
                source,
            })
        }
    })
    .await
}
#[cosmos::userdata(name = "encoding.xml")]
impl Xml {
    async fn parse(data: LuaBytes) -> Result<Document, String> {
        if data.len() > MAX_BYTES {
            return Err("XML input exceeds 16 MiB".into());
        }
        work(move || {
            let bytes = data.to_vec();
            move || {
                let text = String::from_utf8(bytes).map_err(|_| "XML input must be valid UTF-8")?;
                parse(text, false).map(Document)
            }
        })
        .await
    }
    async fn element(
        name: LuaStr,
        attrs: Option<InputList<AttributeInput>>,
        children: Option<InputList<Child>>,
    ) -> Result<Node, String> {
        let attrs = attrs.map(|v| v.0).transpose()?.unwrap_or_default();
        let children = children.map(|v| v.0).transpose()?.unwrap_or_default();
        if name.len() > 1024 {
            return Err("XML name exceeds 1024 bytes".into());
        }
        work(move || {
            let name = name.to_string();
            let attrs: Vec<_> = attrs
                .into_iter()
                .map(|a| (a.name.to_string(), a.value.to_string()))
                .collect();
            let children: Vec<_> = children
                .into_iter()
                .map(|c| match c {
                    Child::Text(v) => PreparedChild::Text(v.to_string()),
                    Child::Node(v) => PreparedChild::Node(v),
                })
                .collect();
            move || {
                qname(&name)?;
                let mut text = String::from("<");
                append(&mut text, &name)?;
                let mut bindings = Bindings::new();
                for (attr_name, attr_value) in attrs {
                    qname(&attr_name)?;
                    if attr_name == "xmlns" {
                        bindings.insert(String::new(), Arc::from(attr_value.as_str()));
                    } else if let Some(prefix) = attr_name.strip_prefix("xmlns:") {
                        bindings.insert(prefix.into(), Arc::from(attr_value.as_str()));
                    }
                    append(&mut text, " ")?;
                    append(&mut text, &attr_name)?;
                    append(&mut text, "=\"")?;
                    append(&mut text, &escape(&attr_value, true)?)?;
                    append(&mut text, "\"")?;
                }
                append(&mut text, ">")?;
                for child in children {
                    let raw = match child {
                        PreparedChild::Text(text) => escape(&text, false)?,
                        PreparedChild::Node(node) => node.source.fragment(node.index, &bindings)?,
                    };
                    append(&mut text, &raw)?;
                }
                append(&mut text, "</")?;
                append(&mut text, &name)?;
                append(&mut text, ">")?;
                Ok(root(parse(text, true)?))
            }
        })
        .await
    }
    async fn text(text: LuaStr) -> Result<Node, String> {
        leaf(text, Kind::Text, None).await
    }
    async fn cdata(text: LuaStr) -> Result<Node, String> {
        leaf(text, Kind::Cdata, None).await
    }
    async fn comment(text: LuaStr) -> Result<Node, String> {
        leaf(text, Kind::Comment, None).await
    }
    async fn pi(target: LuaStr, text: LuaStr) -> Result<Node, String> {
        leaf(text, Kind::Pi, Some(target)).await
    }
}
impl Document {
    fn check(&self, node: &Node) -> Result<(), String> {
        if !Arc::ptr_eq(&self.0, &node.source) {
            return Err("XML node belongs to a different document version".into());
        }
        Ok(())
    }
    async fn edit(
        self,
        operation: impl FnOnce(&Source) -> Result<(Range<usize>, String), String> + Send + 'static,
    ) -> Result<Document, String> {
        work(move || {
            move || {
                let (range, replacement) = operation(&self.0)?;
                if self.0.text.len() - range.len() + replacement.len() > MAX_BYTES {
                    return Err("XML output exceeds 16 MiB".into());
                }
                let mut text = String::new();
                append(&mut text, &self.0.text[..range.start])?;
                append(&mut text, &replacement)?;
                append(&mut text, &self.0.text[range.end..])?;
                parse(text, false).map(Document)
            }
        })
        .await
    }
}
#[cosmos::userdata(name = "encoding.xml.document")]
impl Document {
    async fn new(node: Node) -> Result<Document, String> {
        if node.data().kind != Kind::Element {
            return Err("XML document requires an element root".into());
        }
        work(move || {
            move || parse(node.source.fragment(node.index, &Bindings::new())?, false).map(Document)
        })
        .await
    }
    fn root(&self) -> Node {
        root(self.0.clone())
    }
    fn children(&self) -> List<Node> {
        List(
            self.0.nodes[0]
                .children
                .iter()
                .map(|&index| Node {
                    source: self.0.clone(),
                    index,
                })
                .collect(),
        )
    }
    fn encode(&self) -> String {
        self.0.text.clone()
    }
    async fn replace(self, node: Node, replacement: Option<Node>) -> Result<Document, String> {
        self.check(&node)?;
        self.edit(move |source| {
            let parent = node
                .data()
                .parent
                .ok_or("cannot replace an XML document node")?;
            let raw = replacement
                .map(|n| n.source.fragment(n.index, &source.bindings(parent)))
                .transpose()?
                .unwrap_or_default();
            Ok((node.data().range.clone(), raw))
        })
        .await
    }
    async fn insert(
        self,
        parent: Node,
        child: Node,
        index: Option<f64>,
    ) -> Result<Document, String> {
        self.check(&parent)?;
        self.edit(move |source| {
            let data = parent.data();
            if data.kind != Kind::Element {
                return Err("XML insert parent must be an element".into());
            }
            let maximum = data.children.len() + 1;
            let index = index.unwrap_or(maximum as f64);
            if !index.is_finite()
                || index.fract() != 0.0
                || !(1.0..=maximum as f64).contains(&index)
            {
                return Err("XML insertion index is out of range".into());
            }
            let raw = child
                .source
                .fragment(child.index, &source.bindings(parent.index))?;
            if source.text[..data.open_end].ends_with("/>") {
                Ok((
                    data.open_end - 2..data.open_end,
                    format!(">{raw}</{}>", data.name),
                ))
            } else {
                let position = if index as usize <= data.children.len() {
                    source.nodes[data.children[index as usize - 1]].range.start
                } else {
                    data.range.start
                        + source.text[data.range.clone()]
                            .rfind("</")
                            .ok_or("missing XML closing tag")?
                };
                Ok((position..position, raw))
            }
        })
        .await
    }
}
impl Node {
    fn data(&self) -> &model::Record {
        &self.source.nodes[self.index]
    }
}
#[cosmos::userdata(name = "encoding.xml.node")]
impl Node {
    fn kind(&self) -> &str {
        self.data().kind.name()
    }
    fn name(&self) -> Option<String> {
        matches!(self.data().kind, Kind::Element | Kind::Pi).then(|| self.data().name.clone())
    }
    fn local_name(&self) -> Option<String> {
        (self.data().kind == Kind::Element).then(|| {
            self.data()
                .name
                .rsplit(':')
                .next()
                .unwrap_or_default()
                .into()
        })
    }
    fn prefix(&self) -> Option<String> {
        (self.data().kind == Kind::Element)
            .then(|| self.data().name.split_once(':').map(|(p, _)| p.into()))
            .flatten()
    }
    fn namespace(&self) -> Option<String> {
        self.data().namespace.as_ref().map(|v| v.to_string())
    }
    fn parent(&self) -> Option<Node> {
        self.data()
            .parent
            .filter(|&i| {
                self.source.nodes[i].kind != Kind::Document
                    && !(self.source.hidden_root && i == 1 && self.data().kind != Kind::Element)
            })
            .map(|index| Node {
                source: self.source.clone(),
                index,
            })
    }
    fn children(&self) -> List<Node> {
        List(
            self.data()
                .children
                .iter()
                .map(|&index| Node {
                    source: self.source.clone(),
                    index,
                })
                .collect(),
        )
    }
    fn descendants(&self) -> List<Node> {
        let mut output = Vec::new();
        let mut stack: Vec<_> = self.data().children.iter().rev().copied().collect();
        while let Some(index) = stack.pop() {
            output.push(Node {
                source: self.source.clone(),
                index,
            });
            stack.extend(self.source.nodes[index].children.iter().rev().copied());
        }
        List(output)
    }
    fn elements(&self, name: Option<String>, namespace: Option<String>) -> List<Node> {
        List(
            self.data()
                .children
                .iter()
                .copied()
                .filter(|&i| {
                    let n = &self.source.nodes[i];
                    n.kind == Kind::Element
                        && name
                            .as_ref()
                            .is_none_or(|name| n.name.rsplit(':').next() == Some(name))
                        && (name.is_none() || namespace.is_some() || n.namespace.is_none())
                        && namespace
                            .as_ref()
                            .is_none_or(|ns| n.namespace.as_deref().unwrap_or("") == ns)
                })
                .map(|index| Node {
                    source: self.source.clone(),
                    index,
                })
                .collect(),
        )
    }
    fn child(&self, name: String, namespace: Option<String>) -> Option<Node> {
        self.elements(Some(name), namespace).0.into_iter().next()
    }
    fn attributes(&self) -> Result<List<Attribute>, String> {
        let mut bytes = 0;
        let mut attrs = Vec::new();
        for a in &self.data().attrs {
            bytes += a.name.len()
                + a.name.split_once(':').map_or(0, |(prefix, _)| prefix.len())
                + a.local.len()
                + a.namespace.as_ref().map_or(0, |v| v.len())
                + a.value.len();
            if bytes > MAX_BYTES {
                return Err("XML attribute output exceeds 16 MiB".into());
            }
            attrs.push(Attribute {
                name: a.name.clone(),
                local_name: a.local.clone(),
                prefix: a.name.split_once(':').map(|(p, _)| p.into()),
                namespace: a.namespace.as_ref().map(|v| v.to_string()),
                value: a.value.clone(),
            });
        }
        Ok(List(attrs))
    }
    fn attribute(&self, name: String, namespace: Option<String>) -> Option<String> {
        self.data()
            .attrs
            .iter()
            .find(|a| {
                a.local == name
                    && a.namespace.as_deref().unwrap_or("") == namespace.as_deref().unwrap_or("")
            })
            .map(|a| a.value.clone())
    }
    fn namespaces(&self) -> Result<List<Namespace>, String> {
        let mut bytes = 0;
        let mut entries = Vec::new();
        for (prefix, uri) in self.source.bindings(self.index) {
            bytes += prefix.len() + uri.len();
            if bytes > MAX_BYTES {
                return Err("XML namespace output exceeds 16 MiB".into());
            }
            entries.push(Namespace {
                prefix,
                uri: uri.to_string(),
            });
        }
        Ok(List(entries))
    }
    fn text(&self) -> String {
        if matches!(self.data().kind, Kind::Comment | Kind::Pi) {
            return self.data().value.clone();
        }
        let mut out = String::new();
        let mut stack = vec![self.index];
        while let Some(index) = stack.pop() {
            let data = &self.source.nodes[index];
            if matches!(data.kind, Kind::Text | Kind::Cdata) {
                out.push_str(&data.value);
            } else {
                stack.extend(data.children.iter().rev());
            }
        }
        out
    }
    fn raw(&self) -> String {
        self.source.text[self.data().range.clone()].into()
    }
    fn range(&self) -> Option<(f64, f64)> {
        (!self.source.synthetic).then(|| {
            (
                self.data().range.start as f64 + 1.0,
                self.data().range.end as f64,
            )
        })
    }
    async fn encode(self) -> Result<String, String> {
        work(move || move || self.source.fragment(self.index, &Bindings::new())).await
    }
}

#[cfg(test)]
mod tests {
    use super::work;
    use std::{future::Future, task::Poll, time::Duration};
    #[test]
    fn cancelled_native_jobs_keep_budget_and_queue_remains_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut releases = Vec::new();
            let mut jobs = Vec::new();
            for _ in 0..2 {
                let (ready, started) = tokio::sync::oneshot::channel();
                let (release, finish) = std::sync::mpsc::channel();
                releases.push(release);
                jobs.push(tokio::spawn(work(move || {
                    move || {
                        ready.send(()).unwrap();
                        finish.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    }
                })));
                tokio::time::timeout(Duration::from_secs(10), started)
                    .await
                    .unwrap()
                    .unwrap();
            }
            for job in jobs {
                job.abort();
                assert!(job.await.unwrap_err().is_cancelled());
            }
            let mut waiting = Vec::new();
            for _ in 0..14 {
                let mut job = Box::pin(work(|| || Ok(())));
                std::future::poll_fn(|cx| {
                    assert!(
                        job.as_mut().poll(cx).is_pending(),
                        "XML worker budget returned early"
                    );
                    Poll::Ready(())
                })
                .await;
                waiting.push(job);
            }
            assert_eq!(
                work(|| || Ok(())).await.unwrap_err(),
                "XML worker queue is full"
            );
            drop(waiting);
            for release in releases {
                release.send(()).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), work(|| || Ok(())))
                .await
                .unwrap()
                .unwrap();
        });
    }
}
