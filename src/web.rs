//! 联网后端：搜索与抓取。**接口在这里，实现可换。**
//!
//! # 为什么是一个 trait 而不是直接写死
//!
//! 三个理由，按重要性排：
//!
//! 1. **测试。** 链路测试不能真的联网 —— 那样测试会因为别人的网站改版而变红。
//!    [`MockWeb`] 让整条工具链在没有网络的机器上跑得通。
//! 2. **换实现。** 现在走 firecrawl 的 HTTP API，将来换成自建的抓取服务、
//!    或者项目引入 `reqwest` 之后走进程内 HTTP，换的只是这个文件。
//! 3. **降级。** 没配 firecrawl 密钥时退到裸 [`Curl`]：搜不了，但「抓这个网址」
//!    还能用。有一半能力比整块功能消失好。
//!
//! # 密钥不进 argv
//!
//! `ps` 能看到任何进程的命令行。把 `Authorization: Bearer sk-...` 放进 curl 的
//! 参数里，等于在多用户机器上把密钥广播出去。所以走 curl 的 `--config` 文件：
//! 临时文件、0600、用完删掉，参数里只有文件路径。

use crate::model::BoxFuture;
use crate::shell::Shell;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 抓回来的一页。`text` 已经是正文（markdown 或纯文本），不是 HTML。
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub text: String,
}

pub trait WebBackend: Send + Sync {
    fn name(&self) -> &str;
    /// 这个后端支持搜索吗。裸 curl 不支持 —— 那时 `web_search` 工具干脆不注册，
    /// 而不是注册一个一调就报错的工具。
    fn can_search(&self) -> bool;
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>>;
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Page, String>>;
}

// ───────────────────────── firecrawl ─────────────────────────

/// 走 firecrawl 的 v1 HTTP API，用 curl 发请求。
///
/// 选 HTTP API 而不是 firecrawl 的 CLI：CLI 的参数会随版本漂，而 v1 的
/// `/search` 与 `/scrape` 是稳定契约。curl 到处都有，不用额外装东西。
pub struct Firecrawl {
    shell: Shell,
    key: String,
    base: String,
}

impl Firecrawl {
    pub fn new(shell: Shell, key: impl Into<String>) -> Firecrawl {
        Firecrawl { shell, key: key.into(), base: "https://api.firecrawl.dev".into() }
    }

    pub fn base(mut self, b: impl Into<String>) -> Firecrawl {
        self.base = b.into();
        self
    }

    async fn post(&self, path: &str, body: Value, token: &CancellationToken) -> Result<Value, String> {
        let cfg = CurlConfig::post(
            &format!("{}{}", self.base, path),
            &[
                ("Authorization", &format!("Bearer {}", self.key)),
                ("Content-Type", "application/json"),
            ],
            &body.to_string(),
        )?;
        let out = self
            .shell
            .run("curl", &["--config".into(), cfg.path_arg()], token)
            .await
            .map_err(|e| e.to_string())?;
        if !out.ok() {
            return Err(format!("curl {}", out.brief_error()));
        }
        let v: Value =
            serde_json::from_str(&out.stdout).map_err(|e| format!("响应不是 JSON：{e}"))?;
        if v.get("success").and_then(Value::as_bool) == Some(false) {
            let msg = v.get("error").and_then(Value::as_str).unwrap_or("未知错误");
            return Err(format!("firecrawl 拒绝了请求：{msg}"));
        }
        Ok(v)
    }
}

impl WebBackend for Firecrawl {
    fn name(&self) -> &str {
        "firecrawl"
    }

    fn can_search(&self) -> bool {
        true
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let v = self
                .post("/v1/search", serde_json::json!({"query": query, "limit": limit}), token)
                .await?;
            let arr = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            Ok(arr
                .iter()
                .map(|d| Hit {
                    title: str_of(d, "title"),
                    url: str_of(d, "url"),
                    snippet: str_of(d, "description"),
                })
                .filter(|h| !h.url.is_empty())
                .collect())
        })
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            let v = self
                .post(
                    "/v1/scrape",
                    serde_json::json!({"url": url, "formats": ["markdown"]}),
                    token,
                )
                .await?;
            let d = v.get("data").cloned().unwrap_or(Value::Null);
            let text = str_of(&d, "markdown");
            if text.trim().is_empty() {
                return Err("抓回来是空的（页面可能需要登录或全是脚本渲染）".into());
            }
            let title = d
                .get("metadata")
                .map(|m| str_of(m, "title"))
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| url.to_string());
            Ok(Page { url: url.to_string(), title, text })
        })
    }
}

// ───────────────────────── 裸 curl ─────────────────────────

/// 没有 firecrawl 密钥时的降级：能抓、不能搜。HTML 用一个粗糙的剥标签器转成文本。
pub struct Curl {
    shell: Shell,
}

impl Curl {
    pub fn new(shell: Shell) -> Curl {
        Curl { shell }
    }
}

impl WebBackend for Curl {
    fn name(&self) -> &str {
        "curl"
    }

    fn can_search(&self) -> bool {
        false
    }

    fn search<'a>(
        &'a self,
        _q: &'a str,
        _n: usize,
        _t: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async { Err("当前后端不支持搜索（没配 firecrawl 密钥）".to_string()) })
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            let args: Vec<String> = [
                "-sS", "-L", "--max-time", "30",
                // 有些站对空 UA 直接 403
                "-H", "User-Agent: premortem/0.1 (research assistant)",
                url,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let out = self.shell.run("curl", &args, token).await.map_err(|e| e.to_string())?;
            if !out.ok() {
                return Err(format!("curl {}", out.brief_error()));
            }
            let title = between(&out.stdout, "<title", "</title>")
                .and_then(|t| t.split_once('>').map(|(_, v)| v.trim().to_string()))
                .unwrap_or_else(|| url.to_string());
            Ok(Page { url: url.to_string(), title, text: html_to_text(&out.stdout) })
        })
    }
}

/// 剥标签。**明确是个近似**：`script` / `style` 整块丢掉，其余标签去掉，
/// 常见实体还原。要真正的正文抽取就该走 firecrawl，这里只是让降级路径有东西可看。
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let b = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'<' {
            let skip_to = ["<script", "<style"]
                .iter()
                .find(|t| lower[i..].starts_with(*t))
                .and_then(|t| {
                    let close = if *t == "<script" { "</script" } else { "</style" };
                    lower[i..].find(close).map(|off| i + off)
                });
            if let Some(j) = skip_to {
                i = j;
            }
            match lower[i..].find('>') {
                Some(off) => {
                    // 块级标签换行，行内标签不换，免得每个 <b> 都断行
                    let tag = &lower[i..i + off];
                    if tag.starts_with("<p")
                        || tag.starts_with("<div")
                        || tag.starts_with("<br")
                        || tag.starts_with("<li")
                        || tag.starts_with("<h")
                        || tag.starts_with("</p")
                        || tag.starts_with("</div")
                    {
                        out.push('\n');
                    }
                    i += off + 1;
                }
                None => break,
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    // 压掉剥标签留下的成片空行
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines() {
        let t = l.trim();
        if t.is_empty() && lines.last().map(|p: &&str| p.is_empty()).unwrap_or(true) {
            continue;
        }
        lines.push(t);
    }
    lines.join("\n").trim().to_string()
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let lower = s.to_ascii_lowercase();
    let a = lower.find(open)?;
    let b = lower[a..].find(close)? + a;
    Some(&s[a..b])
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

// ───────────────────────── curl 的配置文件 ─────────────────────────

/// 一次性的 curl `--config` 文件。**密钥只写在这里，不进命令行参数。**
///
/// Drop 时删除。删失败也不重试 —— 那只是临时目录里一个 0600 的小文件，
/// 为它引一层重试逻辑不值得，但**必须**是 0600，因为它含密钥。
struct CurlConfig {
    path: std::path::PathBuf,
}

impl CurlConfig {
    fn post(url: &str, headers: &[(&str, &str)], body: &str) -> Result<CurlConfig, String> {
        let mut s = String::new();
        s.push_str(&format!("url = {}\n", quote(url)));
        s.push_str("request = POST\n");
        for (k, v) in headers {
            s.push_str(&format!("header = {}\n", quote(&format!("{k}: {v}"))));
        }
        s.push_str(&format!("data = {}\n", quote(body)));
        s.push_str("silent\nshow-error\nlocation\nmax-time = 60\n");

        let name = format!("premortem-curl-{}.cfg", uuid::Uuid::new_v4().simple());
        // 刻意放系统临时目录，**不**放 workspace/：workspace 在工具的可读白名单里，
        // 放那儿等于让模型能用 fs_read 把密钥读出来。
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, s).map_err(|e| format!("写 curl 配置失败：{e}"))?;
        set_private(&path);
        Ok(CurlConfig { path })
    }

    fn path_arg(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for CurlConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// curl 配置文件的引号规则：双引号包起来，反斜杠与双引号转义。
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(unix)]
fn set_private(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private(_p: &std::path::Path) {}

/// 按配置挑一个后端。**这是「填了密钥就生效」那条链的落点。**
///
/// 有密钥 ⇒ firecrawl（能搜能抓）；没密钥但有 curl ⇒ 裸 curl（只能抓）；
/// 都没有 ⇒ `None`，联网工具干脆不注册。
///
/// 降级而不是报错：**有一半能力比整块功能消失好**，而且模型看得见
/// `web_search` 在不在工具清单里，比调用之后收到一句「未配置」有用。
pub fn pick(
    cfg: &crate::policy::PolicyCfg,
    key: Option<String>,
) -> Option<std::sync::Arc<dyn WebBackend>> {
    if !cfg.net {
        return None;
    }
    let shell = Shell::new(cfg.exec_allow.clone());
    if !shell.have("curl") {
        return None;
    }
    match key.filter(|k| !k.trim().is_empty()) {
        Some(k) => Some(std::sync::Arc::new(Firecrawl::new(shell, k))),
        None => Some(std::sync::Arc::new(Curl::new(shell))),
    }
}

// ───────────────────────── 测试用 ─────────────────────────

/// 固定应答的后端。链路测试用它，**不联网**。
pub struct MockWeb {
    pub hits: Vec<Hit>,
    pub pages: std::collections::HashMap<String, Page>,
    pub fail: Option<String>,
}

impl MockWeb {
    pub fn new() -> MockWeb {
        MockWeb { hits: vec![], pages: Default::default(), fail: None }
    }

    pub fn hit(mut self, title: &str, url: &str, snippet: &str) -> MockWeb {
        self.hits.push(Hit {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
        });
        self
    }

    pub fn page(mut self, url: &str, title: &str, text: &str) -> MockWeb {
        self.pages.insert(
            url.to_string(),
            Page { url: url.into(), title: title.into(), text: text.into() },
        );
        self
    }

    pub fn failing(mut self, why: &str) -> MockWeb {
        self.fail = Some(why.into());
        self
    }
}

impl Default for MockWeb {
    fn default() -> Self {
        Self::new()
    }
}

impl WebBackend for MockWeb {
    fn name(&self) -> &str {
        "mock"
    }

    fn can_search(&self) -> bool {
        true
    }

    fn search<'a>(
        &'a self,
        _q: &'a str,
        limit: usize,
        _t: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.hits.iter().take(limit).cloned().collect()),
            }
        })
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        _t: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            self.pages.get(url).cloned().ok_or_else(|| format!("mock 里没有 {url}"))
        })
    }
}
