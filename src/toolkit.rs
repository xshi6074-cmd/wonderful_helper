//! 给模型的只读工具链：搜、抓、读、找、检索、看结构。
//!
//! # 六个工具，一条原则：**返回值的大小由模型决定，不由外部数据决定**
//!
//! 抓一个网页可能是 20 字也可能是 20 万字。要是直接把正文当工具结果返回，
//! 一次抓取就能把这一轮的上下文吃光，而症状是「模型忽然变笨了」——
//! 没有人会把它联想到三步之前的一次 `web_fetch`。
//!
//! 所以抓取类工具**先落盘再返回摘要**（见 [`crate::scratch`]），
//! 模型想细看再用 `fs_read` 带 offset 去取。每个工具都有条数/字节/行长上限，
//! 截断时页脚**一定给出续读的办法** —— 只说「已截断」会让模型卡在原地。
//!
//! # 全部只读
//!
//! 没有写文件的工具，没有 shell 工具。模型能改的只有推断图（走 op），
//! 能落盘的只有工具自己写进 `workspace/` 的产物。理由见 [`crate::policy`]。
//!
//! # 检索用 ripgrep 的实现
//!
//! `ignore` / `grep-searcher` / `globset` 就是 ripgrep 的本体。自己写目录遍历
//! 和 `.gitignore` 解析是典型的「看着简单、边界能写一年」：嵌套 ignore 文件、
//! 否定规则、符号链接、二进制探测、编码嗅探。用现成的。

use crate::model::{BoxFuture, Call};
use crate::policy::Policy;
use crate::scratch::Scratch;
use crate::tools::{Concurrency, Registry, Tool, ToolResult};
use crate::web::WebBackend;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

/// 工具链的可观测量。**每轮打出来**，异常一眼能看出来：
///
/// - `denied` 一直涨 ⇒ 模型在反复撞白名单，说明工具说明没写清楚可读范围。
/// - `clipped / calls` 高 ⇒ 上限设小了，或者模型在拿 grep 当 read 用。
/// - `bytes_out` 是这条链真正灌进 prompt 的量，比「调了几次」有用得多。
#[derive(Debug, Default)]
pub struct Metrics {
    pub calls: AtomicU64,
    pub denied: AtomicU64,
    pub clipped: AtomicU64,
    pub bytes_out: AtomicU64,
    pub files_written: AtomicU64,
    pub web_errors: AtomicU64,
}

impl Metrics {
    fn call(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
    fn deny(&self) {
        self.denied.fetch_add(1, Ordering::Relaxed);
    }
    fn out(&self, n: usize, clipped: bool) {
        self.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
        if clipped {
            self.clipped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn line(&self) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "工具链：调用 {} · 拒绝 {} · 截断 {} · 回灌 {} 字节 · 落盘 {} 个 · 网络失败 {}",
            g(&self.calls),
            g(&self.denied),
            g(&self.clipped),
            g(&self.bytes_out),
            g(&self.files_written),
            g(&self.web_errors),
        )
    }

    pub fn get(&self, which: &str) -> u64 {
        match which {
            "calls" => self.calls.load(Ordering::Relaxed),
            "denied" => self.denied.load(Ordering::Relaxed),
            "clipped" => self.clipped.load(Ordering::Relaxed),
            "bytes_out" => self.bytes_out.load(Ordering::Relaxed),
            "files_written" => self.files_written.load(Ordering::Relaxed),
            "web_errors" => self.web_errors.load(Ordering::Relaxed),
            _ => 0,
        }
    }
}

/// 六个工具共用的依赖。
#[derive(Clone)]
pub struct Deps {
    pub policy: Arc<Policy>,
    pub scratch: Arc<Scratch>,
    /// 没配就不注册联网工具。**宁可少一个工具，也不要一个一调就报错的工具** ——
    /// 后者会让模型反复去试，白烧 token。
    pub web: Option<Arc<dyn WebBackend>>,
    pub metrics: Arc<Metrics>,
}

/// 按当前配置注册可用的工具。
pub fn register(mut reg: Registry, d: &Deps) -> Registry {
    reg = reg
        .with(Arc::new(FsRead(d.clone())))
        .with(Arc::new(FsGrep(d.clone())))
        .with(Arc::new(FsFind(d.clone())))
        .with(Arc::new(RepoTree(d.clone())));
    if let Some(w) = &d.web {
        reg = reg.with(Arc::new(WebFetch(d.clone())));
        if w.can_search() {
            reg = reg.with(Arc::new(WebSearch(d.clone())));
        }
    }
    reg
}

// ───────────────────────── 参数解析 ─────────────────────────

fn s_arg(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(|s| s.to_string()).filter(|s| !s.trim().is_empty())
}

fn u_arg(v: &Value, k: &str) -> Option<usize> {
    v.get(k).and_then(Value::as_u64).map(|n| n as usize)
}

/// 参数缺失的报错要带上**正确的用法**。只说「缺少 path」，模型下一次很可能
/// 换个同样错的写法再试一遍。
fn need(call: &Call, k: &str, usage: &str) -> ToolResult {
    ToolResult::failed(call, format!("缺少参数 {k}。用法：{usage}"))
}

// ───────────────────────── fs_read ─────────────────────────

struct FsRead(Deps);

impl Tool for FsRead {
    fn name(&self) -> &str {
        "fs_read"
    }

    fn description(&self) -> &str {
        "读一个文件，带行号。只读，且只能读授权目录内的文件。\
         args: {path: 相对或绝对路径, offset?: 起始行号（1 起，默认 1）, limit?: 读多少行}。\
         文件大时先读前几百行，再按需要挪 offset 继续 —— 不要试图一次读完。\
         工具抓回来的网页也在这里读，路径形如 workspace/xxx.md。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "相对或绝对路径，如 src/core.rs 或 workspace/xxx.md" },
                "offset": { "type": "integer", "description": "从第几行开始读，1 起，默认 1" },
                "limit": { "type": "integer", "description": "读多少行" }
            },
            "required": ["path"]
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, _t: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let d = &self.0;
            d.metrics.call();
            let Some(p) = s_arg(&call.args, "path") else {
                return need(&call, "path", "{path: \"src/core.rs\", offset: 1, limit: 200}");
            };
            let real = match d.policy.check_path(Path::new(&p)) {
                Ok(r) => r,
                Err(e) => {
                    d.metrics.deny();
                    return ToolResult::failed(&call, e.0);
                }
            };
            let meta = match std::fs::metadata(&real) {
                Ok(m) => m,
                Err(e) => return ToolResult::failed(&call, format!("读不到 {p}：{e}")),
            };
            if meta.is_dir() {
                return ToolResult::failed(
                    &call,
                    format!("{p} 是目录。列内容用 repo_tree，按名字找用 fs_find。"),
                );
            }
            if meta.len() > d.policy.cfg.max_file_bytes {
                return ToolResult::failed(
                    &call,
                    format!(
                        "{p} 有 {} 字节，超过单文件上限 {}。用 fs_grep 定位需要的那几行。",
                        meta.len(),
                        d.policy.cfg.max_file_bytes
                    ),
                );
            }
            let text = match std::fs::read(&real) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => return ToolResult::failed(&call, format!("读不了 {p}：{e}")),
            };

            let offset = u_arg(&call.args, "offset").unwrap_or(1).max(1);
            let limit = u_arg(&call.args, "limit").unwrap_or(d.policy.cfg.max_lines);
            let total = text.lines().count();
            if offset > total && total > 0 {
                return ToolResult::failed(
                    &call,
                    format!("{p} 只有 {total} 行，offset={offset} 越界了"),
                );
            }
            // 行号跟着走：模型引用「第几行」时说的必须是文件里的真实行号
            let body: String = text
                .lines()
                .enumerate()
                .skip(offset - 1)
                .take(limit)
                .map(|(i, l)| format!("{:>6}\t{}\n", i + 1, l))
                .collect();

            let clipped = d.policy.clip().apply(&body);
            let next = offset + clipped.lines_shown;
            let out = format!(
                "{p}（共 {total} 行）\n{}",
                clipped.render(&format!("续读：fs_read path={p} offset={next}"))
            );
            let more = next <= total;
            d.metrics.out(out.len(), clipped.truncated || more);
            let tail = if more && !clipped.truncated {
                format!("\n[还有 {} 行。续读：fs_read path={p} offset={next}]", total - next + 1)
            } else {
                String::new()
            };
            ToolResult::ok(&call, format!("{out}{tail}"))
        })
    }
}

// ───────────────────────── fs_grep ─────────────────────────

struct FsGrep(Deps);

impl Tool for FsGrep {
    fn name(&self) -> &str {
        "fs_grep"
    }

    fn description(&self) -> &str {
        "在授权目录里按正则搜内容，返回 路径:行号: 内容。遵守 .gitignore。\
         args: {pattern: 正则, path?: 从哪个目录开始, glob?: 只搜匹配的文件如 \"*.py\", max?: 最多几条}。\
         先用它定位，再用 fs_read 读那一段 —— 不要为了找一行内容去通读整个文件。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "正则表达式" },
                "path": { "type": "string", "description": "从哪个目录开始，默认当前目录" },
                "glob": { "type": "string", "description": "只搜匹配的文件，如 *.py 或 src/**/*.rs" },
                "max": { "type": "integer", "description": "最多返回几条" }
            },
            "required": ["pattern"]
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, token: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let d = self.0.clone();
            d.metrics.call();
            let Some(pattern) = s_arg(&call.args, "pattern") else {
                return need(&call, "pattern", "{pattern: \"fn main\", glob: \"*.rs\"}");
            };
            let root = s_arg(&call.args, "path").unwrap_or_else(|| ".".into());
            let glob = s_arg(&call.args, "glob");
            let max = u_arg(&call.args, "max")
                .unwrap_or(d.policy.cfg.max_matches)
                .min(d.policy.cfg.max_matches);
            let real = match d.policy.check_path(Path::new(&root)) {
                Ok(r) => r,
                Err(e) => {
                    d.metrics.deny();
                    return ToolResult::failed(&call, e.0);
                }
            };

            // 遍历 + 搜索是阻塞 IO，放 spawn_blocking；取消由外层 run_tools 的
            // 硬上限兜底（这里的工作量本来就有 max_matches 封顶）。
            let d2 = d.clone();
            let job = tokio::task::spawn_blocking(move || {
                grep_blocking(&d2, &real, &pattern, glob.as_deref(), max)
            });
            let res = tokio::select! {
                biased;
                _ = token.cancelled() => return ToolResult::interrupted(&call),
                r = job => r,
            };
            match res {
                Ok(Ok((lines, _, scanned))) if lines.is_empty() => ToolResult::ok(
                    &call,
                    format!("没有匹配（扫了 {scanned} 个文件）。换个更宽的正则，或去掉 glob 再试。"),
                ),
                Ok(Ok((lines, hit_cap, scanned))) => {
                    let joined = lines.join("\n");
                    let clipped = d.policy.clip().apply(&joined);
                    let note = if hit_cap {
                        format!("\n[只列了前 {max} 条（扫了 {scanned} 个文件）。缩小 glob 或用更精确的正则。]")
                    } else {
                        format!("\n[{} 条匹配，扫了 {scanned} 个文件]", lines.len())
                    };
                    let out =
                        format!("{}{note}", clipped.render("用更精确的 pattern 或 glob 缩小范围"));
                    d.metrics.out(out.len(), clipped.truncated || hit_cap);
                    ToolResult::ok(&call, out)
                }
                Ok(Err(e)) => ToolResult::failed(&call, e),
                Err(e) => ToolResult::failed(&call, format!("检索任务失败：{e}")),
            }
        })
    }
}

fn grep_blocking(
    d: &Deps,
    root: &Path,
    pattern: &str,
    glob: Option<&str>,
    max: usize,
) -> Result<(Vec<String>, bool, usize), String> {
    use grep_regex::RegexMatcher;
    use grep_searcher::sinks::UTF8;
    use grep_searcher::{BinaryDetection, SearcherBuilder};

    let matcher = RegexMatcher::new_line_matcher(pattern)
        .map_err(|e| format!("正则不合法：{e}"))?;
    let set = build_globset(glob)?;
    let mut searcher = SearcherBuilder::new()
        // 二进制文件里搜出来的「行」对模型毫无意义，还可能是一大坨乱码
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();

    let mut out: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut hit_cap = false;
    for entry in walker(d, root) {
        if out.len() >= max {
            hit_cap = true;
            break;
        }
        let Some(entry) = entry else { continue };
        let path = entry.path().to_path_buf();
        let rel = rel_of(root, &path);
        if let Some(set) = &set
            && !set.is_match(&rel) && !set.is_match(path.file_name().unwrap_or_default()) {
                continue;
            }
        scanned += 1;
        let cap = max;
        let mut here: Vec<String> = Vec::new();
        let r = searcher.search_path(
            &matcher,
            &path,
            UTF8(|lnum, line| {
                here.push(format!("{}:{}: {}", rel, lnum, line.trim_end()));
                Ok(here.len() + out.len() < cap)
            }),
        );
        if r.is_err() {
            continue; // 读不了的单个文件不该让整次检索失败
        }
        out.extend(here);
    }
    if out.len() >= max {
        hit_cap = true;
        out.truncate(max);
    }
    Ok((out, hit_cap, scanned))
}

// ───────────────────────── fs_find ─────────────────────────

struct FsFind(Deps);

impl Tool for FsFind {
    fn name(&self) -> &str {
        "fs_find"
    }

    fn description(&self) -> &str {
        "按文件名模式找文件，遵守 .gitignore。\
         args: {glob: 如 \"**/*.py\" 或 \"train*.py\", path?: 从哪个目录开始}。\
         想知道某个东西在哪个文件里用 fs_grep；只想按名字找用这个。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "glob": { "type": "string", "description": "文件名模式，如 **/*.py 或 train*.py" },
                "path": { "type": "string", "description": "从哪个目录开始" }
            },
            "required": ["glob"]
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, _t: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let d = &self.0;
            d.metrics.call();
            let Some(glob) = s_arg(&call.args, "glob") else {
                return need(&call, "glob", "{glob: \"**/*.py\"}");
            };
            let root = s_arg(&call.args, "path").unwrap_or_else(|| ".".into());
            let real = match d.policy.check_path(Path::new(&root)) {
                Ok(r) => r,
                Err(e) => {
                    d.metrics.deny();
                    return ToolResult::failed(&call, e.0);
                }
            };
            let set = match build_globset(Some(&glob)) {
                Ok(s) => s,
                Err(e) => return ToolResult::failed(&call, e),
            };
            let cap = d.policy.cfg.max_entries;
            let mut hits: Vec<String> = Vec::new();
            let mut capped = false;
            for entry in walker(d, &real) {
                let Some(entry) = entry else { continue };
                let rel = rel_of(&real, entry.path());
                let name = entry.path().file_name().unwrap_or_default();
                if set.as_ref().is_some_and(|s| s.is_match(&rel) || s.is_match(name)) {
                    if hits.len() >= cap {
                        capped = true;
                        break;
                    }
                    hits.push(rel);
                }
            }
            if hits.is_empty() {
                return ToolResult::ok(&call, format!("没有文件匹配 {glob}"));
            }
            hits.sort();
            let clipped = d.policy.clip().apply(&hits.join("\n"));
            let note = if capped {
                format!("\n[只列了前 {cap} 个，还有更多。用更窄的 glob。]")
            } else {
                format!("\n[{} 个文件]", hits.len())
            };
            let out = format!("{}{note}", clipped.render("用更窄的 glob"));
            d.metrics.out(out.len(), clipped.truncated || capped);
            ToolResult::ok(&call, out)
        })
    }
}

// ───────────────────────── repo_tree ─────────────────────────

struct RepoTree(Deps);

impl Tool for RepoTree {
    fn name(&self) -> &str {
        "repo_tree"
    }

    fn description(&self) -> &str {
        "看一个目录的结构：文件树 + 按扩展名的统计。遵守 .gitignore。\
         args: {path?: 目录, depth?: 展开几层（默认 3）}。\
         刚接触一个仓库时先用它，比一个个 fs_find 快得多。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目录，默认当前目录" },
                "depth": { "type": "integer", "description": "展开几层，默认 3" }
            },
            "required": []
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, _t: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let d = &self.0;
            d.metrics.call();
            let root = s_arg(&call.args, "path").unwrap_or_else(|| ".".into());
            let depth = u_arg(&call.args, "depth").unwrap_or(3).min(d.policy.cfg.max_depth);
            let real = match d.policy.check_path(Path::new(&root)) {
                Ok(r) => r,
                Err(e) => {
                    d.metrics.deny();
                    return ToolResult::failed(&call, e.0);
                }
            };
            let cap = d.policy.cfg.max_entries;
            let mut lines: Vec<String> = Vec::new();
            let mut dirs = 0usize;
            let mut files = 0usize;
            let mut by_ext: BTreeMap<String, usize> = BTreeMap::new();
            let mut capped = false;

            for entry in walker_depth(d, &real, depth) {
                let Some(entry) = entry else { continue };
                let p = entry.path();
                if p == real {
                    continue;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    dirs += 1;
                } else {
                    files += 1;
                    let ext = p
                        .extension()
                        .map(|e| e.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "(无后缀)".into());
                    *by_ext.entry(ext).or_default() += 1;
                }
                if lines.len() >= cap {
                    capped = true;
                    continue; // 继续统计，只是不再画树
                }
                let indent = "  ".repeat(entry.depth().saturating_sub(1));
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                lines.push(if is_dir {
                    format!("{indent}{name}/")
                } else {
                    format!("{indent}{name}")
                });
            }

            // 「结构」的信息量大半在这一行统计上：一个仓库是 Python 还是 Rust、
            // 有没有测试目录、配置多不多，看扩展名分布比看树快。
            let mut ext: Vec<(String, usize)> = by_ext.into_iter().collect();
            ext.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let summary = format!(
                "{root}：{dirs} 个目录 / {files} 个文件（展开 {depth} 层）\n按扩展名：{}",
                ext.iter()
                    .take(12)
                    .map(|(e, n)| format!("{e} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let clipped = d.policy.clip().apply(&lines.join("\n"));
            let note = if capped {
                format!("\n[树只画了前 {cap} 项（统计是全量的）。减小 depth 或指定子目录。]")
            } else {
                String::new()
            };
            let out = format!(
                "{summary}\n\n{}{note}",
                clipped.render("减小 depth 或指定子目录")
            );
            d.metrics.out(out.len(), clipped.truncated || capped);
            ToolResult::ok(&call, out)
        })
    }
}

// ───────────────────────── web_search ─────────────────────────

struct WebSearch(Deps);

impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "网页搜索，返回标题 / 网址 / 摘要。\
         args: {query: 查询词, limit?: 几条（默认 5）}。\
         结果只是线索：要看正文得再用 web_fetch 抓具体网址。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "查询词" },
                "limit": { "type": "integer", "description": "返回几条，默认 5" }
            },
            "required": ["query"]
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, token: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let d = &self.0;
            d.metrics.call();
            let Some(web) = &d.web else {
                return ToolResult::failed(&call, "没有可用的联网后端");
            };
            let Some(q) = s_arg(&call.args, "query") else {
                return need(&call, "query", "{query: \"contrastive learning tau\", limit: 5}");
            };
            let limit = u_arg(&call.args, "limit").unwrap_or(5).clamp(1, 20);
            let hits = match web.search(&q, limit, &token).await {
                Ok(h) => h,
                Err(e) => {
                    d.metrics.web_errors.fetch_add(1, Ordering::Relaxed);
                    return ToolResult::failed(&call, format!("搜索失败：{e}"));
                }
            };
            if hits.is_empty() {
                return ToolResult::ok(&call, format!("「{q}」没有结果"));
            }
            // 全量结果落盘，返回值只给精简版 —— 摘要有时一条就好几百字
            let full = hits
                .iter()
                .map(|h| format!("## {}\n{}\n\n{}\n", h.title, h.url, h.snippet))
                .collect::<Vec<_>>()
                .join("\n");
            let saved = d.scratch.put(&q, &q, "md", &full).ok();
            if saved.is_some() {
                d.metrics.files_written.fetch_add(1, Ordering::Relaxed);
            }
            let brief = hits
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    format!("{}. {}\n   {}\n   {}", i + 1, h.title, h.url, short(&h.snippet, 160))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut out = format!("「{q}」{} 条结果：\n{brief}", hits.len());
            if let Some(s) = saved {
                out.push_str(&format!("\n\n[完整摘要已存到 {}，需要时 fs_read]", s.rel));
            }
            d.metrics.out(out.len(), false);
            ToolResult::ok(&call, out)
        })
    }
}

// ───────────────────────── web_fetch ─────────────────────────

struct WebFetch(Deps);

impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "抓一个网址，转成正文存到 workspace/，返回开头一段和存放路径。\
         args: {url: 完整网址, lines?: 先看多少行（默认 80）}。\
         只允许抓白名单域名。正文全文在返回的路径里，用 fs_read / fs_grep 继续看 —— \
         不要指望这个工具把整页内容都吐给你。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "完整网址，必须带 http:// 或 https://" },
                "lines": { "type": "integer", "description": "先看开头多少行，默认 80" }
            },
            "required": ["url"]
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, token: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let d = &self.0;
            d.metrics.call();
            let Some(web) = &d.web else {
                return ToolResult::failed(&call, "没有可用的联网后端");
            };
            let Some(raw) = s_arg(&call.args, "url") else {
                return need(&call, "url", "{url: \"https://arxiv.org/abs/2103.00020\"}");
            };
            let url = match d.policy.check_url(&raw) {
                Ok(u) => u,
                Err(e) => {
                    d.metrics.deny();
                    return ToolResult::failed(&call, e.0);
                }
            };
            let page = match web.fetch(&url, &token).await {
                Ok(p) => p,
                Err(e) => {
                    d.metrics.web_errors.fetch_add(1, Ordering::Relaxed);
                    return ToolResult::failed(&call, format!("抓取失败：{e}"));
                }
            };
            let saved = match d.scratch.put(&url, &page.title, "md", &page.text) {
                Ok(s) => s,
                Err(e) => return ToolResult::failed(&call, format!("存不下来：{e}")),
            };
            d.metrics.files_written.fetch_add(1, Ordering::Relaxed);

            let head_lines = u_arg(&call.args, "lines").unwrap_or(80).min(d.policy.cfg.max_lines);
            let head: String =
                page.text.lines().take(head_lines).collect::<Vec<_>>().join("\n");
            let clipped = d.policy.clip().apply(&head);
            let out = format!(
                "{}\n{}\n共 {} 行 / {} 字节，全文已存到 {}\n\n{}\n\n[以上是开头 {} 行。\
                 要看后面：fs_read path={} offset={}；要找某段：fs_grep pattern=... path={}]",
                page.title,
                page.url,
                saved.lines,
                saved.bytes,
                saved.rel,
                clipped.text.trim_end(),
                clipped.lines_shown,
                saved.rel,
                clipped.lines_shown + 1,
                saved.rel,
            );
            d.metrics.out(out.len(), saved.lines > clipped.lines_shown);
            ToolResult::ok(&call, out)
        })
    }
}

// ───────────────────────── 遍历与匹配 ─────────────────────────

/// `.gitignore` 感知的遍历。**不跟随符号链接** —— 跟随就等于给了一条绕过
/// 授权目录的路，而 `check_path` 只在入口处把过一次关。
fn walker(d: &Deps, root: &Path) -> impl Iterator<Item = Option<ignore::DirEntry>> {
    walker_depth(d, root, d.policy.cfg.max_depth).filter(|e| {
        e.as_ref().is_none_or(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
    })
}

fn walker_depth(
    d: &Deps,
    root: &Path,
    depth: usize,
) -> impl Iterator<Item = Option<ignore::DirEntry>> {
    let deny: Vec<String> = d.policy.cfg.deny_names.clone();
    ignore::WalkBuilder::new(root)
        .max_depth(Some(depth))
        .follow_links(false)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        // 名字黑名单在遍历时就剪掉，比走到底再逐个 check_path 快一个数量级
        .filter_entry(move |e| {
            let n = e.file_name().to_string_lossy();
            !deny.iter().any(|x| n.as_ref() == x.as_str())
        })
        .build()
        .map(|r| r.ok())
}

fn build_globset(pat: Option<&str>) -> Result<Option<globset::GlobSet>, String> {
    let Some(p) = pat else { return Ok(None) };
    let mut b = globset::GlobSetBuilder::new();
    b.add(globset::Glob::new(p).map_err(|e| format!("glob 不合法：{e}"))?);
    // "*.rs" 应该也能命中 "src/core.rs"，否则模型每次都得写 "**/*.rs"
    if !p.contains('/')
        && let Ok(g) = globset::Glob::new(&format!("**/{p}")) {
            b.add(g);
        }
    b.build().map(Some).map_err(|e| format!("glob 编译失败：{e}"))
}

fn rel_of(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

fn short(s: &str, n: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n {
        t
    } else {
        format!("{}…", t.chars().take(n).collect::<String>())
    }
}

/// 从 `config.json` 直接搭出依赖。**这一条把配置文件和闸门接起来** ——
/// 少了它，`Settings.tools` 就是个没人读的装饰性字段，而那种缺口在链路测试里
/// 是看不出来的（每一半都工作，只是从没接上）。
pub fn from_settings(
    s: &crate::config::Settings,
    base: &Path,
    web: Option<Arc<dyn WebBackend>>,
) -> std::io::Result<Deps> {
    deps(s.tools.clone(), base, &crate::config::scratch_dir(base), web)
}

/// 搭出一整套依赖。**顺序在这里是有意义的**，所以只留这一个入口：
///
/// 1. 先把 `workspace/` 建出来并 canonical 化；
/// 2. 再把它加进可读白名单 —— 漏了这一步，模型抓完东西自己读不了，
///    而症状是「fs_read 说路径不在授权目录内」，看起来像权限配错了；
/// 3. 最后才构造 [`Policy`]，因为它会把解析不了的 root 直接丢掉。
pub fn deps(
    mut cfg: crate::policy::PolicyCfg,
    base: &Path,
    scratch_root: &Path,
    web: Option<Arc<dyn WebBackend>>,
) -> std::io::Result<Deps> {
    let scratch = Scratch::open(scratch_root)?;
    let s = scratch.root().to_string_lossy().into_owned();
    if !cfg.roots.contains(&s) {
        cfg.roots.push(s);
    }
    Ok(Deps {
        policy: Arc::new(Policy::new(cfg, base)),
        scratch: Arc::new(scratch),
        web,
        metrics: Arc::new(Metrics::default()),
    })
}
