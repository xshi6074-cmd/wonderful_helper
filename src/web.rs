//! 联网后端：**抓**与**搜**分开配，各自可换。
//!
//! # 为什么拆成两半
//!
//! 上一版按厂商切（一个 `Firecrawl` 同时管搜和抓），换成 crawl4ai 时立刻暴露问题：
//! **crawl4ai 只做抓取，不做搜索**。按厂商切就只能要么丢掉搜索，要么为了搜索
//! 继续绑着一个要密钥的服务。拆成 `fetch` / `search` 两个位置之后，
//! 「本地 crawl4ai 抓 + 自建 SearXNG 搜」这种全本地、全免密钥的组合才配得出来。
//!
//! # 本地优先，不要密钥
//!
//! | 位置 | 默认 | 要密钥 | 怎么起 |
//! |---|---|---|---|
//! | 抓 | crawl4ai 服务 | 否 | `docker run -p 11235:8080 unclecode/crawl4ai` |
//! | 抓 | crawl4ai CLI | 否 | `pip install crawl4ai && crawl4ai-setup` |
//! | 抓 | firecrawl | 是 | 云服务 |
//! | 抓 | http | 否 | 裸 GET + 剥标签，兜底 |
//! | 搜 | searxng | 否 | 自建，`GET /search?format=json` |
//! | 搜 | firecrawl | 是 | 云服务 |
//!
//! # 后端自己的地址不过闸门
//!
//! [`crate::policy::Policy::check_url`] 挡的是**模型请求的目标网址**。
//! 后端自身的 base_url（`http://localhost:11235`）是运维配置，不是模型输入，
//! 所以它不该被内网检查挡住 —— 否则本地部署一个都用不了。
//! 模型给的目标网址仍然照常过闸门，SSRF 防线没有松。

use crate::model::BoxFuture;
use crate::shell::Shell;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
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
    /// 这个后端支持搜索吗。不支持时 `web_search` 干脆不注册 ——
    /// 不给模型一个一调就报错的工具。
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
    /// 探活：**给真实链路测试用**。返回一句人能看懂的话。
    ///
    /// 有它才能把「抓取失败」拆成「服务没起来」和「这个页面抓不了」——
    /// 两者的下一步完全不同，混在一起会让人去改错的东西。
    fn probe<'a>(&'a self, token: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        let _ = token;
        Box::pin(async { Ok("（这个后端没有探活）".to_string()) })
    }
}

// ───────────────────────── 配置 ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fetcher {
    /// crawl4ai 的本地服务。**默认，不要密钥。**
    Crawl4ai,
    /// crawl4ai 的命令行。同样不要密钥，适合不想跑 Docker 的情况。
    Crawl4aiCli,
    /// firecrawl 云服务，要密钥。
    Firecrawl,
    /// 裸 GET + 剥标签。永远可用，但正文抽取很粗。
    Http,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Searcher {
    /// 自建 SearXNG，不要密钥。
    Searxng,
    Firecrawl,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebCfg {
    pub fetch: Fetcher,
    /// crawl4ai 服务 / firecrawl 的地址。
    pub fetch_base: String,
    pub search: Searcher,
    /// SearXNG 的地址。
    pub search_base: String,
}

impl Default for WebCfg {
    fn default() -> Self {
        WebCfg {
            // 默认走本地 crawl4ai：不用密钥，起一条 docker 就能用
            fetch: Fetcher::Crawl4ai,
            fetch_base: "http://localhost:11235".into(),
            // 搜索没有免密钥的默认可用项，所以默认关掉而不是配一个起不来的
            search: Searcher::None,
            search_base: "http://localhost:8080".into(),
        }
    }
}

/// 按配置搭后端。`key` 只有 firecrawl 用得上。
///
/// 返回 `None` 表示这一轮不注册任何联网工具。
pub fn build(cfg: &WebCfg, policy: &crate::policy::PolicyCfg, key: Option<String>) -> Option<Arc<dyn WebBackend>> {
    if !policy.net {
        return None;
    }
    let http = client();
    let key = key.filter(|k| !k.trim().is_empty());

    let fetcher: Option<Arc<dyn WebBackend>> = match cfg.fetch {
        Fetcher::Crawl4ai => Some(Arc::new(Crawl4ai::new(http.clone(), &cfg.fetch_base))),
        Fetcher::Crawl4aiCli => {
            let sh = Shell::new(policy.exec_allow.clone()).timeout(Duration::from_secs(90));
            sh.have("crwl").then(|| Arc::new(Crawl4aiCli::new(sh)) as Arc<dyn WebBackend>)
        }
        Fetcher::Firecrawl => key
            .clone()
            .map(|k| Arc::new(Firecrawl::new(http.clone(), k, &cfg.fetch_base)) as Arc<dyn WebBackend>),
        Fetcher::Http => Some(Arc::new(PlainHttp::new(http.clone()))),
        Fetcher::None => None,
    };
    let searcher: Option<Arc<dyn WebBackend>> = match cfg.search {
        Searcher::Searxng => Some(Arc::new(Searxng::new(http.clone(), &cfg.search_base))),
        Searcher::Firecrawl => key
            .map(|k| Arc::new(Firecrawl::new(http, k, "https://api.firecrawl.dev")) as Arc<dyn WebBackend>),
        Searcher::None => None,
    };
    match (fetcher, searcher) {
        (None, None) => None,
        (f, s) => Some(Arc::new(Combo { fetch: f, search: s })),
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .user_agent("premortem/0.1 (research assistant)")
        .build()
        .unwrap_or_default()
}

/// 把「抓」和「搜」两个后端粘起来。
pub struct Combo {
    pub fetch: Option<Arc<dyn WebBackend>>,
    pub search: Option<Arc<dyn WebBackend>>,
}

impl WebBackend for Combo {
    fn name(&self) -> &str {
        "combo"
    }

    fn can_search(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.can_search())
    }

    fn search<'a>(
        &'a self,
        q: &'a str,
        n: usize,
        t: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            match &self.search {
                Some(s) => s.search(q, n, t).await,
                None => Err("没有配置搜索后端（config.json 的 web.search）".into()),
            }
        })
    }

    fn fetch<'a>(&'a self, url: &'a str, t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            match &self.fetch {
                Some(f) => f.fetch(url, t).await,
                None => Err("没有配置抓取后端（config.json 的 web.fetch）".into()),
            }
        })
    }

    fn probe<'a>(&'a self, t: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for (tag, b) in [("抓", &self.fetch), ("搜", &self.search)] {
                match b {
                    None => out.push(format!("{tag}：未配置")),
                    Some(b) => match b.probe(t).await {
                        Ok(m) => out.push(format!("{tag}：{} {m}", b.name())),
                        Err(e) => out.push(format!("{tag}：{} 不可用 —— {e}", b.name())),
                    },
                }
            }
            Ok(out.join("\n"))
        })
    }
}

// ───────────────────────── crawl4ai（服务） ─────────────────────────

/// 本地 crawl4ai 服务。**不需要密钥。**
///
/// `docker run -d -p 11235:8080 --shm-size=1g unclecode/crawl4ai:latest`
///
/// 先打 `/md`（0.6+ 有，最省事），404 就退回 `/crawl` —— 后者从第一版就有。
/// 两条都试是因为版本差异在本地部署里太常见，而症状（404）看起来像地址写错了。
pub struct Crawl4ai {
    http: reqwest::Client,
    base: String,
}

impl Crawl4ai {
    pub fn new(http: reqwest::Client, base: &str) -> Crawl4ai {
        Crawl4ai { http, base: base.trim_end_matches('/').to_string() }
    }
}

impl WebBackend for Crawl4ai {
    fn name(&self) -> &str {
        "crawl4ai"
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
        Box::pin(async { Err("crawl4ai 只做抓取，搜索要另配（config.json 的 web.search）".into()) })
    }

    fn fetch<'a>(&'a self, url: &'a str, t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            let md = self.http.post(format!("{}/md", self.base)).json(&serde_json::json!({
                "url": url, "f": "fit"
            }));
            let r = guard(t, md.send()).await?;
            if r.status().is_success() {
                let v: Value = r.json().await.map_err(|e| format!("/md 响应不是 JSON：{e}"))?;
                let text = v["markdown"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty() {
                    return Ok(Page { url: url.into(), title: title_of(&text, url), text });
                }
            }
            // 老版本没有 /md，退回 /crawl
            let body = serde_json::json!({
                "urls": [url],
                "browser_config": { "type": "BrowserConfig", "params": { "headless": true } },
                "crawler_config": { "type": "CrawlerRunConfig", "params": {} }
            });
            let r = guard(t, self.http.post(format!("{}/crawl", self.base)).json(&body).send()).await?;
            let st = r.status();
            let v: Value = r.json().await.map_err(|e| format!("/crawl 响应不是 JSON（{st}）：{e}"))?;
            let first = v["results"].get(0).cloned().unwrap_or(Value::Null);
            let text = first["markdown"]["fit_markdown"]
                .as_str()
                .or_else(|| first["markdown"]["raw_markdown"].as_str())
                .or_else(|| first["markdown"].as_str())
                .unwrap_or("")
                .to_string();
            if text.trim().is_empty() {
                return Err(format!(
                    "抓回来是空的（{st}）。页面可能需要登录，或 crawl4ai 没渲染出内容"
                ));
            }
            let title = first["metadata"]["title"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(String::from)
                .unwrap_or_else(|| title_of(&text, url));
            Ok(Page { url: url.into(), title, text })
        })
    }

    fn probe<'a>(&'a self, t: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let r = guard(t, self.http.get(format!("{}/health", self.base)).send()).await?;
            if r.status().is_success() {
                let v: Value = r.json().await.unwrap_or(Value::Null);
                let ver = v["version"].as_str().unwrap_or("?");
                Ok(format!("服务在 {}（版本 {ver}）", self.base))
            } else {
                Err(format!("{} 返回 {}", self.base, r.status()))
            }
        })
    }
}

// ───────────────────────── crawl4ai（CLI） ─────────────────────────

/// crawl4ai 的命令行。`pip install crawl4ai && crawl4ai-setup`，同样不要密钥。
///
/// 给不想跑 Docker 的情况留的。走 [`Shell`] ⇒ argv 数组、有超时、会 kill。
pub struct Crawl4aiCli {
    shell: Shell,
}

impl Crawl4aiCli {
    pub fn new(shell: Shell) -> Crawl4aiCli {
        Crawl4aiCli { shell }
    }
}

impl WebBackend for Crawl4aiCli {
    fn name(&self) -> &str {
        "crawl4ai-cli"
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
        Box::pin(async { Err("crawl4ai 只做抓取".into()) })
    }

    fn fetch<'a>(&'a self, url: &'a str, t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            let args: Vec<String> =
                ["crwl-placeholder", url, "-o", "markdown"].iter().skip(1).map(|s| s.to_string()).collect();
            let out = self.shell.run("crwl", &args, t).await.map_err(|e| e.to_string())?;
            if !out.ok() {
                return Err(format!("crwl {}", out.brief_error()));
            }
            let text = out.stdout;
            if text.trim().is_empty() {
                return Err("crwl 没有输出正文".into());
            }
            Ok(Page { url: url.into(), title: title_of(&text, url), text })
        })
    }

    fn probe<'a>(&'a self, _t: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if self.shell.have("crwl") {
                Ok("crwl 在 PATH 上".into())
            } else {
                Err("PATH 上没有 crwl（pip install crawl4ai && crawl4ai-setup）".into())
            }
        })
    }
}

// ───────────────────────── SearXNG ─────────────────────────

/// 自建 SearXNG。不要密钥，JSON API 要在实例的 `settings.yml` 里打开
/// （`search.formats` 加上 `json`）—— 探活会把这条报出来。
pub struct Searxng {
    http: reqwest::Client,
    base: String,
}

impl Searxng {
    pub fn new(http: reqwest::Client, base: &str) -> Searxng {
        Searxng { http, base: base.trim_end_matches('/').to_string() }
    }
}

impl WebBackend for Searxng {
    fn name(&self) -> &str {
        "searxng"
    }

    fn can_search(&self) -> bool {
        true
    }

    fn search<'a>(
        &'a self,
        q: &'a str,
        limit: usize,
        t: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let r = guard(
                t,
                self.http
                    .get(format!("{}/search", self.base))
                    .query(&[("q", q), ("format", "json")])
                    .send(),
            )
            .await?;
            let st = r.status();
            let v: Value = r.json().await.map_err(|e| {
                format!("SearXNG 响应不是 JSON（{st}）：{e}。实例的 settings.yml 里要把 json 加进 search.formats")
            })?;
            Ok(v["results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .take(limit)
                .map(|d| Hit {
                    title: d["title"].as_str().unwrap_or("").into(),
                    url: d["url"].as_str().unwrap_or("").into(),
                    snippet: d["content"].as_str().unwrap_or("").into(),
                })
                .filter(|h| !h.url.is_empty())
                .collect())
        })
    }

    fn fetch<'a>(&'a self, _u: &'a str, _t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async { Err("SearXNG 只做搜索".into()) })
    }

    fn probe<'a>(&'a self, t: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match self.search("premortem probe", 1, t).await {
                Ok(h) => Ok(format!("{} 可用（{} 条结果）", self.base, h.len())),
                Err(e) => Err(e),
            }
        })
    }
}

// ───────────────────────── firecrawl（要密钥） ─────────────────────────

/// firecrawl 云服务。**唯一要密钥的后端**，留着是因为它抓取质量最好。
pub struct Firecrawl {
    http: reqwest::Client,
    key: String,
    base: String,
}

impl Firecrawl {
    pub fn new(http: reqwest::Client, key: impl Into<String>, base: &str) -> Firecrawl {
        Firecrawl { http, key: key.into(), base: base.trim_end_matches('/').to_string() }
    }

    async fn post(&self, path: &str, body: Value, t: &CancellationToken) -> Result<Value, String> {
        let r = guard(
            t,
            self.http
                .post(format!("{}{}", self.base, path))
                // 密钥走 header —— 不进 URL、不进命令行参数
                .bearer_auth(&self.key)
                .json(&body)
                .send(),
        )
        .await?;
        let st = r.status();
        let v: Value = r.json().await.map_err(|e| format!("响应不是 JSON（{st}）：{e}"))?;
        if v["success"].as_bool() == Some(false) {
            return Err(format!("firecrawl 拒绝了请求：{}", v["error"].as_str().unwrap_or("未知")));
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
        q: &'a str,
        limit: usize,
        t: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let v = self.post("/v1/search", serde_json::json!({"query": q, "limit": limit}), t).await?;
            Ok(v["data"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|d| Hit {
                    title: d["title"].as_str().unwrap_or("").into(),
                    url: d["url"].as_str().unwrap_or("").into(),
                    snippet: d["description"].as_str().unwrap_or("").into(),
                })
                .filter(|h| !h.url.is_empty())
                .collect())
        })
    }

    fn fetch<'a>(&'a self, url: &'a str, t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            let v = self
                .post("/v1/scrape", serde_json::json!({"url": url, "formats": ["markdown"]}), t)
                .await?;
            let text = v["data"]["markdown"].as_str().unwrap_or("").to_string();
            if text.trim().is_empty() {
                return Err("抓回来是空的（页面可能需要登录或全是脚本渲染）".into());
            }
            let title = v["data"]["metadata"]["title"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(String::from)
                .unwrap_or_else(|| url.to_string());
            Ok(Page { url: url.into(), title, text })
        })
    }
}

// ───────────────────────── 裸 HTTP ─────────────────────────

/// 兜底：GET 一下，剥掉标签。**正文抽取很粗**，能用但别指望质量。
pub struct PlainHttp {
    http: reqwest::Client,
}

impl PlainHttp {
    pub fn new(http: reqwest::Client) -> PlainHttp {
        PlainHttp { http }
    }
}

impl WebBackend for PlainHttp {
    fn name(&self) -> &str {
        "http"
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
        Box::pin(async { Err("裸 HTTP 不支持搜索".into()) })
    }

    fn fetch<'a>(&'a self, url: &'a str, t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            let r = guard(t, self.http.get(url).send()).await?;
            let st = r.status();
            let html = r.text().await.map_err(|e| format!("读响应失败（{st}）：{e}"))?;
            if !st.is_success() {
                return Err(format!("{st}：{}", clip(&html, 200)));
            }
            let title = between(&html, "<title", "</title>")
                .and_then(|t| t.split_once('>').map(|(_, v)| v.trim().to_string()))
                .unwrap_or_else(|| url.to_string());
            Ok(Page { url: url.into(), title, text: html_to_text(&html) })
        })
    }
}

/// 剥标签。**明确是个近似**：`script` / `style` 整块丢掉，其余标签去掉，
/// 常见实体还原。要真正的正文抽取就该用 crawl4ai。
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
                    let tag = &lower[i..i + off];
                    if ["<p", "<div", "<br", "<li", "<h", "</p", "</div"]
                        .iter()
                        .any(|t| tag.starts_with(t))
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

// ───────────────────────── 小工具 ─────────────────────────

/// 把一次请求套上取消。用户打断时立刻放弃，不等它跑完。
async fn guard<F>(t: &CancellationToken, fut: F) -> Result<reqwest::Response, String>
where
    F: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    tokio::select! {
        biased;
        _ = t.cancelled() => Err("已取消".into()),
        r = fut => r.map_err(|e| {
            if e.is_connect() {
                format!("连不上（服务没起来？）：{e}")
            } else if e.is_timeout() {
                format!("超时：{e}")
            } else {
                e.to_string()
            }
        }),
    }
}

/// markdown 的第一个标题当页面标题；没有就用网址。
fn title_of(text: &str, url: &str) -> String {
    text.lines()
        .find(|l| l.trim_start().starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| url.to_string())
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let lower = s.to_ascii_lowercase();
    let a = lower.find(open)?;
    let b = lower[a..].find(close)? + a;
    Some(&s[a..b])
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect() }
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
        self.hits.push(Hit { title: title.into(), url: url.into(), snippet: snippet.into() });
        self
    }

    pub fn page(mut self, url: &str, title: &str, text: &str) -> MockWeb {
        self.pages
            .insert(url.to_string(), Page { url: url.into(), title: title.into(), text: text.into() });
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

    fn fetch<'a>(&'a self, url: &'a str, _t: &'a CancellationToken) -> BoxFuture<'a, Result<Page, String>> {
        Box::pin(async move {
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            self.pages.get(url).cloned().ok_or_else(|| format!("mock 里没有 {url}"))
        })
    }
}
