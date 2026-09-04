//! 工具的权限闸门与体量上限。**每个工具的每次外部接触都要过这里。**
//!
//! # 三件事，缺一件这套工具链就不能给模型用
//!
//! 1. **路径**：只读、且只能读授权目录内的东西。`..`、符号链接、绝对路径都得挡住。
//! 2. **网址**：只走 http(s)、按域名白名单、且**挡住内网与回环地址**（SSRF）——
//!    一个「帮我抓一下这个网址」的工具是天然的内网探测器。
//! 3. **体量**：工具返回值直接进 prompt。一个 3MB 的 HTML 页面能把一轮对话撑爆，
//!    而症状是「模型忽然变笨了」，极难联想到是某次抓取。
//!
//! # 拒绝要说清理由，而且要还给模型
//!
//! [`Denied`] 的文本会作为工具结果回给模型，不是抛异常、不是弹给用户。
//! 模型看到「这个目录不在授权范围内」才知道换个做法；只回一句 `error` 它只会重试。
//! 这跟 `ToolResultKind::Failed` 的处理原则是同一条。
//!
//! # 不给模型裸 shell
//!
//! 这一整套的前提是模型只能通过**语义明确的工具**接触外部。开一个 `bash` 工具
//! 等于把上面三条全部作废 —— 有了它，路径白名单、域名白名单、体量上限一个都不成立。
//! 外部命令只在 [`crate::shell`] 里以 argv 数组的形式跑，模型碰不到。

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 落在 `config.json` 里的那份。用户改得动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCfg {
    /// 总开关。关掉之后 web 类工具直接不注册。
    pub net: bool,
    /// 允许抓取的域名，**后缀匹配**（`arxiv.org` 命中 `www.arxiv.org`）。
    /// 单独一个 `"*"` 表示不限制 —— 默认不是这个。
    pub allow_hosts: Vec<String>,
    /// 黑名单优先于白名单。
    pub deny_hosts: Vec<String>,
    /// 可读的目录。相对路径按**第一个 root** 解析。
    pub roots: Vec<String>,
    /// 路径里出现这些片段就拒绝。挡的是密钥与版本库内部。
    pub deny_names: Vec<String>,
    /// 单次工具返回给模型的字节上限。
    pub max_bytes: usize,
    /// 单次返回的行数上限。
    pub max_lines: usize,
    /// 单行长度上限。压缩过的 JS 一行能有几百 KB。
    pub max_line_len: usize,
    /// 检索结果条数上限。
    pub max_matches: usize,
    /// 文件树条目上限。
    pub max_entries: usize,
    /// 遍历深度上限。
    pub max_depth: usize,
    /// 单个文件读取的字节上限（超了要么截断要么拒绝，由工具决定）。
    pub max_file_bytes: u64,
    /// 允许启动的外部二进制。**只按文件名匹配，不接受路径。**
    pub exec_allow: Vec<String>,
}

impl Default for PolicyCfg {
    fn default() -> Self {
        PolicyCfg {
            net: true,
            // 默认给的是这个项目真正会用到的几个学术站点。
            // 空着或写 "*" 都是用户自己的选择，但默认不该是「随便抓」。
            allow_hosts: vec![
                "arxiv.org".into(),
                "openreview.net".into(),
                "github.com".into(),
                "raw.githubusercontent.com".into(),
                "huggingface.co".into(),
                "paperswithcode.com".into(),
            ],
            deny_hosts: vec![],
            roots: vec![".".into()],
            deny_names: vec![
                ".git".into(),
                ".env".into(),
                "secrets.json".into(),
                "id_rsa".into(),
                ".ssh".into(),
                ".aws".into(),
                "node_modules".into(),
                "target".into(),
                ".venv".into(),
                "__pycache__".into(),
            ],
            max_bytes: 24_000,
            max_lines: 400,
            max_line_len: 400,
            max_matches: 80,
            max_entries: 300,
            max_depth: 8,
            max_file_bytes: 8 * 1024 * 1024,
            // crwl 是 crawl4ai 的命令行（本地、免密钥的抓取路径之一）
            exec_allow: vec!["rg".into(), "curl".into(), "crwl".into()],
        }
    }
}

/// 一次拒绝。**文本会原样回给模型**，所以它要说清「为什么」和「那该怎么办」。
#[derive(Debug, Clone)]
pub struct Denied(pub String);

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 解析好的运行时闸门。`roots` 已经 canonical 化。
pub struct Policy {
    pub cfg: PolicyCfg,
    roots: Vec<PathBuf>,
    denied: AtomicU64,
    clipped: AtomicU64,
}

impl Policy {
    /// `base` 是配置目录，`cfg.roots` 里的相对路径按它解析。
    pub fn new(cfg: PolicyCfg, base: &Path) -> Policy {
        let roots: Vec<PathBuf> = cfg
            .roots
            .iter()
            .map(|r| {
                let p = Path::new(r);
                if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
            })
            // 解析不了的 root 直接丢掉：留着它等于留一个永远匹配不上的规则，
            // 症状是「所有读取都被拒」，而用户会以为是自己路径写错了。
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        Policy { cfg, roots, denied: AtomicU64::new(0), clipped: AtomicU64::new(0) }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn denied_count(&self) -> u64 {
        self.denied.load(Ordering::Relaxed)
    }

    pub fn clipped_count(&self) -> u64 {
        self.clipped.load(Ordering::Relaxed)
    }

    fn deny(&self, msg: impl Into<String>) -> Denied {
        self.denied.fetch_add(1, Ordering::Relaxed);
        Denied(msg.into())
    }

    /// 路径闸门。返回 canonical 化之后的真实路径。
    ///
    /// # 为什么一定要 canonicalize
    ///
    /// 光做字符串前缀比对挡不住两样东西：`a/../../etc/passwd` 这种回溯，
    /// 以及**指到授权目录外面的符号链接**。`canonicalize` 两个都解决 ——
    /// 它走一遍真实文件系统，回来的路径里既没有 `..` 也没有链接。
    ///
    /// 文件还不存在时（要写进 workspace 的情况）退一步 canonical 化它的父目录。
    pub fn check_path(&self, p: &Path) -> Result<PathBuf, Denied> {
        if self.roots.is_empty() {
            return Err(self.deny("没有配置任何可读目录（config.json 的 tools.roots）"));
        }
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.roots[0].join(p)
        };
        let real = match joined.canonicalize() {
            Ok(r) => r,
            Err(_) => {
                // 目标还不存在 ⇒ 用父目录定位，文件名原样接回去
                let parent = joined.parent().ok_or_else(|| self.deny("路径没有父目录"))?;
                let name = joined
                    .file_name()
                    .ok_or_else(|| self.deny("路径没有文件名"))?
                    .to_os_string();
                let base = parent
                    .canonicalize()
                    .map_err(|_| self.deny(format!("路径不存在：{}", p.display())))?;
                base.join(name)
            }
        };
        if !self.roots.iter().any(|r| real.starts_with(r)) {
            return Err(self.deny(format!(
                "{} 不在授权目录内。可读的是：{}",
                p.display(),
                self.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(" / ")
            )));
        }
        // 片段黑名单在 canonical 之后查：查之前的话，一个指向 .ssh 的软链接就绕过去了。
        for c in real.components() {
            if let Component::Normal(seg) = c {
                let seg = seg.to_string_lossy();
                if self.cfg.deny_names.iter().any(|d| seg.as_ref() == d.as_str()) {
                    return Err(self.deny(format!("路径里含受保护的名字「{seg}」，拒绝访问")));
                }
            }
        }
        Ok(real)
    }

    /// 网址闸门。返回规范化后的 URL。
    pub fn check_url(&self, raw: &str) -> Result<String, Denied> {
        if !self.cfg.net {
            return Err(self.deny("联网已关闭（config.json 的 tools.net）"));
        }
        let raw = raw.trim();
        let rest = match raw.split_once("://") {
            Some(("http", r)) | Some(("https", r)) => r,
            Some((scheme, _)) => {
                return Err(self.deny(format!("只支持 http/https，收到 {scheme}")));
            }
            None => return Err(self.deny("网址要带 http:// 或 https://")),
        };
        let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
        // 用户名密码形式的 authority（user@host）是经典的绕过写法，直接不收
        if hostport.contains('@') {
            return Err(self.deny("网址里不接受 user@host 形式"));
        }
        let host = hostport.split(':').next().unwrap_or("").to_ascii_lowercase();
        if host.is_empty() {
            return Err(self.deny("网址里没有主机名"));
        }
        if is_internal(&host) {
            return Err(self.deny(format!("{host} 指向本机或内网，拒绝访问")));
        }
        if self.cfg.deny_hosts.iter().any(|d| host_matches(&host, d)) {
            return Err(self.deny(format!("{host} 在黑名单里")));
        }
        let allowed = self.cfg.allow_hosts.iter().any(|a| a == "*" || host_matches(&host, a));
        if !allowed {
            return Err(self.deny(format!(
                "{host} 不在允许抓取的域名里。当前允许：{}（在 config.json 的 tools.allow_hosts 里加）",
                self.cfg.allow_hosts.join(" / ")
            )));
        }
        Ok(raw.to_string())
    }

    pub fn clip(&self) -> Clip {
        Clip {
            max_bytes: self.cfg.max_bytes,
            max_lines: self.cfg.max_lines,
            max_line_len: self.cfg.max_line_len,
        }
    }

    /// 记一次截断。给指标用 —— 截断率高说明上限设小了或者模型在乱抓。
    pub fn note_clipped(&self) {
        self.clipped.fetch_add(1, Ordering::Relaxed);
    }
}

/// 后缀匹配：`arxiv.org` 命中 `arxiv.org` 与 `www.arxiv.org`，
/// 但**不**命中 `evilarxiv.org` —— 必须卡在点上，否则白名单形同虚设。
fn host_matches(host: &str, pat: &str) -> bool {
    let pat = pat.trim_start_matches('.').to_ascii_lowercase();
    host == pat || host.ends_with(&format!(".{pat}"))
}

/// 本机 / 内网 / 链路本地。这是 SSRF 的主要防线。
///
/// 只按字面判断，挡不住「域名解析到内网 IP」那一类（要挡得在连接时拿到对端地址
/// 再判，得由具体的 HTTP 客户端配合）。**先挡住 90% 的直白写法，剩下的记在这里。**
fn is_internal(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    if host == "::1" || host == "[::1]" || host.starts_with("[fd") || host.starts_with("[fe80") {
        return true;
    }
    // 不带点的名字基本都是内网主机名 / 容器名
    if !host.contains('.') {
        return true;
    }
    let o: Vec<u8> = host.split('.').filter_map(|s| s.parse::<u8>().ok()).collect();
    if o.len() != 4 || host.split('.').count() != 4 {
        return false; // 不是 IPv4 字面量
    }
    match (o[0], o[1]) {
        (127, _) | (10, _) | (0, _) => true,
        (169, 254) => true,
        (192, 168) => true,
        (172, b) if (16..=31).contains(&b) => true,
        _ => false,
    }
}

/// 体量裁剪。**每个工具的返回值都要过它。**
#[derive(Debug, Clone, Copy)]
pub struct Clip {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub max_line_len: usize,
}

/// 裁剪结果。`truncated` 为真时**必须**把 [`Clipped::footer`] 拼上去。
#[derive(Debug, Clone)]
pub struct Clipped {
    pub text: String,
    pub lines_shown: usize,
    pub lines_total: usize,
    pub bytes_total: usize,
    pub truncated: bool,
}

impl Clip {
    pub fn apply(&self, text: &str) -> Clipped {
        let bytes_total = text.len();
        let mut lines_total = 0usize;
        let mut out = String::new();
        let mut shown = 0usize;
        let mut truncated = false;

        for line in text.lines() {
            lines_total += 1;
            if shown >= self.max_lines || out.len() >= self.max_bytes {
                truncated = true;
                continue; // 继续数总行数，才能在页脚说清「一共多少行」
            }
            let (l, cut) = cut_chars(line, self.max_line_len);
            truncated |= cut;
            out.push_str(&l);
            if cut {
                out.push_str(" …[本行过长已截断]");
            }
            out.push('\n');
            shown += 1;
        }
        if out.len() > self.max_bytes {
            let (t, _) = cut_bytes(&out, self.max_bytes);
            out = t;
            truncated = true;
        }
        Clipped { text: out, lines_shown: shown, lines_total, bytes_total, truncated }
    }
}

impl Clipped {
    /// 页脚。**一定要给续读的办法** —— 只说「已截断」会让模型卡住，
    /// 它既不知道漏了多少，也不知道下一步该怎么拿。
    pub fn footer(&self, how_to_continue: &str) -> String {
        if !self.truncated {
            return String::new();
        }
        format!(
            "\n[已截断：显示 {} / 共 {} 行，原文 {} 字节。{}]",
            self.lines_shown,
            self.lines_total,
            self.bytes_total,
            how_to_continue
        )
    }

    /// 正文 + 页脚。
    pub fn render(&self, how_to_continue: &str) -> String {
        format!("{}{}", self.text.trim_end(), self.footer(how_to_continue))
    }
}

/// 按**字符**截断，不会切碎多字节字符。
fn cut_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

/// 按字节截断，退到最近的字符边界。
fn cut_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}
