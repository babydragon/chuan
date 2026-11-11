//! 结构索引：把一份资料里"怎么组织的"写成节点树，并能按结构取集合。
//!
//! 只提供机制，不定义任何领域结构：节点类型（unit / lesson / 生字 / 条款 / 章节…）
//! 由 agent 自己决定并沉淀成 SOP。Rust 这边只保证四件事：
//! 节点形状可校验、父子关系一致、集合运算是确定的、跨资料合并有固定语义。
//!
//! 为什么不让模型自己算交集：听写清单错一个字就废，集合运算必须由代码做，
//! 模型只负责"抽出来"和"怎么用"。

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use harness_kernel::{
    Block, DocId, DocOp, DocWrite, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};

const MAX_NODES: usize = 5000;
const MAX_DEPTH: usize = 12;
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_RESULTS: usize = 500;
const MAX_LINES: usize = 2000;
const KB_SCOPE: &str = "kb";
const OUTLINE_KIND: &str = "kb.outline";

fn normalize_source(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("kb:") { raw.to_owned() } else { format!("kb:{raw}") }
}

#[derive(Clone, Debug)]
struct Node {
    id: String,
    parent: Option<String>,
    kind: String,
    label: String,
    page: Option<u32>,
    attrs: serde_json::Map<String, serde_json::Value>,
}

impl Node {
    fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or_else(|| {
            if v.is_string() {
                // 真实踩过：模型按 description 里"用 path 传路径"的提示，把路径塞进了 nodes。
                "节点必须是对象（收到字符串）。如果这是 /ws 下的 JSON 路径，请用 `path` 参数，不要放进 nodes"
                    .to_owned()
            } else {
                "节点必须是对象".to_owned()
            }
        })?;
        let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::trim);

        let id = get_str("id").filter(|s| !s.is_empty()).ok_or("节点缺少 id")?;
        if id.chars().count() > 64 {
            return Err(format!("id 太长（≤64）：{id}"));
        }
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(format!("id 只能用字母/数字/._-：{id}"));
        }
        let kind = get_str("type").filter(|s| !s.is_empty()).ok_or("节点缺少 type")?;
        if kind.chars().count() > 32 {
            return Err(format!("type 太长（≤32）：{kind}"));
        }
        let label = get_str("label").unwrap_or("");
        if label.chars().count() > 200 {
            return Err(format!("label 太长（≤200）：{id}"));
        }
        let parent = get_str("parent").map(str::to_owned);
        let page = obj.get("page").and_then(|v| v.as_u64()).map(|p| p as u32);
        let attrs = match obj.get("attrs") {
            None => serde_json::Map::new(),
            Some(serde_json::Value::Object(m)) => {
                if m.len() > 20 {
                    return Err(format!("attrs 最多 20 个键：{id}"));
                }
                for (k, v) in m {
                    if !(v.is_string() || v.is_number() || v.is_boolean()) {
                        return Err(format!("attrs.{k} 只能是字符串/数字/布尔：{id}"));
                    }
                }
                m.clone()
            }
            Some(_) => return Err(format!("attrs 必须是对象：{id}")),
        };
        Ok(Self {
            id: id.to_owned(),
            parent,
            kind: kind.to_owned(),
            label: label.to_owned(),
            page,
            attrs,
        })
    }

    fn field(&self, field: &str) -> Option<String> {
        match field {
            "" | "label" => Some(self.label.clone()),
            "type" => Some(self.kind.clone()),
            other => {
                let key = other.strip_prefix("attrs.").unwrap_or(other);
                self.attrs.get(key).map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
            }
        }
    }
}

type Index = (BTreeMap<String, Node>, BTreeMap<String, Vec<String>>);

/// 校验并建索引：id 唯一、父节点存在、无环、深度受限。
fn index_nodes(nodes: Vec<Node>) -> Result<Index, String> {
    if nodes.len() > MAX_NODES {
        return Err(format!("节点数 {} 超过上限 {MAX_NODES}", nodes.len()));
    }
    let mut by_id: BTreeMap<String, Node> = BTreeMap::new();
    for n in nodes {
        if by_id.contains_key(&n.id) {
            return Err(format!("id 重复：{}", n.id));
        }
        by_id.insert(n.id.clone(), n);
    }
    // 父必须存在 + 不能成环
    for n in by_id.values() {
        let mut cur = n.parent.as_deref();
        let mut depth = 0usize;
        let mut seen = BTreeSet::new();
        while let Some(p) = cur {
            if !seen.insert(p.to_owned()) {
                return Err(format!("父子关系成环：{}", n.id));
            }
            let Some(parent) = by_id.get(p) else {
                return Err(format!("节点 {} 的父节点不存在：{p}", n.id));
            };
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(format!("层级超过 {MAX_DEPTH}：{}", n.id));
            }
            cur = parent.parent.as_deref();
        }
    }
    // 子节点按原顺序（BTreeMap 会打乱，所以用 id 顺序重建一次稳定的树）
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in by_id.values() {
        if let Some(p) = &n.parent {
            children.entry(p.clone()).or_default().push(n.id.clone());
        }
    }
    Ok((by_id, children))
}

fn chain<'a>(nodes: &'a BTreeMap<String, Node>, id: &str) -> Vec<&'a Node> {
    let mut out = Vec::new();
    let mut cur = nodes.get(id).and_then(|n| n.parent.as_deref());
    while let Some(p) = cur {
        let Some(n) = nodes.get(p) else { break };
        out.push(n);
        cur = n.parent.as_deref();
    }
    out.reverse();
    out
}

/// `under` 是有序祖先链：在"根 → 直接父"里**按顺序**找这些条目，允许中间有别的层级。
///
/// 为什么不做"取最后 N 层"：`[{type:unit,label:第一单元}]` 才是常用的问法
/// （"第一单元下面的条目"），而 unit 不一定紧挨着条目（中间还有课、栏目）。
fn matches_under(chain: &[&Node], under: &[serde_json::Value]) -> bool {
    if under.is_empty() {
        return true;
    }
    let mut i = 0;
    for node in chain {
        if i >= under.len() {
            break;
        }
        let Some(w) = under[i].as_object() else { return false };
        let ok_type = w.get("type").and_then(|v| v.as_str()).is_none_or(|t| t == node.kind);
        let ok_label = w.get("label").and_then(|v| v.as_str()).is_none_or(|l| l == node.label);
        if ok_type && ok_label {
            i += 1;
        }
    }
    i == under.len()
}

fn matches_node(node: &Node, m: &serde_json::Value) -> bool {
    let Some(obj) = m.as_object() else { return false };
    if obj.get("type").and_then(|v| v.as_str()).is_some_and(|t| node.kind != t) {
        return false;
    }
    if obj.get("label").and_then(|v| v.as_str()).is_some_and(|l| node.label != l) {
        return false;
    }
    if obj
        .get("label_contains")
        .and_then(|v| v.as_str())
        .is_some_and(|c| !node.label.contains(c))
    {
        return false;
    }
    if let Some(attrs) = obj.get("attrs").and_then(|v| v.as_object()) {
        for (k, want) in attrs {
            let Some(have) = node.attrs.get(k) else { return false };
            let same = match (have, want) {
                (serde_json::Value::String(a), serde_json::Value::String(b)) => a == b,
                (a, b) => a == b,
            };
            if !same {
                return false;
            }
        }
    }
    true
}

async fn load_outline(ctx: &ToolCtx<'_>, sid: &str) -> Result<Option<Vec<Node>>, ToolError> {
    let Some(doc) = ctx
        .docs
        .get(&format!("{sid}:outline"))
        .await
        .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
    else {
        return Ok(None);
    };
    let raw = doc.payload.get("nodes").cloned().unwrap_or(serde_json::json!([]));
    let arr = raw.as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for v in &arr {
        out.push(
            Node::from_json(v)
                .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("索引损坏：{e}"), false))?,
        );
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------- set

pub struct KbOutlineSetTool;

#[async_trait]
impl Tool for KbOutlineSetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_outline_set".into(),
            description: "把一份资料的**结构**写进索引（整体替换这一份的结构）。\
节点是扁平列表：`{id, type, label?, parent?, page?, attrs?}`，用 parent 串成树；\
type 由你自己定（章节/单元/生字/条款…），系统不预设；**用 attrs 表达属性**（是否加粗、等级、释义…）\
而不是分裂 type。写完之后用 `kb_select` 按结构取集合。\
同一份资料重复写只会覆盖；**清空必须显式 clear=true**。\
结构大（几百个节点以上）时**不要用 nodes 内联**：让脚本生成 JSON 文件，用 path 传路径。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "来源 id，如 kb:ab12cd34" },
                    "nodes": {
                        "type": "array",
                        "items": { "type": "object" },
                        "description": "节点数组（小结构直接内联）；id 唯一（字母数字 ._-），parent 指向已有 id，最多 5000 个"
                    },
                    "path": {
                        "type": "string",
                        "description": "/ws 下的 JSON 文件（节点数组，或 {\"nodes\":[...]}）。几百个节点以上用它，与 nodes 互斥"
                    },
                    "clear": {
                        "type": "boolean",
                        "description": "清空这份资料的结构。只有 nodes/path 为空时才需要它——空结构等于删除，不显式确认一律拒绝"
                    }
                },
                "required": ["source"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Idempotent { key: "kb_outline".into() }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let sid = normalize_source(
            args.get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 source", false))?,
        );
        let inline = args.get("nodes").and_then(|v| v.as_array()).cloned();
        let file = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let arr: Vec<serde_json::Value> = match (inline, file) {
            (Some(_), Some(_)) => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "nodes 与 path 只能给一个：大结构用 path（脚本生成 JSON），小结构才内联",
                    false,
                ))
            }
            (Some(a), None) => a,
            (None, Some(p)) => read_nodes_file(&ctx.workspace.root, &p)
                .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?,
            (None, None) => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "需要 nodes 数组或 path（/ws 下的 JSON 文件）",
                    false,
                ))
            }
        };
        // 空结构 = 清空这份资料的结构。这是破坏性动作，必须显式确认：
        // "先清空再写"是很自然的思路，但一次手滑就把索引清了（真实踩过：
        // nodes=[] 回了「已写入结构：0 个节点」，看起来像成功）。
        let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
        if arr.is_empty() {
            if !clear {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    "空结构等于**清空**这份资料的结构索引。确实要清空就加 clear=true；\
                     想写入内容请给非空的 nodes 或 path（别用 [] 占位）",
                    false,
                ));
            }
        } else if clear {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "clear=true 只用于清空：既然给了节点，就去掉 clear（重写即覆盖，不需要先清空）",
                false,
            ));
        }
        let mut nodes = Vec::with_capacity(arr.len());
        for v in &arr {
            nodes.push(Node::from_json(v).map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?);
        }
        let (by_id, _) = index_nodes(nodes.clone()).map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;

        let stored: Vec<serde_json::Value> = nodes
            .iter()
            .map(|n| {
                let mut o = serde_json::Map::new();
                o.insert("id".into(), n.id.clone().into());
                o.insert("type".into(), n.kind.clone().into());
                if !n.label.is_empty() {
                    o.insert("label".into(), n.label.clone().into());
                }
                if let Some(p) = &n.parent {
                    o.insert("parent".into(), p.clone().into());
                }
                if let Some(p) = n.page {
                    o.insert("page".into(), p.into());
                }
                if !n.attrs.is_empty() {
                    o.insert("attrs".into(), serde_json::Value::Object(n.attrs.clone()));
                }
                serde_json::Value::Object(o)
            })
            .collect();

        let payload = serde_json::json!({
            "source": sid,
            "count": by_id.len(),
            "nodes": stored,
            "updated_at": harness_kernel::now_ms(),
        });
        let size = payload.to_string().len();
        if size > MAX_PAYLOAD {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("结构太大（{size} 字节），上限 {MAX_PAYLOAD}；精简 attrs 或拆成多份来源"),
                false,
            ));
        }

        let roots = stored
            .iter()
            .filter(|v| v.get("parent").is_none())
            .count();
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已写入结构：{sid}，{} 个节点（{roots} 个根），{} KB。\n\
                 用 `kb_select` 按结构取集合；`kb_outline` 看整棵树。",
                by_id.len(),
                size / 1024
            ))],
            doc_writes: vec![DocWrite {
                id: DocId(format!("{sid}:outline")),
                kind: OUTLINE_KIND.into(),
                scope: KB_SCOPE.into(),
                payload,
                version: 1,
                target: None,
                op: DocOp::Upsert,
            }],
            artifacts: Vec::new(),
        })
    }
}

/// 从 `/ws`（或相对路径）读节点数组；也接受 `{"nodes": [...]}`。
///
/// 大结构必须走这条路：让模型内联几万 token 的 JSON 是不可行的（生成端会超时/超回合）。
fn read_nodes_file(root: &str, raw: &str) -> Result<Vec<serde_json::Value>, String> {
    let rel = if raw == "/ws" { "" } else { raw.strip_prefix("/ws/").unwrap_or(raw) };
    if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
        return Err("path 必须是 /ws 下或相对工作区的路径".into());
    }
    let path = std::path::Path::new(root).join(rel);
    let bytes = std::fs::read(&path).map_err(|e| format!("读不到 {raw}：{e}"))?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(format!("{raw} 超过 8MB，结构太大"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{raw} 不是合法 JSON：{e}"))?;
    if let Some(a) = v.as_array() {
        return Ok(a.clone());
    }
    if let Some(a) = v.get("nodes").and_then(|n| n.as_array()) {
        return Ok(a.clone());
    }
    Err(format!("{raw} 必须是节点数组，或 {{\"nodes\": [...]}}"))
}

// ---------------------------------------------------------------- read

pub struct KbOutlineTool;

#[async_trait]
impl Tool for KbOutlineTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_outline".into(),
            description: "按缩进看一份资料的结构树（谁在谁下面、在第几页）。\
用来确认结构写得对不对，或在回答“有哪些单元/章节”前先看一眼。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "来源 id，如 kb:ab12cd34" },
                    "max_lines": { "type": "integer", "description": "最多显示多少行，默认 500" }
                },
                "required": ["source"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let sid = normalize_source(
            args.get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 source", false))?,
        );
        let max_lines = args
            .get("max_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
            .clamp(1, MAX_LINES as u64) as usize;
        let Some(nodes) = load_outline(ctx, &sid).await? else {
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(format!(
                    "{sid} 还没有结构索引。先自己把结构抽出来，再用 `kb_outline_set` 写进去。"
                ))],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        };
        let (by_id, children) =
            index_nodes(nodes).map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        let mut lines = Vec::new();
        // 根节点：没有 parent 的那些；BTreeMap 保证同一层按 id 稳定排序
        let mut stack: Vec<(String, usize)> = by_id
            .values()
            .filter(|n| n.parent.is_none())
            .rev()
            .map(|n| (n.id.clone(), 0))
            .collect();
        while let Some((id, depth)) = stack.pop() {
            if lines.len() >= max_lines {
                lines.push("……（还有更多，用 `kb_select` 精确取）".to_string());
                break;
            }
            let Some(n) = by_id.get(&id) else { continue };
            let page = n.page.map(|p| format!(" (p{p})")).unwrap_or_default();
            let label = if n.label.is_empty() { n.id.clone() } else { n.label.clone() };
            lines.push(format!("{}- [{}] {label}{page}", "  ".repeat(depth), n.kind));
            if let Some(kids) = children.get(&id) {
                for k in kids.iter().rev() {
                    stack.push((k.clone(), depth + 1));
                }
            }
        }

        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "{sid} 的结构（{} 个节点）：\n{}",
                by_id.len(),
                lines.join("\n")
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------- select

pub struct KbSelectTool;

#[async_trait]
impl Tool for KbSelectTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_select".into(),
            description: "按**结构**从一份或多份资料里取集合（不是全文检索）。\
例如“某单元某栏目的条目”“所有背诵篇目”“课本生字和老师词表的合集”。\
`match.under` 是有序祖先条件：在祖先链里按顺序找，允许中间还有别的层级\
（例如 [{type:unit,label:第一单元}] 就能取出该单元下所有条目）。\
`merge` 决定多份来源怎么合并：union（并集，默认）/ intersect（交集）。\
集合运算由代码做，你只负责选条件——不要自己心算再去重。\
结果里同一份来源的重复值只保留一次。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "sources": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "来源 id 列表，如 [\"kb:ab12\", \"kb:cd34\"]"
                    },
                    "match": {
                        "type": "object",
                        "description": "匹配条件：type / label / label_contains / attrs / under",
                        "properties": {
                            "type": { "type": "string" },
                            "label": { "type": "string" },
                            "label_contains": { "type": "string" },
                            "attrs": { "type": "object" },
                            "under": {
                                "type": "array",
                                "items": { "type": "object" },
                                "description": "有序祖先链，从根到直接父；每项可含 type / label"
                            }
                        }
                    },
                    "field": { "type": "string", "description": "取哪个字段：label（默认）/ type / attrs.<键>" },
                    "merge": { "type": "string", "enum": ["union", "intersect"], "description": "默认 union" }
                },
                "required": ["sources", "match"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let sources: Vec<String> = args
            .get("sources")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str())
                    .map(normalize_source)
                    .collect()
            })
            .unwrap_or_default();
        if sources.is_empty() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "需要 sources（至少一个来源 id）",
                false,
            ));
        }
        let m = args
            .get("match")
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 match", false))?;
        let field = args.get("field").and_then(|v| v.as_str()).unwrap_or("label");
        let merge = args.get("merge").and_then(|v| v.as_str()).unwrap_or("union");
        let under: Vec<serde_json::Value> = m
            .get("under")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // 每份来源算出自己的有序集合（去重）；再看 merge
        let mut per_source: Vec<(String, Vec<(String, Option<u32>)>)> = Vec::new();
        for sid in &sources {
            let Some(nodes) = load_outline(ctx, sid).await? else {
                per_source.push((sid.clone(), Vec::new()));
                continue;
            };
            let (by_id, _) =
                index_nodes(nodes).map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
            let mut seen = BTreeSet::new();
            let mut hits = Vec::new();
            for n in by_id.values() {
                if !matches_node(n, &m) {
                    continue;
                }
                let ch = chain(&by_id, &n.id);
                if !matches_under(&ch, &under) {
                    continue;
                }
                let Some(v) = n.field(field).filter(|s| !s.trim().is_empty()) else {
                    continue;
                };
                let v = v.trim().to_owned();
                if seen.insert(v.clone()) {
                    hits.push((v, n.page));
                }
                if hits.len() >= MAX_RESULTS {
                    break;
                }
            }
            per_source.push((sid.clone(), hits));
        }

        let merged: Vec<(String, Option<u32>)> = if merge == "intersect" {
            let mut it = per_source.iter();
            let mut out = match it.next() {
                Some((_, first)) => first.clone(),
                None => Vec::new(),
            };
            for (_, other) in it {
                let keep: BTreeSet<&str> = other.iter().map(|(v, _)| v.as_str()).collect();
                out.retain(|(v, _)| keep.contains(v.as_str()));
            }
            out
        } else {
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for (_, hits) in &per_source {
                for (v, p) in hits {
                    if seen.insert(v.clone()) {
                        out.push((v.clone(), *p));
                    }
                }
            }
            out
        };

        if merged.is_empty() {
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(format!(
                    "没有取到条目（{}；条件：{}）。\n\
                     可能原因：① 这些资料还没写结构索引（先 `kb_outline` 看一眼）；\
                     ② type/label 和你写结构时用的不一致；③ 交集为空。",
                    sources.join("、"),
                    m
                ))],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }

        let mut lines: Vec<String> = merged
            .iter()
            .map(|(v, p)| match p {
                Some(p) => format!("{v}（p{p}）"),
                None => v.clone(),
            })
            .collect();
        let total = lines.len();
        if total > MAX_RESULTS {
            lines.truncate(MAX_RESULTS);
            lines.push(format!("……（共 {total} 条，已截断）"));
        }
        let detail = per_source
            .iter()
            .map(|(sid, hits)| format!("{sid}: {}", hits.len()))
            .collect::<Vec<_>>()
            .join("；");
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "共 {total} 条（{}）：\n{}\n\n各来源条目数：{detail}",
                if merge == "intersect" { "交集" } else { "并集，已去重" },
                lines.join("\n")
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_kernel::{CallId, ConversationId, Doc, RunId, Workspace};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct MapDocs {
        docs: Mutex<HashMap<String, Doc>>,
    }

    #[async_trait]
    impl harness_kernel::DocReader for MapDocs {
        async fn list(&self, _kind: &str, _limit: usize) -> Result<Vec<Doc>, String> {
            Ok(Vec::new())
        }
        async fn get(&self, id: &str) -> Result<Option<Doc>, String> {
            Ok(self.docs.lock().unwrap().get(id).cloned())
        }
        async fn delete(&self, _id: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    fn ctx<'a>(ws: &'a Workspace, docs: &'a MapDocs, env: &'a crate::NullEnv) -> ToolCtx<'a> {
        ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("c".into()),
            actor: "owner",
            outbox: &harness_kernel::NullOutbox,
            docs,
            run: RunId("r".into()),
            call: CallId("call".into()),
            workspace: ws,
            env,
            cancel: CancellationToken::new(),
        }
    }

    /// 写入后把 doc_writes 落到 MapDocs，模拟内核提交。
    async fn set(docs: &MapDocs, c: &ToolCtx<'_>, source: &str, nodes: serde_json::Value) {
        let out = KbOutlineSetTool
            .execute(serde_json::json!({"source": source, "nodes": nodes}), c)
            .await
            .unwrap();
        for w in out.doc_writes {
            docs.docs.lock().unwrap().insert(
                w.id.0.clone(),
                Doc {
                    id: w.id,
                    kind: w.kind,
                    scope: w.scope,
                    payload: w.payload,
                    version: 1,
                    valid_from: 0,
                    valid_to: None,
                },
            );
        }
    }

    #[tokio::test]
    async fn select_by_structure_union_and_intersect() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace { run: RunId("r".into()), root: dir.path().to_string_lossy().into() };
        let docs = MapDocs::default();
        let env = crate::NullEnv;
        let c = ctx(&ws, &docs, &env);

        set(
            &docs,
            &c,
            "kb:book",
            serde_json::json!([
                {"id": "u1", "type": "unit", "label": "第一单元"},
                {"id": "u1.l1", "type": "lesson", "label": "第1课", "parent": "u1", "page": 4},
                {"id": "u1.l1.z", "type": "section", "label": "生字", "parent": "u1.l1", "page": 5},
                {"id": "u1.l1.z.1", "type": "item", "label": "鸦", "parent": "u1.l1.z", "page": 5},
                {"id": "u1.l1.z.2", "type": "item", "label": "喝", "parent": "u1.l1.z", "page": 5},
                {"id": "u2", "type": "unit", "label": "第二单元"},
                {"id": "u2.l3", "type": "lesson", "label": "第3课", "parent": "u2"},
                {"id": "u2.l3.z", "type": "section", "label": "生字", "parent": "u2.l3"},
                {"id": "u2.l3.z.1", "type": "item", "label": "水", "parent": "u2.l3.z"}
            ]),
        )
        .await;
        set(
            &docs,
            &c,
            "kb:handout",
            serde_json::json!([
                {"id": "u1", "type": "unit", "label": "第一单元"},
                {"id": "u1.t", "type": "section", "label": "听写", "parent": "u1"},
                {"id": "u1.t.1", "type": "item", "label": "鸦", "parent": "u1.t"},
                {"id": "u1.t.2", "type": "item", "label": "喝", "parent": "u1.t"},
                {"id": "u1.t.3", "type": "item", "label": "鸟", "parent": "u1.t"}
            ]),
        )
        .await;

        // 第一单元的条目（按 unit label 限定）
        let out = KbSelectTool
            .execute(
                serde_json::json!({
                    "sources": ["kb:book"],
                    "match": {"type": "item", "under": [{"type": "unit", "label": "第一单元"}]}
                }),
                &c,
            )
            .await
            .unwrap();
        let text = para(&out);
        assert!(text.contains("鸦（p5）"), "{text}");
        assert!(text.contains("共 2 条"), "{text}");

        // 跨资料并集：课本生字 + 老师听写
        let out = KbSelectTool
            .execute(
                serde_json::json!({
                    "sources": ["kb:book", "kb:handout"],
                    "merge": "union",
                    "match": {"type": "item", "under": [{"type": "unit", "label": "第一单元"}]}
                }),
                &c,
            )
            .await
            .unwrap();
        let text = para(&out);
        assert!(text.contains("共 3 条"), "并集去重后三个词：{text}");
        assert!(text.contains("鸟"), "{text}");

        // 交集
        let out = KbSelectTool
            .execute(
                serde_json::json!({
                    "sources": ["kb:book", "kb:handout"],
                    "merge": "intersect",
                    "match": {"type": "item", "under": [{"type": "unit", "label": "第一单元"}]}
                }),
                &c,
            )
            .await
            .unwrap();
        let text = para(&out);
        assert!(text.contains("共 2 条"), "{text}");
        assert!(!text.contains("鸟"), "{text}");
    }

    #[tokio::test]
    async fn outline_rejects_bad_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace { run: RunId("r".into()), root: dir.path().to_string_lossy().into() };
        let docs = MapDocs::default();
        let env = crate::NullEnv;
        let c = ctx(&ws, &docs, &env);

        for bad in [
            serde_json::json!([{"id": "a", "type": "x", "parent": "nope"}]),
            serde_json::json!([{"id": "a", "type": "x"}, {"id": "a", "type": "y"}]),
            serde_json::json!([{"id": "a/1", "type": "x"}]),
        ] {
            let err = KbOutlineSetTool
                .execute(serde_json::json!({"source": "kb:book", "nodes": bad}), &c)
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{}", err.message);
        }
    }

    fn para(out: &ToolOutput) -> String {
        match &out.content[0] {
            Block::Paragraph(t) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn outline_can_be_loaded_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(
            ws_root.join("outline.json"),
            serde_json::json!({ "nodes": [
                {"id": "u1", "type": "unit", "label": "第一单元"},
                {"id": "u1.l1", "type": "课", "label": "第1课", "parent": "u1"}
            ]})
            .to_string(),
        )
        .unwrap();

        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into() };
        let docs = MapDocs::default();
        let env = crate::NullEnv;
        let c = ctx(&ws, &docs, &env);

        let out = KbOutlineSetTool
            .execute(
                serde_json::json!({"source": "kb:book", "path": "/ws/outline.json"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(out.doc_writes.len(), 1);
        assert_eq!(out.doc_writes[0].payload["count"], 2);

        // 二选一：两个都给要拒绝
        let err = KbOutlineSetTool
            .execute(
                serde_json::json!({
                    "source": "kb:book",
                    "path": "/ws/outline.json",
                    "nodes": [{"id": "a", "type": "x"}]
                }),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("只能给一个"), "{}", err.message);
    }

    /// 参数 schema 必须声明 `path`、且不能把 `nodes` 列为必填。
    ///
    /// 真实事故：description 写着"用 path 传路径"，schema 里却只有 required 的 `nodes`，
    /// 于是模型只能把路径塞进数组，被 `Node::from_json` 以"节点必须是对象"拒掉——
    /// 113 个节点的词表结构因此写不进库，模型的结论是"工具不支持"。
    #[test]
    fn schema_declares_path_and_does_not_require_nodes() {
        let spec = KbOutlineSetTool.spec();
        let props = &spec.parameters["properties"];
        assert!(
            props.get("path").is_some(),
            "schema 必须声明 path，否则模型看不见这个入参：{}",
            spec.parameters
        );
        let required: Vec<&str> = spec.parameters["required"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(required, vec!["source"], "只有 source 必填（nodes/path 二选一）");
    }

    /// 把路径塞进 nodes 时，报错要指向 `path`，别只说"必须是对象"。
    #[tokio::test]
    async fn string_in_nodes_points_at_path_param() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace { run: RunId("r".into()), root: dir.path().to_string_lossy().into() };
        let docs = MapDocs::default();
        let env = crate::NullEnv;
        let c = ctx(&ws, &docs, &env);

        let err = KbOutlineSetTool
            .execute(
                serde_json::json!({"source": "kb:book", "nodes": ["/ws/outline.json"]}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("path"), "{}", err.message);
    }

    /// 空结构是**清空**，必须显式确认；给了节点又写 clear 也要拒绝（别让语义含混）。
    #[tokio::test]
    async fn empty_outline_requires_explicit_clear() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace { run: RunId("r".into()), root: dir.path().to_string_lossy().into() };
        let docs = MapDocs::default();
        let env = crate::NullEnv;
        let c = ctx(&ws, &docs, &env);

        // 不带 clear 的 [] → 拒绝
        let err = KbOutlineSetTool
            .execute(serde_json::json!({"source":"kb:book","nodes":[]}), &c)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{}", err.message);
        assert!(err.message.contains("clear=true"), "{}", err.message);

        // 带 clear 的 [] → 真的清空
        let out = KbOutlineSetTool
            .execute(serde_json::json!({"source":"kb:book","nodes":[],"clear":true}), &c)
            .await
            .unwrap();
        assert_eq!(out.doc_writes[0].payload["count"], 0);

        // 有节点 + clear → 语义含混，拒绝
        let err = KbOutlineSetTool
            .execute(
                serde_json::json!({"source":"kb:book","nodes":[{"id":"a","type":"x"}],"clear":true}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("去掉 clear"), "{}", err.message);
    }
}
