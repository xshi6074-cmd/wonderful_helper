//! Markdown → HTML。**在 Rust 侧渲染，不在浏览器里。**
//!
//! # 为什么不放到前端
//!
//! 两个理由，第二个是决定性的：
//!
//! 1. 前端是零构建步骤的纯 ES module，引一个 markdown 库就得开始考虑打包或 CDN。
//! 2. **模型输出是不可信输入。** 把它当 HTML 塞进 DOM 是标准的 XSS 通道 ——
//!    模型被提示词注入之后写一段 `<img onerror=...>`，或者它抓回来的网页里本来
//!    就有一段，前端一 `innerHTML` 就执行了。
//!
//! 这里的做法是**丢掉 raw HTML 事件**：pulldown-cmark 把 `<script>` 这类原样
//! 透传的片段单独报成 `Event::Html` / `Event::InlineHtml`，我们不转发它们，
//! 于是它们连生成都没生成过。剩下的正文一律走转义。
//! 这不是「过滤干净了」，是**那条路根本没接通** —— 和 `shell` 只走 argv 同一个道理。

use pulldown_cmark::{Event, Options, Parser, html};

/// 渲染成安全的 HTML 片段。
pub fn to_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(md, opts).filter(|e| {
        // ★ 唯一的安全措施，也是唯一需要的：原样透传的 HTML 一律不要。
        !matches!(e, Event::Html(_) | Event::InlineHtml(_))
    });
    let mut out = String::with_capacity(md.len() * 3 / 2);
    html::push_html(&mut out, parser);
    out
}

/// 流式过程中用的轻量版：**不解析 markdown**，只转义。
///
/// 半截的 markdown 渲染出来会不停跳动（一个没闭合的 ``` 会让后面全变代码块）。
/// 流式阶段按纯文本显示、`Wrote` 落定之后再换成渲染结果，看起来稳得多，
/// 也省掉每来一个 delta 就重渲一遍全文的开销。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
