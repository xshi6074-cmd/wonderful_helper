//! 真实模型客户端：Anthropic Messages 与 OpenAI 兼容的 `/chat/completions`。
//!
//! # 判断段用「强制工具调用」拿结构化输出
//!
//! 判断段要的是 `{scene, rationale, ops}`，不是一段散文。让模型「输出 JSON」再去
//! 解析，失败率高得离谱（前后包一层 ```、多一句解释、少一个引号）。两家 API 都支持
//! **强制调用某个工具**，那条路给的是schema 校验过的参数对象 —— 这是目前最可靠的
//! 结构化输出手段。
//!
//! 单条 op 解析失败**只丢那一条**，不让整次判断失败。判断段跪了这一轮就没有场景、
//! 没有推断更新；为了一条格式不对的 op 赔上整轮，代价不成比例。
//!
//! # 流式自己解 SSE
//!
//! 十几行的事：按空行切块、认 `event:` 与 `data:`。为它引一个 crate 不划算，
//! 而且两家的事件语义差别大到任何通用封装都得再包一层。
//!
//! # 消息映射上有个必须处理的坑
//!
//! Anthropic 要求 user / assistant **严格交替**，而一轮里连着几个工具返回，
//! 映射过去就是连着几条 user。不合并的话 API 直接 400，而错误信息只说
//! "messages: roles must alternate"，第一次遇到会查很久。所以有一遍合并。

use crate::caps::{Cap, Caps, Outcome, Report, Step, Think};
use crate::config::{Api, ModelCfg, ProviderCfg};
use crate::model::{
    AnswerReq, BoxFuture, BoxStream, Call, JudgeOut, JudgeReq, Message, ModelClient, ModelError,
    MsgRole, StreamEvent, ToolSpec, Usage,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

/// 判断段那个被强制调用的工具名。
const JUDGE_TOOL: &str = "record_judgement";

pub struct HttpClient {
    http: reqwest::Client,
    api: Api,
    base: String,
    key: String,
    model: String,
    provider: String,
    role: &'static str,
    temperature: Option<f32>,
    max_tokens: u32,
    /// 当前这套发法。协商成功会就地改它，并通过 `sink` 报给上层落盘。
    caps: std::sync::Mutex<Caps>,
    /// 协商出新发法时往这里发一条 `(provider, caps)`，由 server 写进 config.json。
    /// `None` = 不落盘（测试、或者上层不关心）。
    sink: Option<CapsSink>,
}

/// 协商结果的出口。用通道而不是回调：写 config.json 是 server 的事，
/// 客户端不该知道配置文件在哪，也不该在请求路径上做磁盘 IO。
pub type CapsSink = tokio::sync::mpsc::UnboundedSender<(String, Caps)>;

impl HttpClient {
    pub fn new(p: &ProviderCfg, m: &ModelCfg, key: String) -> Result<HttpClient, String> {
        Self::with_sink(p, m, key, "", None)
    }

    /// 带上「我是哪个 provider / 哪个角色」和协商出口。
    ///
    /// 角色只用在失败报告里 —— 判断段挂了和 subagent 挂了，用户要做的事不一样。
    pub fn with_sink(
        p: &ProviderCfg,
        m: &ModelCfg,
        key: String,
        role: &'static str,
        sink: Option<CapsSink>,
    ) -> Result<HttpClient, String> {
        let http = reqwest::Client::builder()
            // 只给连接阶段设超时。整体超时会在长回答中途把流掐断，
            // 而那正是最不该掐的时刻 —— 用户已经看到半截正文了。
            .connect_timeout(Duration::from_secs(20))
            .user_agent("premortem/0.1")
            .build()
            .map_err(|e| format!("建 HTTP 客户端失败：{e}"))?;
        Ok(HttpClient {
            http,
            api: p.api,
            base: p.base_url.trim_end_matches('/').to_string(),
            key,
            model: m.model.clone(),
            provider: m.provider.clone(),
            role,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            // 盘上有实测结果就用它；没有就从最严的一档开始。
            caps: std::sync::Mutex::new(p.caps.clone().unwrap_or_default()),
            sink,
        })
    }

    fn caps(&self) -> Caps {
        self.caps.lock().unwrap().clone()
    }

    /// 记下新的发法并报给上层落盘。
    fn adopt(&self, c: Caps) {
        *self.caps.lock().unwrap() = c.clone();
        if let Some(s) = &self.sink {
            let _ = s.send((self.provider.clone(), c));
        }
    }

    fn endpoint(&self) -> String {
        match self.api {
            Api::Anthropic => format!("{}/v1/messages", self.base),
            Api::OpenAiCompat => format!("{}/chat/completions", self.base),
        }
    }

    fn auth(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.api {
            Api::Anthropic => r
                .header("x-api-key", &self.key)
                .header("anthropic-version", "2023-06-01"),
            Api::OpenAiCompat => r.bearer_auth(&self.key),
        }
    }

    async fn send(&self, body: Value) -> Result<reqwest::Response, ModelError> {
        let r = self
            .auth(self.http.post(self.endpoint()))
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Call(format!("请求发不出去：{e}")))?;
        let status = r.status();
        if status.is_success() {
            return Ok(r);
        }
        // **一定要把响应体带上。** 401/404/429 的区别全在这里，只报状态码
        // 等于让人去猜是密钥错了、模型名错了，还是限流了。
        let text = r.text().await.unwrap_or_default();
        Err(ModelError::Call(format!(
            "{} {}：{}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            clip(&text, 600)
        )))
    }
}

impl ModelClient for HttpClient {
    fn judge<'a>(&'a self, req: JudgeReq) -> BoxFuture<'a, Result<JudgeOut, ModelError>> {
        Box::pin(async move {
            // ── 调用方式协商 ──
            //
            // 正常路径就是一次调用：按当前这套发法发出去。被拒了才换发法重试。
            // **降级的是「怎么发」，不是「要什么」** —— 判断段要的始终是一个
            // schema 校验过的场景判定。一路试到底还拿不到，就带着完整阶梯报错，
            // 不假装判成 none：悄悄降级的代价是用户以为场景判定在工作，
            // 实际每轮都是空的，而这件事在界面上完全看不出来。
            let mut caps = self.caps();
            let start = caps.clone();
            let mut steps: Vec<Step> = Vec::new();
            // 阶梯最多这么长。每一档只走一次，这里是防呆上限。
            for _ in 0..8 {
                let body = self.judge_body(&req.msgs, &caps);
                let sent = tokio::select! {
                    biased;
                    _ = req.token.cancelled() => return Err(ModelError::Call("已取消".into())),
                    r = self.send(body) => r,
                };
                let err = match sent {
                    Ok(resp) => {
                        let v: Value = resp.json().await.map_err(|e| {
                            ModelError::Call(format!("判断段响应不是 JSON：{e}"))
                        })?;
                        return match parse_judge(self.api, &v) {
                            Ok(out) => {
                                if caps != start {
                                    steps.push(Step {
                                        cap: Cap::ForcedTool,
                                        tried: "这套发法成了，记进 config.json".into(),
                                        outcome: Outcome::Ok,
                                        detail: String::new(),
                                    });
                                    self.adopt(caps);
                                }
                                Ok(out)
                            }
                            // 请求本身过了，但没拿到工具调用 —— 多半是 thinking 开着
                            // 把强制降级成了 auto，模型于是只写了一段思考。
                            // 这也算一次「被拒」，继续往下试。
                            Err(e) => {
                                let msg = e.to_string();
                                match caps.next(&msg) {
                                    Some(s) => {
                                        steps.push(s);
                                        continue;
                                    }
                                    None => Err(self.caps_error(steps, msg)),
                                }
                            }
                        };
                    }
                    Err(e) => e.to_string(),
                };
                match caps.next(&err) {
                    Some(s) => steps.push(s),
                    None => return Err(self.caps_error(steps, err)),
                }
            }
            Err(self.caps_error(steps, "试完了所有发法".into()))
        })
    }

    fn stream<'a>(&'a self, req: AnswerReq) -> BoxFuture<'a, Result<BoxStream, ModelError>> {
        Box::pin(async move {
            let body = self.answer_body(&req.msgs, &req.tools);
            let resp = tokio::select! {
                biased;
                _ = req.token.cancelled() => return Err(ModelError::Call("已取消".into())),
                r = self.send(body) => r?,
            };
            Ok(sse_to_events(self.api, resp, req.token))
        })
    }
}

// ───────────────────────── 请求体 ─────────────────────────

impl HttpClient {
    fn judge_body(&self, msgs: &[Message], caps: &Caps) -> Value {
        let tool = judge_tool_spec();
        let anthropic = self.api == Api::Anthropic;
        let mut b = match self.api {
            Api::Anthropic => {
                let (system, messages) = anthropic_messages(msgs);
                json!({
                    "model": self.model,
                    "system": system,
                    "messages": messages,
                    "tools": [anthropic_tool(&tool)],
                })
            }
            Api::OpenAiCompat => json!({
                "model": self.model,
                "messages": openai_messages(msgs),
                "tools": [openai_tool(&tool)],
            }),
        };
        caps.apply(&mut b, anthropic, self.temperature, self.max_tokens, Think::OffForTools);
        if let Some(tc) = caps.tool_choice(anthropic, JUDGE_TOOL) {
            b["tool_choice"] = tc;
        }
        b
    }

    /// 阶梯走到底：带上完整记录，说清这个角色需要什么、试了什么、厂商怎么说的。
    fn caps_error(&self, mut steps: Vec<Step>, last: String) -> ModelError {
        steps.push(Step {
            cap: Cap::ForcedTool,
            tried: "没有别的发法了".into(),
            outcome: Outcome::Exhausted,
            detail: last,
        });
        ModelError::Caps(Box::new(Report {
            provider: self.provider.clone(),
            model: self.model.clone(),
            role: if self.role.is_empty() { "判断段".into() } else { self.role.into() },
            required: vec![Cap::ForcedTool, Cap::ThinkingOff, Cap::SystemRole],
            steps,
            verdict: format!(
                "{} 上的 {} 给不出「强制工具调用」，当不了判断段。\
                 换一个非 thinking 的型号，或者把这个角色换到别家。",
                self.provider, self.model
            ),
        }))
    }

    /// 回答段。**这里不协商** —— 它是流式的，试错要把已经吐出来的正文丢掉，
    /// 而判断段每轮都先跑一次，协商在那边做完了，这边直接用结果。
    fn answer_body(&self, msgs: &[Message], tools: &[ToolSpec]) -> Value {
        let caps = self.caps();
        let anthropic = self.api == Api::Anthropic;
        let mut b = match self.api {
            Api::Anthropic => {
                let (system, messages) = anthropic_messages(msgs);
                json!({
                    "model": self.model,
                    "system": system,
                    "messages": messages,
                    "stream": true,
                })
            }
            Api::OpenAiCompat => json!({
                "model": self.model,
                "messages": openai_messages(msgs),
                "stream": true,
            }),
        };
        // 回答段不强制工具调用，所以让它好好想。
        caps.apply(&mut b, anthropic, self.temperature, self.max_tokens, Think::On);
        if !caps.stream_usage {
            // 拿不到就只能估算
        } else if !anthropic {
            b["stream_options"] = json!({ "include_usage": true });
        }
        if !tools.is_empty() {
            b["tools"] = Value::Array(
                tools
                    .iter()
                    .map(if anthropic { anthropic_tool } else { openai_tool })
                    .collect(),
            );
        }
        b
    }
}

/// 判断段的工具。**只判场景，不写推断。**
///
/// 原来这里还有一个 `ops` 字段，schema 是 `{"type":"object"}` —— 没有任何字段
/// 说明，也没有任何 prompt 告诉过模型 op 长什么样。于是真实模型永远给不出
/// 合法的 op，指标上是 `inferred_ops: 0`，而这看起来像「模型不爱写推断」。
/// 写推断挪到了回答段的两个工具（[`crate::actions`]）：那才是模型读完材料、
/// 真正形成判断的时刻。
fn judge_tool_spec() -> ToolSpec {
    ToolSpec {
        name: JUDGE_TOOL.into(),
        description: "记录这一轮的场景判定。必须调用一次，且只调这一个。".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "scenes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "命中的场景 id，来自场景目录。可以多个 ——                                     「目标还没说清」和「预算和方案对不上」经常同时成立。                                    最多 3 个，按重要性排序。一个都不匹配就填 [\"none\"]"
                },
                "rationale": { "type": "string", "description": "为什么判成这些场景，一两句" }
            },
            "required": ["scenes", "rationale"]
        }),
    }
}

fn anthropic_tool(t: &ToolSpec) -> Value {
    json!({ "name": t.name, "description": t.description, "input_schema": t.schema })
}

fn openai_tool(t: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": { "name": t.name, "description": t.description, "parameters": t.schema }
    })
}

// ───────────────────────── 消息映射 ─────────────────────────

/// → `(system 段, messages)`。
///
/// system 消息全部并成一段：Anthropic 的 system 是顶层字段，不在 messages 里。
pub fn anthropic_messages(msgs: &[Message]) -> (String, Vec<Value>) {
    let system = msgs
        .iter()
        .filter(|m| m.role == MsgRole::System)
        .map(|m| m.content.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut out: Vec<Value> = Vec::new();
    for m in msgs.iter().filter(|m| m.role != MsgRole::System) {
        let (role, blocks) = match m.role {
            MsgRole::User => ("user", vec![text_block(&m.content)]),
            MsgRole::Assistant => {
                let mut b = vec![];
                if !m.content.trim().is_empty() {
                    b.push(text_block(&m.content));
                }
                for c in &m.tool_calls {
                    b.push(json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": c.args
                    }));
                }
                // 空 content 会被 400 掉。宁可放一个占位符也不要发空数组。
                if b.is_empty() {
                    b.push(text_block("(无输出)"));
                }
                ("assistant", b)
            }
            // 工具返回在 Anthropic 里是 user 角色的一个块
            MsgRole::Tool => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": clip(&m.content, 200_000),
                })],
            ),
            MsgRole::System => unreachable!(),
        };
        let blocks: Vec<Value> = blocks.into_iter().filter(|b| !is_empty_text(b)).collect();
        if blocks.is_empty() {
            continue;
        }
        // ★ 合并相邻同角色：连着几条工具返回映射过来就是连着几条 user，
        //   而 Anthropic 要求严格交替，不合并会 400。
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                }
            }
            _ => out.push(json!({ "role": role, "content": blocks })),
        }
    }
    // 第一条必须是 user
    if out.first().map(|m| m["role"] == "assistant").unwrap_or(false) {
        out.insert(0, json!({ "role": "user", "content": [text_block("(继续)")] }));
    }
    (system, out)
}

fn text_block(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

fn is_empty_text(b: &Value) -> bool {
    b["type"] == "text" && b["text"].as_str().unwrap_or("").trim().is_empty()
}

/// → OpenAI 风格的 messages。它允许连续同角色，所以不用合并。
pub fn openai_messages(msgs: &[Message]) -> Vec<Value> {
    msgs.iter()
        .filter_map(|m| match m.role {
            MsgRole::System => Some(json!({ "role": "system", "content": m.content })),
            MsgRole::User => Some(json!({ "role": "user", "content": m.content })),
            MsgRole::Assistant => {
                // 兼容修复旧会话：历史版本可能落过 Wrote{text:""}。没有正文也没有
                // tool_calls 的 assistant 对协议没有任何语义，只会让 Moonshot 400。
                if m.content.trim().is_empty() && m.tool_calls.is_empty() {
                    return None;
                }
                let mut v = json!({ "role": "assistant", "content": m.content });
                if !m.tool_calls.is_empty() {
                    v["tool_calls"] = Value::Array(
                        m.tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    // OpenAI 的 arguments 是**字符串**不是对象
                                    "function": { "name": c.name, "arguments": c.args.to_string() }
                                })
                            })
                            .collect(),
                    );
                }
                Some(v)
            }
            MsgRole::Tool => Some(json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": clip(&m.content, 200_000),
            })),
        })
        .collect()
}

// ───────────────────────── 判断段解析 ─────────────────────────

fn parse_judge(api: Api, v: &Value) -> Result<JudgeOut, ModelError> {
    let (input, usage) = match api {
        Api::Anthropic => {
            let input = v["content"]
                .as_array()
                .and_then(|a| a.iter().find(|b| b["type"] == "tool_use"))
                .map(|b| b["input"].clone())
                .ok_or_else(|| {
                    ModelError::Schema(format!("响应里没有 tool_use 块：{}", clip(&v.to_string(), 400)))
                })?;
            let u = &v["usage"];
            (
                input,
                Usage {
                    prompt: u["input_tokens"].as_u64().unwrap_or(0) as u32,
                    completion: u["output_tokens"].as_u64().unwrap_or(0) as u32,
                    estimated: false,
                },
            )
        }
        Api::OpenAiCompat => {
            let raw = v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .ok_or_else(|| {
                    ModelError::Schema(format!(
                        "响应里没有 tool_calls：{}",
                        clip(&v.to_string(), 400)
                    ))
                })?;
            let input: Value = serde_json::from_str(raw)
                .map_err(|e| ModelError::Schema(format!("工具参数不是 JSON：{e}")))?;
            let u = &v["usage"];
            (
                input,
                Usage {
                    prompt: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                    completion: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
                    estimated: false,
                },
            )
        }
    };

    // 收数组，也收单个字符串 —— 模型偶尔会不看 schema 直接给一个 id，
    // 为此判整轮失败不划算。
    let scenes: Vec<String> = match &input["scenes"] {
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Value::String(s) => vec![s.clone()],
        _ => match input["scene"].as_str() {
            Some(s) => vec![s.to_string()],
            None => vec!["none".to_string()],
        },
    };
    let rationale = input["rationale"].as_str().unwrap_or("").to_string();
    Ok(JudgeOut { scenes, rationale, usage })
}

// ───────────────────────── SSE ─────────────────────────

/// 把 HTTP 响应流转成 [`StreamEvent`]。
fn sse_to_events(
    api: Api,
    resp: reqwest::Response,
    token: tokio_util::sync::CancellationToken,
) -> BoxStream {
    let bytes = resp.bytes_stream();
    let st = async_stream::stream! {
        let mut acc = Acc::default();
        let mut buf = String::new();
        let mut bytes = Box::pin(bytes);
        loop {
            let chunk = tokio::select! {
                biased;
                _ = token.cancelled() => break,
                c = bytes.next() => c,
            };
            let Some(chunk) = chunk else { break };
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => { yield StreamEvent::Failed(format!("流中断：{e}")); return; }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE 以空行分块。半块留在缓冲里等下一次。
            while let Some(pos) = buf.find("\n\n") {
                let block = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for ev in acc.feed(api, &block) {
                    yield ev;
                }
            }
        }
        for ev in acc.finish() {
            yield ev;
        }
    };
    Box::pin(st)
}

/// 流式过程中攒起来的东西：工具调用是**分片到达**的，参数 JSON 得拼完整才能解析。
#[derive(Default)]
struct Acc {
    /// index → (id, name, 拼到一半的参数 JSON)
    calls: Vec<(String, String, String)>,
    usage: Usage,
    emitted_calls: bool,
}

impl Acc {
    fn feed(&mut self, api: Api, block: &str) -> Vec<StreamEvent> {
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            return vec![];
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else { return vec![] };
        match api {
            Api::Anthropic => self.feed_anthropic(&v),
            Api::OpenAiCompat => self.feed_openai(&v),
        }
    }

    fn feed_anthropic(&mut self, v: &Value) -> Vec<StreamEvent> {
        let mut out = vec![];
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.usage.prompt =
                    v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32;
            }
            "content_block_start" => {
                let b = &v["content_block"];
                if b["type"] == "tool_use" {
                    self.calls.push((
                        b["id"].as_str().unwrap_or("").to_string(),
                        b["name"].as_str().unwrap_or("").to_string(),
                        String::new(),
                    ));
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                if let Some(t) = d["text"].as_str() {
                    out.push(StreamEvent::Chunk(t.to_string()));
                } else if let Some(j) = d["partial_json"].as_str() {
                    if let Some(last) = self.calls.last_mut() {
                        last.2.push_str(j);
                    }
                }
            }
            "message_delta" => {
                self.usage.completion =
                    v["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;
            }
            "error" => {
                out.push(StreamEvent::Failed(format!(
                    "服务端错误：{}",
                    clip(&v["error"].to_string(), 300)
                )));
            }
            _ => {}
        }
        out
    }

    fn feed_openai(&mut self, v: &Value) -> Vec<StreamEvent> {
        let mut out = vec![];
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage.prompt = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
            self.usage.completion = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
        }
        let d = &v["choices"][0]["delta"];
        if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
            out.push(StreamEvent::Chunk(t.to_string()));
        }
        if let Some(tcs) = d["tool_calls"].as_array() {
            for tc in tcs {
                let i = tc["index"].as_u64().unwrap_or(0) as usize;
                while self.calls.len() <= i {
                    self.calls.push((String::new(), String::new(), String::new()));
                }
                if let Some(id) = tc["id"].as_str() {
                    self.calls[i].0 = id.to_string();
                }
                if let Some(n) = tc["function"]["name"].as_str() {
                    self.calls[i].1.push_str(n);
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    self.calls[i].2.push_str(a);
                }
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = vec![];
        if !self.emitted_calls && !self.calls.is_empty() {
            self.emitted_calls = true;
            let calls: Vec<Call> = self
                .calls
                .iter()
                .filter(|(_, name, _)| !name.is_empty())
                .map(|(id, name, args)| Call {
                    id: id.clone(),
                    name: name.clone(),
                    // 参数拼不完整（流被打断）时给空对象：工具会报「缺少参数」，
                    // 那条错误回给模型比整轮失败有用。
                    args: serde_json::from_str(args).unwrap_or_else(|_| json!({})),
                })
                .collect();
            if !calls.is_empty() {
                out.push(StreamEvent::ToolCalls(calls));
            }
        }
        out.push(StreamEvent::Done(self.usage));
        out
    }
}

/// 把若干个 SSE 块喂进解析器，拿到事件序列。**离线测这一段用的。**
///
/// 流式解析是「本地全绿、一上线全错」的头号重灾区：分片边界、工具参数拼接、
/// usage 落在哪个事件上，每一条都只在真实响应里才暴露。但它本身是纯函数 ——
/// 把真实响应抄成固定块就能测，没有理由只靠真调用去发现。
pub fn parse_sse(api: Api, blocks: &[&str]) -> Vec<StreamEvent> {
    let mut acc = Acc::default();
    let mut out = vec![];
    for b in blocks {
        out.extend(acc.feed(api, b));
    }
    out.extend(acc.finish());
    out
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!("{}…", s.chars().take(n).collect::<String>())
}
