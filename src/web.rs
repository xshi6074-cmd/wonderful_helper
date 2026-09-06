//! 网页读取。当前公开能力只有进程内的基础 fetch。
//!
//! # 当前路径
//!
//! `web_fetch` 先用 reqwest 获取服务端返回的页面，再由 Readability 风格的 DOM
//! 评分选出正文，最后转成 Markdown。相对链接按最终页面 URL 绝对化，响应体、
//! 重定向和目标域名都受策略限制；不执行 JavaScript，也不注册网页搜索。
//!
//! # 旧配置兼容
//!
//! 早期版本的多后端类型和实现暂时留在本模块，保证旧 `config.json` 与外部调用
//! 能继续反序列化、编译；[`build`] 不再选择它们。等配置迁移窗口过去后可整体删除。

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
    /// 旧配置兼容项，不再由 [`build`] 启用。
    Crawl4ai,
    /// 旧配置兼容项，不再由 [`build`] 启用。
    Crawl4aiCli,
    /// 旧配置兼容项，不再由 [`build`] 启用。
    Firecrawl,
    /// 当前内置抓取。
    Http,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Searcher {
    /// 旧配置兼容项，不再由 [`build`] 启用。
    Searxng,
    /// 旧配置兼容项，不再由 [`build`] 启用。
    Firecrawl,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebCfg {
    /// 旧字段；所有旧值均由联网总开关接管并降级为内置 fetch。
    pub fetch: Fetcher,
    /// 旧字段；当前忽略。
    pub fetch_base: String,
    /// 旧字段；当前忽略且不注册 search。
    pub search: Searcher,
    /// 旧字段；当前忽略。
    pub search_base: String,
}

impl Default for WebCfg {
    fn default() -> Self {
        WebCfg {
            // 默认只用进程内的 Rust 抓取器，没有外部服务和安装步骤。
            fetch: Fetcher::Http,
            fetch_base: String::new(),
            search: Searcher::None,
            search_base: String::new(),
        }
    }
}

/// 搭建当前公开的基础抓取能力。
///
/// `WebCfg` 与 `key` 暂时保留，只为让旧 `config.json` 可以无损升级；其中曾经
/// 暴露的 crawl4ai / SearXNG / Firecrawl 选择不再参与运行。否则 UI 虽然删掉了
/// 选项，旧文件里的一行 `crawl4ai` 仍会让新版本去连一个不存在的本地服务。
/// 返回 `None` 只表示联网总开关关闭。
pub fn build(_cfg: &WebCfg, policy: &crate::policy::PolicyCfg, _key: Option<String>) -> Option<Arc<dyn WebBackend>> {
    if !policy.net {
        return None;
    }
    Some(Arc::new(PlainHttp::new(client(policy), policy.max_file_bytes)))
}

fn client(policy: &crate::policy::PolicyCfg) -> reqwest::Client {
    let redirect_policy = policy.clone();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .user_agent("premortem/0.1 (research assistant)")
        // 初始 URL 会在工具入口过闸；重定向也必须逐跳复查，否则公网网址可以
        // 302 到 localhost，绕过 SSRF 和域名白名单。
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error(std::io::Error::other("重定向超过 10 次"));
            }
            match crate::policy::validate_web_url(&redirect_policy, attempt.url().as_str()) {
                Ok(_) => attempt.follow(),
                Err(e) => attempt.error(std::io::Error::other(format!("拒绝重定向：{e}"))),
            }
        }))
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

// ───────────────────────── 内置网页抓取 ─────────────────────────

/// 单二进制抓取器：HTTP 获取后按 DOM 结构抽正文，再保留格式转成 Markdown。
///
/// 它不执行 JavaScript；动态页面会得到服务端实际返回的内容，而不是浏览器渲染结果。
/// 这个边界换来了 Windows / WSL / macOS 都不需要额外服务或浏览器。
pub struct PlainHttp {
    http: reqwest::Client,
    max_body_bytes: u64,
}

impl PlainHttp {
    pub fn new(http: reqwest::Client, max_body_bytes: u64) -> PlainHttp {
        PlainHttp { http, max_body_bytes }
    }
}

impl WebBackend for PlainHttp {
    fn name(&self) -> &str {
        "builtin-fetch"
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
            let final_url = r.url().to_string();
            let content_type = r
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = read_limited(r, self.max_body_bytes, t).await?;
            let body = decode_body(&bytes, &content_type);
            if !st.is_success() {
                return Err(format!("{st}：{}", clip(&body, 200)));
            }

            if is_html(&content_type, &body) {
                let (title, text) = smart_html_to_markdown(&body, &final_url)?;
                if text.trim().is_empty() {
                    return Err("页面存在，但没有抽取到可读正文（可能依赖 JavaScript 渲染）".into());
                }
                Ok(Page { url: final_url, title, text })
            } else {
                let text = body.trim().to_string();
                if text.is_empty() {
                    return Err("响应正文为空".into());
                }
                Ok(Page { url: final_url.clone(), title: final_url, text })
            }
        })
    }

    fn probe<'a>(&'a self, _t: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Ok("内置抓取可用（HTTP + Readability + Markdown）".into()) })
    }
}

/// 从 HTML 得到主内容 Markdown。
///
/// 这里的 “smart” 不是多写几条 class 名匹配：Readability 会综合段落长度、标点、
/// 链接密度、语义元素和兄弟节点关系给 DOM 子树打分，再清理导航、广告和侧栏。
/// `document_url` 还会把正文中的相对链接和图片地址补成绝对 URL；因此抓到二级页面
/// 入口后，保存下来的 Markdown 可以直接继续 fetch。
pub fn smart_html_to_markdown(html: &str, document_url: &str) -> Result<(String, String), String> {
    let mut cfg = dom_smoothie::Config::default();
    // 字节上限挡不住 `<i></i>` 重复几十万次这种小标签炸弹；DOM 元素数也要封顶。
    cfg.max_elements_to_parse = 150_000;
    let mut readability = dom_smoothie::Readability::new(html, Some(document_url), Some(cfg))
        .map_err(|e| format!("HTML 解析失败：{e}"))?;
    let article = readability.parse().map_err(|e| format!("正文识别失败：{e}"))?;
    let title = article.title.trim().to_string();
    let content = article.content.as_ref();
    let markdown = htmd::convert(content).map_err(|e| format!("Markdown 转换失败：{e}"))?;
    let markdown = tidy_markdown(&markdown);
    let text = if markdown.is_empty() {
        tidy_markdown(article.text_content.as_ref())
    } else {
        markdown
    };
    let title = if title.is_empty() { document_url.to_string() } else { title };
    Ok((title, text))
}

fn tidy_markdown(text: &str) -> String {
    text.trim().trim_start_matches('\u{feff}').trim().to_string()
}

fn is_html(content_type: &str, body: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("text/html") || ct.contains("application/xhtml+xml") {
        return true;
    }
    let start = body.trim_start().to_ascii_lowercase();
    start.starts_with("<!doctype html")
        || start.starts_with("<html")
        || start.starts_with("<head")
        || start.starts_with("<body")
}

fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let charset = content_type.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['\'', '"']))
    });
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

async fn read_limited(
    response: reqwest::Response,
    max_bytes: u64,
    token: &CancellationToken,
) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;

    let max_bytes = max_bytes.max(1);
    if response.content_length().is_some_and(|n| n > max_bytes) {
        return Err(format!("响应体超过上限：大于 {max_bytes} 字节"));
    }
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = token.cancelled() => return Err("已取消".into()),
            next = stream.next() => next,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| format!("读取响应失败：{e}"))?;
        if out.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(format!("响应体超过上限：{max_bytes} 字节"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// 旧版的无 DOM 纯文本转换仍保留为兼容 API；生产 fetch 已不再使用它。
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

#[cfg(test)]
mod extraction_tests {
    use super::{decode_body, smart_html_to_markdown};

    #[test]
    fn extracts_main_dom_preserves_markdown_and_absolutizes_links() {
        let filler =
            "这一段提供足够的正文密度。它讨论实验动机、约束、方法和可以复核的结果，不是菜单或站点导航。"
                .repeat(12);
        let html = format!(r#"
            <!doctype html><html><head><title>站点名 | 真正的文章标题</title></head><body>
              <nav><a href="/pricing">全站导航与价格</a></nav>
              <main><article>
                <h1>真正的文章标题</h1>
                <p>{filler}</p>
                <h2>方法</h2>
                <ul><li><strong>保留粗体</strong></li><li>保留列表</li></ul>
                <blockquote>一段引用</blockquote>
                <pre><code>cargo test --all-targets</code></pre>
                <table><thead><tr><th>字段</th><th>值</th></tr></thead>
                  <tbody><tr><td>seed</td><td>3</td></tr></tbody></table>
                <p><a href="../next?seed=3#result">下一页</a>；
                   <a href="/appendix/a">附录</a></p>
              </article></main>
              <footer><a href="/legal">冗长页脚</a></footer>
            </body></html>
        "#);

        let (title, md) = smart_html_to_markdown(
            &html,
            "https://example.com/docs/chapter/intro.html",
        )
        .expect("extract");
        assert!(title.contains("真正的文章标题"));
        assert!(md.contains("## 方法"));
        assert!(md.contains("**保留粗体**"));
        assert!(md.contains("cargo test --all-targets"));
        assert!(md.contains("seed"));
        assert!(md.contains("[下一页](https://example.com/docs/next?seed=3#result)"));
        assert!(md.contains("[附录](https://example.com/appendix/a)"));
        assert!(!md.contains("全站导航与价格"));
        assert!(!md.contains("冗长页脚"));
    }

    #[test]
    fn respects_declared_non_utf8_charset() {
        let (encoded, _, _) = encoding_rs::GBK.encode("中文正文");
        assert_eq!(decode_body(&encoded, "text/plain; charset=gbk"), "中文正文");
    }
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
