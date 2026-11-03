//! 命令条目表（`DECISIONS.md` D31）：把"免审批"变成数据。
//!
//! 一条条目 = 名字（文件名）+ 执行环境 + argv 模板 + 一组**封闭**参数域。
//! 三条硬规则：
//! 1. **不经 shell**：argv 直接交给 `ExecutionEnv`，模板变量只能是已声明的参数；
//! 2. **没有自由字符串**：参数只接受枚举 / 受根约束的文件 / 区间整数 / 封闭字符类；
//! 3. **单条失效**：一个文件写坏只废那一条，其余照常（`capability list` 会点名）。
//!
//! 目录由内核在主人批准后写入（agent 没有写路径），按 mtime 热加载。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 条目允许的最长执行时间（模型不能覆盖）。
pub const MAX_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;

/// v1 允许的虚拟根。宿主侧按它拼真实路径做校验，但**传给沙盒的仍是虚拟路径**。
///
/// 为什么没有 `/media`：`lan` lane 不挂 `/media`（也挂不了 `/inbox`、`/skills`）——
/// 那条 lane 同时跑 `lan_exec`，为打印放开整块相册不值。要打照片就让沙盒先 `cp` 到 `/ws`。
pub const FILE_ROOTS: [&str; 1] = ["/ws"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Env {
    Lan,
}

impl Env {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "lan" => Ok(Env::Lan),
            // host / install 是 D31 里明确排在后面的：解析上不装懂，直接说没做。
            "host" => Err("env = \"host\" 尚未实现（D31 §11，触发式）".into()),
            "install" => Err("env = \"install\" 永远不进条目表（包名是开放域，D31 §11）".into()),
            other => Err(format!("env 只能是 lan（收到 {other}）")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    Owner,
    Anyone,
}

impl Who {
    fn parse(s: Option<&str>) -> Result<Self, String> {
        match s {
            None | Some("owner") => Ok(Who::Owner),
            Some("anyone") => Ok(Who::Anyone),
            Some(other) => Err(format!("who 只能是 owner / anyone（收到 {other}）")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// 封闭取值
    Enum(Vec<String>),
    /// 必须是 `/ws` 下的真实文件（执行时 canonicalize，拒绝符号链接逃逸）
    File,
    /// 区间整数
    Int { min: i64, max: i64 },
    /// 封闭字符类（`chars` 支持 `a-z` 这类区间）+ 长度
    Chars { chars: String, min: usize, max: usize },
}

#[derive(Clone, Debug)]
pub struct CommandEntry {
    pub name: String,
    pub env: Env,
    pub argv: Vec<String>,
    pub timeout_ms: u64,
    pub who: Who,
    pub params: BTreeMap<String, ParamKind>,
}

impl CommandEntry {
    /// 给模型看的签名：`printer_status()` / `print_file(media: A4|B5, file: /ws 下的路径)`。
    ///
    /// 只列参数域，不列 argv：模型需要知道"能传什么"，不需要知道"具体跑哪条命令"。
    pub fn signature(&self) -> String {
        if self.params.is_empty() {
            return format!("{}()", self.name);
        }
        let mut parts = Vec::new();
        for (k, kind) in &self.params {
            let v = match kind {
                ParamKind::Enum(items) => items.join("|"),
                ParamKind::File => "/ws 下的路径".to_string(),
                ParamKind::Int { min, max } => format!("{min}..{max}"),
                ParamKind::Chars { min, max, .. } => format!("文本(≤{max} 字)"),
            };
            parts.push(format!("{k}: {v}"));
        }
        format!("{}({})", self.name, parts.join(", "))
    }
}

/// 表 + 坏文件清单。`errors` 只影响对应条目。
#[derive(Clone, Debug, Default)]
pub struct CommandTable {
    pub entries: BTreeMap<String, CommandEntry>,
    pub errors: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct ResolvedCommand {
    pub name: String,
    pub env: Env,
    pub argv: Vec<String>,
    pub timeout_ms: u64,
    /// 解析后 argv 的指纹：审计与"批准的是不是这一条"靠它
    pub hash: String,
}

impl CommandTable {
    /// 读整个目录。单个文件解析失败只记进 `errors`，不影响其它条目。
    pub fn load_dir(dir: &Path) -> Self {
        let mut table = CommandTable::default();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return table; // 目录不存在 = 空表（一切拒绝，但不是"坏"）
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            match Self::load_file(&path, name) {
                Ok(entry) => {
                    table.entries.insert(entry.name.clone(), entry);
                }
                Err(reason) => table.errors.push((name.to_owned(), reason)),
            }
        }
        table
    }

    fn load_file(path: &Path, name: &str) -> Result<CommandEntry, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::parse_entry(name, &text)
    }

    /// 从文本解析一条条目（提案时先校验，批准后原样落盘）。
    pub fn parse_entry(name: &str, text: &str) -> Result<CommandEntry, String> {
        if !valid_name(name) {
            return Err("文件名只能是小写字母/数字/-/_，且不超过 32 字符".into());
        }
        let f: EntryFile = toml::from_str(text).map_err(|e| e.to_string())?;

        let env = Env::parse(&f.env)?;
        let who = Who::parse(f.who.as_deref())?;
        if f.argv.is_empty() {
            return Err("argv 不能为空".into());
        }
        if f.argv.len() > 16 {
            return Err("argv 过长（≤16 段）".into());
        }
        let first = &f.argv[0];
        if !first.starts_with('/') {
            return Err(format!("argv[0] 必须是绝对路径（收到 {first}）"));
        }
        if f.argv.iter().any(|a| a.is_empty()) {
            return Err("argv 里不允许空串".into());
        }

        let mut params: BTreeMap<String, ParamKind> = BTreeMap::new();
        for (k, p) in &f.params {
            if !valid_name(k) {
                return Err(format!("参数名 {k} 只能用 [a-z0-9_-]"));
            }
            params.insert(k.clone(), p.to_kind()?);
        }

        // 模板与参数必须一一对应：多一个未声明的 `{x}` 就是自由变量，
        // 少用一个声明的参数则说明作者写错了（都会让"参数域"名不副实）。
        let mut used: Vec<String> = Vec::new();
        for tok in &f.argv {
            for name in placeholders(tok) {
                if !params.contains_key(&name) {
                    return Err(format!("argv 里的 {{{name}}} 没有在 [params] 声明"));
                }
                used.push(name);
            }
        }
        for k in params.keys() {
            if !used.iter().any(|u| u == k) {
                return Err(format!("参数 {k} 声明了但 argv 里没用"));
            }
        }

        let timeout_ms = if f.timeout_ms == 0 { DEFAULT_TIMEOUT_MS } else { f.timeout_ms };
        if timeout_ms > MAX_TIMEOUT_MS {
            return Err(format!("timeout_ms 上限 {MAX_TIMEOUT_MS}"));
        }

        Ok(CommandEntry { name: name.to_owned(), env, argv: f.argv, timeout_ms, who, params })
    }

    pub fn get(&self, name: &str) -> Option<&CommandEntry> {
        self.entries.get(name)
    }

    /// 解析一次调用：校验参数、拼出 argv、算指纹。
    ///
    /// `workspace` 是宿主上的工作区根（用于 `file` 参数的 canonicalize 校验）；
    /// **返回的 argv 仍是沙盒路径**（`/ws/...`），因为 lan lane 把 workspace 挂在 `/ws`。
    pub fn resolve(
        &self,
        name: &str,
        args: &Value,
        workspace: &Path,
    ) -> Result<ResolvedCommand, String> {
        let entry = self.get(name).ok_or_else(|| {
            format!(
                "没有这个命令条目：{name}。不要改用 `lan_exec` 重试——需要新动作请用 \
                 capability(action=\"propose_command\") 提交提案"
            )
        })?;
        let obj = match args {
            Value::Null => serde_json::Map::new(),
            Value::Object(m) => m.clone(),
            _ => return Err("args 必须是对象".into()),
        };
        for k in obj.keys() {
            if !entry.params.contains_key(k) {
                return Err(format!("条目 {name} 不认识参数 {k}"));
            }
        }
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        for (k, kind) in &entry.params {
            let v = obj.get(k).ok_or_else(|| format!("缺少参数 {k}"))?;
            values.insert(k.clone(), check_value(k, kind, v, workspace)?);
        }

        let mut argv = Vec::with_capacity(entry.argv.len());
        for tok in &entry.argv {
            let mut out = tok.clone();
            for (k, v) in &values {
                out = out.replace(&format!("{{{k}}}"), v);
            }
            if out.is_empty() {
                return Err(format!("参数展开后出现空参数：{tok}"));
            }
            argv.push(out);
        }
        Ok(ResolvedCommand {
            name: entry.name.clone(),
            env: entry.env,
            hash: fingerprint(&argv),
            argv,
            timeout_ms: entry.timeout_ms,
        })
    }
}

fn check_value(
    name: &str,
    kind: &ParamKind,
    v: &Value,
    workspace: &Path,
) -> Result<String, String> {
    match kind {
        ParamKind::Enum(items) => {
            let s = v.as_str().ok_or_else(|| format!("参数 {name} 必须是字符串"))?;
            if items.iter().any(|i| i == s) {
                Ok(s.to_owned())
            } else {
                Err(format!("参数 {name} 只能是 [{}]", items.join(", ")))
            }
        }
        ParamKind::Int { min, max } => {
            let n = v.as_i64().ok_or_else(|| format!("参数 {name} 必须是整数"))?;
            if n < *min || n > *max {
                return Err(format!("参数 {name} 必须在 {min}..={max} 之间"));
            }
            Ok(n.to_string())
        }
        ParamKind::Chars { chars, min, max } => {
            let s = v.as_str().ok_or_else(|| format!("参数 {name} 必须是字符串"))?;
            let set = char_set(chars)?;
            let len = s.chars().count();
            if len < *min || len > *max {
                return Err(format!("参数 {name} 长度必须在 {min}..={max}"));
            }
            if !s.chars().all(|c| set.contains(&c)) {
                return Err(format!("参数 {name} 含未授权字符（允许：{chars}）"));
            }
            Ok(s.to_owned())
        }
        ParamKind::File => {
            let s = v.as_str().ok_or_else(|| format!("参数 {name} 必须是路径字符串"))?;
            check_file(name, s, workspace)
        }
    }
}

/// 文件参数：**校验在宿主侧，交给沙盒的是 `/ws/...` 逻辑路径**。
fn check_file(name: &str, virtual_path: &str, workspace: &Path) -> Result<String, String> {
    let rel = virtual_path
        .strip_prefix("/ws/")
        .ok_or_else(|| format!("参数 {name} 只能是 {} 下的路径", FILE_ROOTS.join(" / ")))?;
    if rel.is_empty() || rel.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(format!("参数 {name} 的路径不合法：{virtual_path}"));
    }
    let host = workspace.join(rel);
    let canon = host
        .canonicalize()
        .map_err(|_| format!("参数 {name} 指向的文件不存在：{virtual_path}"))?;
    let ws = workspace
        .canonicalize()
        .map_err(|_| "工作区不可用".to_owned())?;
    if !canon.starts_with(&ws) {
        return Err(format!("参数 {name} 指向工作区外（符号链接逃逸）：{virtual_path}"));
    }
    if !canon.is_file() {
        return Err(format!("参数 {name} 不是普通文件：{virtual_path}"));
    }
    Ok(virtual_path.to_owned())
}

/// `a-z` / `0-9` / 单个字符。只允许这些形态，别的一律报错——
/// "封闭字符类"的意思是**内核能数清它有哪些字符**，不是正则。
fn char_set(spec: &str) -> Result<Vec<char>, String> {
    let mut out = Vec::new();
    let cs: Vec<char> = spec.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if i + 2 < cs.len() && cs[i + 1] == '-' {
            let (a, b) = (cs[i], cs[i + 2]);
            if a > b || !a.is_ascii_graphic() || !b.is_ascii_graphic() {
                return Err(format!("chars 区间不合法：{a}-{b}"));
            }
            out.extend(a..=b);
            i += 3;
        } else {
            out.push(cs[i]);
            i += 1;
        }
    }
    if out.is_empty() {
        return Err("chars 不能为空".into());
    }
    Ok(out)
}

/// argv 模板里的 `{x}`（允许嵌在参数中间，如 `media={media}`）。
fn placeholders(tok: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = tok;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        let name = &rest[i + 1..i + j];
        if valid_name(name) {
            out.push(name.to_owned());
        }
        rest = &rest[i + j + 1..];
    }
    out
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn fingerprint(argv: &[String]) -> String {
    let mut h = Sha256::new();
    for a in argv {
        h.update(a.as_bytes());
        h.update([0u8]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_owned()
}

/// 条目表的缓存：与 `GrantsCache` 同构（按 mtime 热加载）。
///
/// 区别是**按文件失效**：一个文件写坏只废那一条，而不是整张表作废。
pub struct CommandCache {
    dir: PathBuf,
    state: Mutex<(Option<SystemTime>, CommandTable)>,
}

impl CommandCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), state: Mutex::new((None, CommandTable::default())) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn current(&self) -> CommandTable {
        let sig = signature(&self.dir);
        let mut st = self.state.lock().unwrap();
        if st.0 == sig && st.0.is_some() {
            return st.1.clone();
        }
        let table = CommandTable::load_dir(&self.dir);
        if !table.errors.is_empty() {
            for (name, reason) in &table.errors {
                tracing::warn!(entry = %name, reason, "命令条目解析失败（只废这一条）");
            }
        }
        *st = (sig, table.clone());
        table
    }
}

/// (目录及其条目的最新 mtime, 文件数)。改一个文件也会变。
fn signature(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                newest = Some(newest.map_or(t, |n| n.max(t)));
            }
        }
    }
    newest
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把提案字段渲染成规范 TOML（顺序稳定，便于审阅与 diff）。
///
/// 只做"渲染 + 自校验"：真正的语义判断在 [`CommandTable::parse_entry`] 里，
/// 渲染完立刻回读一遍，写不进去的东西不会被落盘。
pub fn render_entry(spec: &serde_json::Value) -> Result<(String, String), String> {
    let obj = spec.as_object().ok_or("command 必须是对象")?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("command.name 必填")?;
    let env = obj.get("env").and_then(|v| v.as_str()).unwrap_or("lan");
    let argv = obj
        .get("argv")
        .and_then(|v| v.as_array())
        .ok_or("command.argv 必填（字符串数组）")?;
    let argv: Vec<toml::Value> = argv
        .iter()
        .map(|v| v.as_str().map(|s| toml::Value::String(s.to_owned())).ok_or("argv 只能是字符串"))
        .collect::<Result<_, _>>()?;

    let mut root = toml::map::Map::new();
    root.insert("env".into(), toml::Value::String(env.to_owned()));
    root.insert("argv".into(), toml::Value::Array(argv));
    if let Some(t) = obj.get("timeout_ms").and_then(|v| v.as_i64()) {
        root.insert("timeout_ms".into(), toml::Value::Integer(t));
    }
    root.insert(
        "who".into(),
        toml::Value::String(obj.get("who").and_then(|v| v.as_str()).unwrap_or("owner").to_owned()),
    );
    if let Some(params) = obj.get("params") {
        let table = params.as_object().ok_or("command.params 必须是对象")?;
        let mut pt = toml::map::Map::new();
        for (k, v) in table {
            let v = toml::Value::try_from(v.clone()).map_err(|e| format!("参数 {k} 不是合法 TOML：{e}"))?;
            pt.insert(k.clone(), v);
        }
        root.insert("params".into(), toml::Value::Table(pt));
    }
    let doc = toml::Value::Table(root);
    let body = toml::to_string(&doc).map_err(|e| e.to_string())?;
    // 回读：渲染出来的必须是加载器能吃的东西
    CommandTable::parse_entry(name, &body)?;
    Ok((name.to_owned(), body))
}

/// 条目正文的指纹：批准的就是"这一份 body"，审计与去重都看它。
pub fn body_hash(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_owned()
}

/// 原子安装一条条目：**validate → tmpfile → fsync(file) → rename → fsync(dir)**。
///
/// 三条约束：
/// - **只增不改**：同名文件已存在一律拒绝。否则"批准的"和"生效的"可能不是同一份内容；
/// - 落盘前先用同一个解析器验一遍：跑不起来的东西不该进表；
/// - 写失败不留半成品（临时文件会被清掉）。
pub fn install_atomic(dir: &Path, name: &str, body: &str) -> Result<PathBuf, String> {
    let _entry = CommandTable::parse_entry(name, body)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        return Err(format!("条目 {name} 已存在（只增不改；要改先 remove）"));
    }
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let write = || -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写临时文件失败：{e}"));
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename 失败：{e}")
    })?;
    // 目录 fsync：重命名本身也要落盘，否则掉电后条目可能消失。
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(path)
}

/// 删除一条条目（同样只能改数据，不改代码）。
pub fn remove_entry(dir: &Path, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("条目名不合法：{name}"));
    }
    let path = dir.join(format!("{name}.toml"));
    std::fs::remove_file(&path).map_err(|e| format!("删除 {} 失败：{e}", path.display()))?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryFile {
    env: String,
    argv: Vec<String>,
    #[serde(default)]
    timeout_ms: u64,
    #[serde(default)]
    who: Option<String>,
    #[serde(default)]
    params: BTreeMap<String, ParamFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParamFile {
    #[serde(rename = "enum", default)]
    enum_: Option<Vec<String>>,
    #[serde(default)]
    file: Option<Vec<String>>,
    #[serde(default)]
    int: Option<IntRange>,
    #[serde(default)]
    pattern: Option<PatternSpec>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct IntRange {
    min: i64,
    max: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternSpec {
    chars: String,
    #[serde(default)]
    min: usize,
    #[serde(default = "default_max_len")]
    max: usize,
}

fn default_max_len() -> usize {
    32
}

impl ParamFile {
    fn to_kind(&self) -> Result<ParamKind, String> {
        let set = [
            self.enum_.is_some(),
            self.file.is_some(),
            self.int.is_some(),
            self.pattern.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if set != 1 {
            return Err("每个参数必须且只能声明 enum / file / int / pattern 之一".into());
        }
        if let Some(items) = &self.enum_ {
            if items.is_empty() {
                return Err("enum 不能为空".into());
            }
            return Ok(ParamKind::Enum(items.clone()));
        }
        if let Some(roots) = &self.file {
            for r in roots {
                if !FILE_ROOTS.contains(&r.as_str()) {
                    return Err(format!(
                        "file 只支持 {}（v1 没有 /media：lan lane 不挂它，先 cp 到 /ws）",
                        FILE_ROOTS.join(" / ")
                    ));
                }
            }
            return Ok(ParamKind::File);
        }
        if let Some(r) = &self.int {
            if r.min > r.max {
                return Err("int 区间反了".into());
            }
            return Ok(ParamKind::Int { min: r.min, max: r.max });
        }
        let p = self.pattern.as_ref().unwrap();
        let _ = char_set(&p.chars)?;
        if p.min > p.max {
            return Err("pattern 长度区间反了".into());
        }
        Ok(ParamKind::Chars { chars: p.chars.clone(), min: p.min, max: p.max })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(format!("{name}.toml")), body).unwrap();
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    const PRINT: &str = r#"
env = "lan"
argv = ["/usr/bin/lp", "-o", "media={media}", "{file}"]
timeout_ms = 5000
who = "anyone"
[params]
media = { enum = ["A4", "B5"] }
file = { file = ["/ws"] }
"#;

    #[test]
    fn loads_and_resolves_an_entry() {
        let dir = tmp();
        write(dir.path(), "print_file", PRINT);
        let ws = tmp();
        fs::write(ws.path().join("a.txt"), b"x").unwrap();

        let t = CommandTable::load_dir(dir.path());
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        let r = t
            .resolve(
                "print_file",
                &serde_json::json!({"media":"A4","file":"/ws/a.txt"}),
                ws.path(),
            )
            .unwrap();
        assert_eq!(r.argv, vec!["/usr/bin/lp", "-o", "media=A4", "/ws/a.txt"]);
        assert_eq!(r.argv[0], "/usr/bin/lp", "argv[0] 不动");
        assert_eq!(r.hash.len(), 16);
    }

    #[test]
    fn rejects_out_of_domain_args() {
        let dir = tmp();
        write(dir.path(), "print_file", PRINT);
        let ws = tmp();
        fs::write(ws.path().join("a.txt"), b"x").unwrap();
        let t = CommandTable::load_dir(dir.path());

        let bad_media = t.resolve(
            "print_file",
            &serde_json::json!({"media":"A3","file":"/ws/a.txt"}),
            ws.path(),
        );
        assert!(bad_media.unwrap_err().contains("media"));

        // 不在 /ws 下：v1 明确不给 /media 开口
        let bad_file = t.resolve(
            "print_file",
            &serde_json::json!({"media":"A4","file":"/media/x.jpg"}),
            ws.path(),
        );
        assert!(bad_file.unwrap_err().contains("/ws"));

        // 逃逸与不存在
        let escape = t.resolve(
            "print_file",
            &serde_json::json!({"media":"A4","file":"/ws/../etc/passwd"}),
            ws.path(),
        );
        assert!(escape.is_err());
        let missing = t.resolve(
            "print_file",
            &serde_json::json!({"media":"A4","file":"/ws/nope.txt"}),
            ws.path(),
        );
        assert!(missing.unwrap_err().contains("不存在"));

        // 多余参数
        let extra = t.resolve(
            "print_file",
            &serde_json::json!({"media":"A4","file":"/ws/a.txt","x":"1"}),
            ws.path(),
        );
        assert!(extra.unwrap_err().contains("不认识参数"));
    }

    #[test]
    fn rejects_symlink_escape() {
        let dir = tmp();
        write(dir.path(), "print_file", PRINT);
        let ws = tmp();
        let outside = tmp();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), ws.path().join("link")).unwrap();
        let t = CommandTable::load_dir(dir.path());
        let err = t
            .resolve(
                "print_file",
                &serde_json::json!({"media":"A4","file":"/ws/link"}),
                ws.path(),
            )
            .unwrap_err();
        assert!(err.contains("工作区外"), "{err}");
    }

    #[test]
    fn bad_file_only_kills_that_entry() {
        let dir = tmp();
        write(dir.path(), "print_file", PRINT);
        write(dir.path(), "broken", "env = \"lan\"\nargv = [\"lp\"]\n"); // argv[0] 非绝对
        write(dir.path(), "host_thing", "env = \"host\"\nargv = [\"/usr/bin/true\"]\n");
        write(dir.path(), "free_var", r#"
env = "lan"
argv = ["/usr/bin/lp", "{undeclared}"]
"#);

        let t = CommandTable::load_dir(dir.path());
        assert!(t.get("print_file").is_some());
        let names: Vec<&str> = t.errors.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"broken"), "{:?}", t.errors);
        assert!(names.contains(&"host_thing"), "{:?}", t.errors);
        assert!(names.contains(&"free_var"), "{:?}", t.errors);
        // host/install 的错误信息要说清"不是漏写"
        let host = t.errors.iter().find(|(n, _)| n == "host_thing").unwrap();
        assert!(host.1.contains("尚未实现"), "{}", host.1);
    }

    #[test]
    fn params_must_match_placeholders() {
        let dir = tmp();
        write(dir.path(), "unused", r#"
env = "lan"
argv = ["/usr/bin/true"]
[params]
media = { enum = ["A4"] }
"#);
        let t = CommandTable::load_dir(dir.path());
        assert!(t.get("unused").is_none());
        assert!(t.errors[0].1.contains("没用"), "{:?}", t.errors);
    }

    #[test]
    fn chars_domain_is_closed() {
        let dir = tmp();
        write(dir.path(), "set_title", r#"
env = "lan"
argv = ["/usr/bin/lpadmin", "-t", "{title}"]
[params]
title = { pattern = { chars = "A-Za-z0-9 _-", min = 1, max = 12 } }
"#);
        let ws = tmp();
        let t = CommandTable::load_dir(dir.path());
        assert!(t.errors.is_empty(), "{:?}", t.errors);

        let ok = t.resolve("set_title", &serde_json::json!({"title":"Home Printer"}), ws.path());
        assert!(ok.is_ok(), "{:?}", ok.err());
        let bad = t.resolve("set_title", &serde_json::json!({"title":"a;rm -rf /"}), ws.path());
        assert!(bad.unwrap_err().contains("未授权字符"));
    }

    #[test]
    fn int_range_is_enforced() {
        let dir = tmp();
        write(dir.path(), "copies", r#"
env = "lan"
argv = ["/usr/bin/lp", "-n", "{n}", "{file}"]
[params]
n = { int = { min = 1, max = 5 } }
file = { file = ["/ws"] }
"#);
        let ws = tmp();
        fs::write(ws.path().join("a.txt"), b"x").unwrap();
        let t = CommandTable::load_dir(dir.path());
        assert!(t.resolve("copies", &serde_json::json!({"n":3,"file":"/ws/a.txt"}), ws.path()).is_ok());
        assert!(t
            .resolve("copies", &serde_json::json!({"n":9,"file":"/ws/a.txt"}), ws.path())
            .unwrap_err()
            .contains("1..=5"));
        // 字符串形式的整数也不行：域的判定看类型，不看"能不能 parse"
        assert!(t
            .resolve("copies", &serde_json::json!({"n":"3","file":"/ws/a.txt"}), ws.path())
            .unwrap_err()
            .contains("整数"));
    }

    #[test]
    fn install_is_atomic_and_never_overwrites() {
        let dir = tmp();
        let path = install_atomic(dir.path(), "print_file", PRINT).unwrap();
        assert!(path.exists());
        assert!(CommandTable::load_dir(dir.path()).errors.is_empty());

        // 只增不改：已存在就拒（否则批准的和生效的可能不是同一份）
        let err = install_atomic(dir.path(), "print_file", PRINT).unwrap_err();
        assert!(err.contains("已存在"), "{err}");

        // 语法坏的 body 不落盘
        let bad = install_atomic(dir.path(), "broken", "env = \"nope\"\nargv = [\"x\"]\n");
        assert!(bad.is_err());
        assert!(!dir.path().join("broken.toml").exists());

        // 删除后可以重新装
        remove_entry(dir.path(), "print_file").unwrap();
        assert!(!dir.path().join("print_file.toml").exists());
        assert!(install_atomic(dir.path(), "print_file", PRINT).is_ok());
    }

    #[test]
    fn render_entry_roundtrips_and_self_checks() {
        let spec = serde_json::json!({
            "name": "print_file",
            "env": "lan",
            "argv": ["/usr/bin/lp", "-o", "media={media}", "{file}"],
            "timeout_ms": 5000,
            "who": "anyone",
            "params": {
                "media": {"enum": ["A4","B5"]},
                "file": {"file": ["/ws"]}
            }
        });
        let (name, body) = render_entry(&spec).unwrap();
        assert_eq!(name, "print_file");
        assert!(body.contains("who = \"anyone\""), "{body}");
        let one = tmp();
        write(one.path(), "print_file", &body);
        let t = CommandTable::load_dir(one.path());
        assert!(t.errors.is_empty(), "{:?}", t.errors);

        // 渲染不合法（自由字符串参数）时必须报错，而不是生成一份"看起来能用"的文件
        let bad = serde_json::json!({
            "name": "oops",
            "argv": ["/usr/bin/echo", "{x}"],
            "params": {"x": {"enum": []}}
        });
        assert!(render_entry(&bad).is_err());
    }

    #[test]
    fn signature_lists_params_not_argv() {
        let dir = tmp();
        write(dir.path(), "print_file", PRINT);
        let t = CommandTable::load_dir(dir.path());
        let sig = t.get("print_file").unwrap().signature();
        assert_eq!(sig, "print_file(file: /ws 下的路径, media: A4|B5)");
        assert!(!sig.contains("/usr/bin/lp"), "签名不暴露 argv：{sig}");
    }

    #[test]
    fn body_hash_is_stable_and_distinguishes_content() {
        assert_eq!(body_hash(PRINT), body_hash(PRINT));
        assert_ne!(body_hash(PRINT), body_hash(&format!("{PRINT}\n")));
    }

    #[test]
    fn cache_reloads_when_a_file_changes() {
        let dir = tmp();
        write(dir.path(), "print_file", PRINT);
        let cache = CommandCache::new(dir.path());
        assert!(cache.current().get("print_file").is_some());

        // 删掉 → 下一次 current 必须看到变化（只废这条）
        fs::remove_file(dir.path().join("print_file.toml")).unwrap();
        // mtime 粒度可能是秒：等到目录 mtime 真的变了再断言
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if cache.current().get("print_file").is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = fs::write(dir.path().join("touch"), b"");
            let _ = fs::remove_file(dir.path().join("touch"));
        }
        panic!("热加载没有生效");
    }
}
