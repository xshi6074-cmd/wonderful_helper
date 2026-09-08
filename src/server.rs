//! Web UI 的后端：axum + 一条 WebSocket。
//!
//! # 架构选择
//!
//! - **前端零构建步骤。** 纯 ES module + CSS，`include_str!` 编进二进制。
//!   没有 node、没有打包器、没有 CDN 依赖 —— `cargo run --bin serve` 就能用。
//!   将来要做桌面版，套一层 webview 指向 localhost 即可，前端一行不用改。
//! - **一条 WebSocket 走全部消息**，不混 REST。配置读写、会话切换、发消息、
//!   编辑推断图全走它，客户端只有一个连接状态要管。
//! - **markdown 在 Rust 侧渲染**（[`crate::markdown`]），前端只做 DOM。
//!   模型输出是不可信输入，在浏览器里 `innerHTML` 它就是 XSS 通道。
//!
//! # 全量推送
//!
//! 每一条 [`UiEvent`] 都原样转成 JSON 发给浏览器，**包括 Appended 的每一种 body**。
//! 先全量再削，比先削再补漏容易得多 —— 现在能看见的东西决定了 UI 能做成什么样，
//! 削早了就只能猜。要削的时候，削的是 [`fanout`] 这一个函数。
//!
//! # 会话切换
//!
//! Core 是**每会话一个**。切换 = 关掉当前的、按目标会话恢复出一个新的。
//! 不做「一个 Core 管多个会话」：那会把仲裁、注入、取消的作用域从「一条时间线」
//! 变成「多条」，是这套设计里最不该模糊的边界。

use crate::config::{Secrets, Settings, real_env};
use crate::core::{CoreDeps, start};
use crate::handle::CoreHandle;
use crate::event::Body;
use crate::ids::{SessionId, TurnId};
use crate::markdown;
use crate::memory::Memory;
use crate::model::Mode;
use crate::msg::{SendMode, UiEvent};
use crate::persist::{restore, spawn_writer};
use crate::state::Op;
use crate::store::{SqliteStore, Store};
use crate::tools::Registry;
use crate::{render, toolkit, web};
use axum::extract::ws::{Message as Ws, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, extract::State};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_util::sync::CancellationToken;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");

pub struct App {
    dir: PathBuf,
    store: Arc<dyn Store>,
    settings: RwLock<Settings>,
    secrets: RwLock<Secrets>,
    live: Mutex<Option<Live>>,
    commands: Mutex<()>,
    /// 已序列化好的 JSON，广播给所有打开的页面。多开一个标签页也能同步看到。
    out: broadcast::Sender<String>,
    /// 客户端协商出新的调用方式时从这里进来，落进 config.json。
    caps_in: tokio::sync::mpsc::UnboundedSender<(String, crate::caps::Caps)>,
}

struct Live {
    session: SessionId,
    handle: CoreHandle,
    core_join: tokio::task::JoinHandle<crate::core::CoreSummary>,
    writer_join: tokio::task::JoinHandle<()>,
    pump: tokio::task::JoinHandle<()>,
    metrics: Arc<toolkit::Metrics>,
    tools: Vec<String>,
    web_name: String,
}

impl App {
    pub async fn new(dir: PathBuf) -> Result<Arc<App>, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败：{e}"))?;
        let dir = dir.canonicalize().unwrap_or(dir);
        let store: Arc<dyn Store> = Arc::new(
            SqliteStore::open(&dir.join("premortem.db")).map_err(|e| format!("打不开库：{e}"))?,
        );
        let settings = Settings::load(&dir, &real_env);
        let (secrets, _) = Secrets::load(&dir);
        let (out, _) = broadcast::channel(4096);
        let (caps_in, mut caps_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Arc::new(App {
            dir,
            store,
            settings: RwLock::new(settings),
            secrets: RwLock::new(secrets),
            live: Mutex::new(None),
            commands: Mutex::new(()),
            out,
            caps_in,
        });

        // 协商结果落盘。**单独一个任务**，因为写文件不能挡在模型请求的路径上 ——
        // 判断段每轮都跑，为了记一次调用方式让它多等一次磁盘 IO 不划算。
        let bg = app.clone();
        tokio::spawn(async move {
            while let Some((provider, caps)) = caps_rx.recv().await {
                let mut s = bg.settings.write().await;
                let Some(p) = s.providers.get_mut(&provider) else { continue };
                if p.caps.as_ref() == Some(&caps) {
                    continue;
                }
                p.caps = Some(caps);
                let snapshot = s.clone();
                drop(s);
                if let Err(e) = snapshot.save(&bg.dir) {
                    eprintln!("[caps] 协商结果写不进 config.json：{e}");
                    continue;
                }
                bg.push(json!({
                    "t": "caps", "provider": provider,
                    "settings": serde_json::to_value(&snapshot).unwrap_or(Value::Null),
                }));
            }
        });
        Ok(app)
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/", get(|| async { no_cache("text/html; charset=utf-8", INDEX) }))
            .route("/app.js", get(|| async { js(APP_JS) }))
            .route("/app.css", get(|| async { css(APP_CSS) }))
            .route("/ws", get(ws_upgrade))
            .with_state(self.clone())
    }

    /// 起一个会话的 Core。`resume` 为 None 表示开新会话。
    async fn open(self: &Arc<Self>, resume: Option<SessionId>) -> Result<(), String> {
        let settings = self.settings.read().await.clone();
        let secrets = self.secrets.read().await.clone();
        let models = settings.build_models_with(&secrets, &real_env, Some(self.caps_in.clone()))?;

        let key = settings.web_key(&secrets, &real_env);
        let backend = web::build(&settings.web, &settings.tools, key);
        let web_name = backend.as_ref().map(|b| b.name().to_string()).unwrap_or("无".into());
        let deps = toolkit::from_settings(&settings, &self.dir, backend)
            .map_err(|e| format!("工具链起不来：{e}"))?;
        let registry = toolkit::register(Registry::new(), &deps);
        let tools = registry.names();

        if let Some(id) = &resume {
            if self.store.get_session(id).map_err(|e| e.to_string())?.is_none() {
                return Err("会话不存在".into());
            }
        }
        let mode = {
            let live = self.live.lock().await;
            match live.as_ref().filter(|l| resume.as_ref() == Some(&l.session)) {
                Some(l) => l.handle.session_snapshot().await.map(|s| s.mode).unwrap_or(Mode::Explore),
                None => Mode::Explore,
            }
        };
        // 配置或目标无效时保留当前 Core；恢复必须等旧 writer 冲完。
        self.close().await;

        let session = resume.clone().unwrap_or_else(SessionId::new);
        if resume.is_none() {
            // 根会话以前从来没往 session 表里写过行 —— 只有分叉才写。
            // 后果是「对话历史」永远是空的，重启也接不回上一次的会话：
            // 时间线明明都在库里，只是没人知道它叫什么。
            let s = crate::ids::Session {
                id: session.clone(),
                parent: None,
                forked_at: crate::ids::Seq::ZERO,
                title: String::new(),
                created_ms: crate::event::now_ms(),
            };
            self.store.create_session(&s).map_err(|e| format!("登记会话失败：{e}"))?;
        }
        let restored = match &resume {
            Some(id) => {
                let s = self.store.clone();
                let id = id.clone();
                Some(tokio::task::spawn_blocking(move || restore(s.as_ref(), &id))
                    .await.map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?)
            }
            None => None,
        };

        let memory = Arc::new(Memory::load_or_bootstrap(&self.dir.join("memory")).await);
        let (ui_tx, ui_rx) = broadcast::channel(8192);
        let (writer, writer_join) = spawn_writer(self.store.clone(), ui_tx.clone());
        let mut cd = CoreDeps::new(
            Arc::new(models),
            Arc::new(registry),
            session.clone(),
            memory,
            writer,
            mode,
        );
        cd.memory_dir = Some(self.dir.join("memory"));
        cd.restored = restored;
        cd.ui = Some(ui_tx);
        let started = start(cd);

        let out = self.out.clone();
        let pump = tokio::spawn(pump(ui_rx, out));

        *self.live.lock().await = Some(Live {
            session,
            handle: started.handle,
            core_join: started.join,
            writer_join,
            pump,
            metrics: deps.metrics.clone(),
            tools,
            web_name,
        });
        Ok(())
    }

    async fn close(self: &Arc<Self>) {
        let Some(live) = self.live.lock().await.take() else { return };
        live.handle.session_shutdown().await;
        let _ = live.core_join.await;
        let _ = live.writer_join.await;
        live.pump.abort();
    }

    fn push(&self, v: Value) {
        let _ = self.out.send(v.to_string());
    }
}

/// UiEvent → 浏览器。**要削推送量就削这里**，只有这一个地方。
async fn pump(mut rx: broadcast::Receiver<UiEvent>, out: broadcast::Sender<String>) {
    loop {
        match rx.recv().await {
            Ok(e) => {
                if let Some(v) = fanout(e) {
                    let _ = out.send(v.to_string());
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let _ = out.send(json!({ "t": "lagged", "n": n }).to_string());
            }
            Err(_) => break,
        }
    }
}

/// 一条 UiEvent 转成给浏览器的 JSON。
///
/// **现在是全量转发**：`Appended` 的每一种 body 都发过去，前端自己决定显示成
/// 「回答」还是「执行过程」。写 core / turn 的时候漏了这条，补上之后 UI 才有得选。
fn fanout(e: UiEvent) -> Option<Value> {
    Some(match e {
        UiEvent::Appended { seq, turn, body } => {
            let kind = body.tag().to_string();
            let mut v = json!({
                "t": "event", "seq": seq.0, "turn": turn.map(|t| t.0), "kind": kind,
                "body": serde_json::to_value(&*body).unwrap_or(Value::Null),
            });
            // 会显示成正文的那几种，顺手把渲染好的 HTML 带上
            if let Some(md) = markdown_of(&body) {
                v["html"] = Value::String(markdown::to_html(&md));
            }
            v
        }
        UiEvent::Delta { turn, text } => json!({ "t": "delta", "turn": turn.0, "text": text }),
        UiEvent::TurnStarted { turn } => json!({ "t": "turn_started", "turn": turn.0 }),
        UiEvent::TurnClosed { turn, aborted } => {
            json!({ "t": "turn_closed", "turn": turn.0, "aborted": aborted })
        }
        UiEvent::Stopping { turn } => json!({ "t": "stopping", "turn": turn.0 }),
        UiEvent::Queued { pending } => json!({ "t": "queued", "pending": pending }),
        UiEvent::StateChanged { seq, ops, dropped } => json!({
            "t": "state_changed", "seq": seq.0,
            "ops": serde_json::to_value(&ops).unwrap_or(Value::Null),
            "dropped": dropped.iter().map(|p| p.0.clone()).collect::<Vec<_>>(),
        }),
        UiEvent::StillRunning { pending, elapsed_ms } => {
            json!({ "t": "still_running", "pending": pending, "elapsed_ms": elapsed_ms })
        }
        UiEvent::TaskDone { idx, name } => json!({ "t": "task_done", "idx": idx, "name": name }),
        UiEvent::Compacted { folded, before_tokens, after_tokens } => json!({
            "t": "compacted", "folded": folded, "before": before_tokens, "after": after_tokens
        }),
        UiEvent::Distilled { path, sections, error } => json!({
            "t": "distilled", "path": path, "error": error,
            "sections": sections.iter().map(|s| json!({
                "file": s.file,
                "mode": match s.mode {
                    crate::memory::WriteMode::Replace => "replace",
                    crate::memory::WriteMode::Append => "append",
                },
                "text": s.text,
            })).collect::<Vec<_>>(),
        }),
        UiEvent::ContextFootprint { total, cacheable, events } => json!({
            "t": "footprint", "total": total, "cacheable": cacheable, "events": events
        }),
        UiEvent::CostTick { role, usage, session_total } => json!({
            "t": "cost", "role": format!("{role:?}"),
            "prompt": usage.prompt, "completion": usage.completion,
            "estimated": usage.estimated, "total": session_total
        }),
        UiEvent::PersistDegraded { why, pending } => {
            json!({ "t": "persist", "ok": false, "why": why, "pending": pending })
        }
        UiEvent::PersistOk => json!({ "t": "persist", "ok": true }),
        UiEvent::MemoryDegraded { file, err } => {
            json!({ "t": "memory_bad", "file": file, "err": err })
        }
        UiEvent::CapsFailed { report } => json!({
            "t": "caps_failed",
            "provider": report.provider, "model": report.model, "role": report.role,
            "verdict": report.verdict,
            "required": report.required.iter().map(|c| json!({
                "code": c.code(), "label": c.label(), "need": c.need(),
            })).collect::<Vec<_>>(),
            "steps": report.steps.iter().map(|s| json!({
                "code": s.cap.code(), "cap": s.cap.label(), "tried": s.tried,
                "outcome": match s.outcome {
                    crate::caps::Outcome::Ok => "ok",
                    crate::caps::Outcome::Rejected => "rejected",
                    crate::caps::Outcome::Exhausted => "exhausted",
                },
                "detail": s.detail,
            })).collect::<Vec<_>>(),
            "text": report.text(),
        }),
        UiEvent::Recovered { events, crashed_turns, reopened_questions } => json!({
            "t": "recovered", "events": events, "crashed": crashed_turns, "reopened": reopened_questions
        }),
        UiEvent::Forked { session, from_turn, at } => json!({
            "t": "forked", "session": session.0, "from_turn": from_turn.0, "at": at.0
        }),
    })
}

/// 哪些 body 是「会作为正文显示」的 —— 只有这些要渲染 markdown。
fn markdown_of(b: &Body) -> Option<String> {
    match b {
        Body::Wrote { text, .. } => Some(text.clone()),
        Body::Said { text, .. } => Some(text.clone()),
        Body::Answered { choice } => Some(choice.clone()),
        Body::Noted { text } => Some(text.clone()),
        Body::Called { text, .. } if !text.trim().is_empty() => Some(text.clone()),
        Body::Asked { question, .. } => Some(question.clone()),
        _ => None,
    }
}

// ───────────────────────── WebSocket ─────────────────────────

async fn ws_upgrade(State(app): State<Arc<App>>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |s| ws_loop(app, s))
}

async fn ws_loop(app: Arc<App>, mut socket: WebSocket) {
    let mut rx = app.out.subscribe();
    // 先把当前状态整份推过去，页面刷新之后不用重放历史
    if let Some(v) = boot(&app).await {
        if socket.send(Ws::Text(v.to_string().into())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Ws::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        handle(&app, v).await;
                    }
                }
                Some(Ok(Ws::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
            b = rx.recv() => match b {
                Ok(s) => {
                    if socket.send(Ws::Text(s.into())).await.is_err() { break }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    if socket.send(Ws::Text(json!({"t":"lagged", "n":n}).to_string().into())).await.is_err() { break }
                }
                Err(_) => break,
            },
        }
    }
}

async fn boot(app: &Arc<App>) -> Option<Value> {
    let memory = Memory::load_or_bootstrap(&app.dir.join("memory")).await;
    let settings = app.settings.read().await.clone();
    let secrets = app.secrets.read().await.clone();
    let live = app.live.lock().await;
    let (session, snap, tools, web_name, metrics) = match live.as_ref() {
        Some(l) => (
            Some(l.session.0.clone()),
            l.handle.session_snapshot().await,
            l.tools.clone(),
            l.web_name.clone(),
            l.metrics.line(),
        ),
        None => (None, None, vec![], "无".into(), String::new()),
    };
    let timeline = match &session {
        Some(s) => app.store.load_chain(&SessionId(s.clone())).unwrap_or_default(),
        None => vec![],
    };
    Some(json!({
        "t": "boot",
        "session": session,
        "sessions": sessions_json(app),
        "settings": serde_json::to_value(&settings).unwrap_or(Value::Null),
        "keys": key_status(&settings, &secrets),
        "warnings": settings.warnings,
        "tools": tools,
        "web": web_name,
        "metrics": metrics,
        "timeline": timeline.iter().map(|e| json!({
            "seq": e.seq.0, "turn": e.turn.map(|t| t.0), "kind": e.body.tag(),
            "body": serde_json::to_value(&e.body).unwrap_or(Value::Null),
            "html": markdown_of(&e.body).map(|m| markdown::to_html(&m)),
        })).collect::<Vec<_>>(),
        "snap": snap.map(|s| snap_json(&s, &memory.prompts.graph)),
        "presets": crate::config::presets_json(),
        // 场景带全字段：左栏要把每个场景做成一个可点开就改的块，
        // 只给 id + label 的话点开是空的。
        "scenes": scenes_json(&memory.playbook)["scenes"],
        "default_tools": memory.playbook.default_tools,
        "memory": memory_files(app),
    }))
}

fn snap_json(s: &crate::msg::Snap, style: &crate::memory::GraphStyle) -> Value {
    json!({
        "session": s.session.0,
        "seq": s.seq.0,
        "turn": s.turn.map(|t| t.0),
        "queued": s.queued,
        "scenes": s.scenes,
        "mode": match s.mode { Mode::Explore => "explore", Mode::Go => "go" },
        "open_questions": s.open_questions.iter().map(|q| json!({
            "seq": q.id.0, "question": q.question, "options": q.options
        })).collect::<Vec<_>>(),
        "ws": serde_json::to_value(&s.ws).unwrap_or(Value::Null),
        "mermaid": render::source(&s.ws.flow, style).map(|(_, src)| src),
    })
}

fn sessions_json(app: &Arc<App>) -> Value {
    let all = app.store.list_sessions().unwrap_or_default();
    let mut v: Vec<Value> = all
        .iter()
        .rev()
        // 只给最近这些算标题：下面那一步要把整条链读回来，不能对着几百个会话做。
        .take(40)
        .map(|s| {
            let title = if s.title.trim().is_empty() { first_words(app, &s.id) } else { s.title.clone() };
            json!({
                "id": s.id.0, "title": title, "parent": s.parent.as_ref().map(|p| p.0.clone()),
                "forked_at": s.forked_at.0, "created_ms": s.created_ms
            })
        })
        .collect();
    // 空会话（点了新对话又没说话）排到后面，别顶掉真正有内容的
    v.sort_by_key(|x| x["title"].as_str().unwrap_or("").is_empty());
    Value::Array(v)
}

/// 拿第一句用户发言当标题。比「新对话 3」有用得多，而且不用额外存字段。
fn first_words(app: &Arc<App>, id: &SessionId) -> String {
    let evs = app.store.load_chain(id).unwrap_or_default();
    let first = evs.iter().find_map(|e| match &e.body {
        Body::Said { text, .. } => Some(text.clone()),
        _ => None,
    });
    match first {
        Some(t) => {
            let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
            if t.chars().count() > 24 { t.chars().take(24).collect::<String>() + "…" } else { t }
        }
        None => String::new(),
    }
}

fn key_status(s: &Settings, sec: &Secrets) -> Value {
    let mut status = Value::Object(
        s.providers
            .iter()
            .map(|(n, p)| {
                let has = sec.resolve(n, p, &real_env).is_some();
                (n.clone(), json!({ "has": has, "env": p.key_env }))
            })
            .collect(),
    );
    status["firecrawl"] = json!({"has": s.web_key(sec, &real_env).is_some(), "env": "FIRECRAWL_API_KEY"});
    status
}

/// 持久层的文件清单 + 内容。**「prompt 都要独立出来」的落点** ——
/// 每个模板是一个可以单独点开改、单独删的块，不是埋在源码里的字符串。
fn memory_files(app: &Arc<App>) -> Value {
    let dir = app.dir.join("memory");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten().filter(|e| e.path().is_file()) {
            let name = e.file_name().to_string_lossy().to_string();
            if draft_allowed(&name) {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                out.push(json!({"file": name, "text": text}));
            }
        }
    }
    for name in ["project.md", "preferences.md", "knowledge.md", "playbook.toml", "prompts.toml"] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        out.push(json!({ "file": name, "text": text }));
    }
    // cases/*.md 是「蒸馏结果」，数量不定
    if let Ok(rd) = std::fs::read_dir(dir.join("cases")) {
        for e in rd.flatten().filter(|e| e.path().is_file()) {
            let name = format!("cases/{}", e.file_name().to_string_lossy());
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            out.push(json!({ "file": name, "text": text }));
        }
    }
    Value::Array(out)
}

// ───────────────────────── 客户端指令 ─────────────────────────

async fn handle(app: &Arc<App>, v: Value) {
    let op = v["op"].as_str().unwrap_or("");
    // 仅串行化生命周期操作，探活/落盘等待不能挡住其他标签页的停止。
    let _command = if matches!(op, "open" | "fork" | "settings_put" | "secret_put") {
        Some(app.commands.lock().await)
    } else { None };
    let live = app.live.lock().await;
    let h = live.as_ref().map(|l| l.handle.clone());
    drop(live);

    match op {
        "send" => {
            let Some(h) = h else { return app.err("还没有会话，先新建一个") };
            let text = v["text"].as_str().unwrap_or("").to_string();
            if text.trim().is_empty() {
                return;
            }
            let mode = if v["interrupt"].as_bool() == Some(true) {
                SendMode::InterruptAndSend
            } else {
                SendMode::Queue
            };
            h.session_send(text, mode).await;
        }
        "answer" => {
            let Some(h) = h else { return };
            let seq = crate::ids::Seq(v["seq"].as_u64().unwrap_or(0));
            let choice = v["choice"].as_str().unwrap_or("").to_string();
            // 回答走 Queue：模型问完就在等，没有要打断的东西
            h.session_answer(seq, choice, SendMode::Queue).await;
        }
        "interrupt" => {
            // 打断要点名轮号 —— 用户点「停止」时看到的那一轮，和 Core 现在跑的
            // 那一轮可能已经不是同一个了（慢一步的点击不该误杀下一轮）。
            let turn = v["turn"].as_u64().map(TurnId);
            if let (Some(h), Some(t)) = (h.clone(), turn) {
                h.session_interrupt(t).await;
            }
        }
        "mode" => {
            if let Some(h) = h {
                let m = if v["to"].as_str() == Some("go") { Mode::Go } else { Mode::Explore };
                h.session_set_mode(m).await;
                app.push(json!({ "t": "mode", "to": v["to"] }));
            }
        }
        // 场景是多选：用户能同时选「目标没说清」和「预算对不上」，
        // 两份 guidance 都会真的注入。一个都不选就回到 none。
        "scene" => {
            if let Some(h) = h {
                let to: Vec<String> = v["to"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .or_else(|| v["to"].as_str().map(|s| vec![s.to_string()]))
                    .unwrap_or_default();
                let to = if to.is_empty() { vec!["none".to_string()] } else { to };
                h.session_override_scene(to).await;
            }
        }
        // ── 场景库：一个场景一个块，改/删都落回 playbook.toml ──
        //
        // 写回前整份校验：`Playbook::from_toml_str` 过不了就不写。
        // 场景库坏掉的症状要到下一轮才以「场景全没了」的形式冒出来。
        "scene_put" => app.push(scene_write(app, &v["scene"], false)),
        "scene_del" => app.push(scene_write(app, &v["scene"], true)),

        // 重命名只改 session 行的 title。
        "session_rename" => {
            let id = SessionId(v["session"].as_str().unwrap_or("").to_string());
            let title = v["title"].as_str().unwrap_or("").trim().to_string();
            match app.store.get_session(&id) {
                Ok(Some(mut s)) => {
                    s.title = title;
                    if let Err(e) = app.store.create_session(&s) {
                        return app.err(&format!("改名失败：{e}"));
                    }
                    app.push(json!({ "t": "sessions", "sessions": sessions_json(app) }));
                }
                _ => app.err("找不到这条会话"),
            }
        }
        // 删除 = 从列表里拿掉。**事件不删**，见 `Store::forget_session`。
        "session_del" => {
            let id = SessionId(v["session"].as_str().unwrap_or("").to_string());
            let cur = app.live.lock().await.as_ref().map(|l| l.session.clone());
            if cur.as_ref() == Some(&id) {
                return app.err("这是当前正在用的会话，先切到别的再删");
            }
            if let Err(e) = app.store.forget_session(&id) {
                return app.err(&format!("删除失败：{e}"));
            }
            app.push(json!({ "t": "sessions", "sessions": sessions_json(app) }));
        }

        "distill" => {
            // 这是 `session_distill` 的第一个调用者 —— 之前它零引用。
            if let Some(h) = h {
                h.session_distill().await;
            }
        }
        // 一键写回蒸馏结果。用户在右栏逐节改过、勾选过之后才走到这里。
        //
        // **每一节单独校验、单独写**：一节的 TOML 写坏了不该连累其它几节，
        // 而且坏在哪一节要说得出来。全成功才算一次成功，部分失败要照实报。
        "distill_apply" => app.push(distill_apply(app, &v["sections"])),

        "edit" => {
            let Some(h) = h else { return };
            match serde_json::from_value::<Vec<Op>>(v["ops"].clone()) {
                Ok(ops) if !ops.is_empty() => {
                    h.session_edit(ops).await;
                }
                Ok(_) => {}
                Err(e) => app.err(&format!("改动解析不了：{e}")),
            }
        }
        "fork" => {
            let Some(h) = h else { return };
            let turn = TurnId(v["turn"].as_u64().unwrap_or(0));
            let title = v["title"].as_str().unwrap_or("").to_string();
            match h.session_fork(turn, title).await {
                Some(Ok(id)) => {
                    // 分叉出来的新会话立刻切过去 —— 用户点「从这里分支」的意图
                    // 就是要在新分支上继续说话，还留在旧会话上等于什么都没发生。
                    if let Err(e) = app.open(Some(id)).await {
                        app.err(&e);
                    }
                    app.reboot().await;
                }
                Some(Err(e)) => app.err(&e),
                None => app.err("分叉没有回执"),
            }
        }
        "open" => {
            let id = v["session"].as_str().unwrap_or("").to_string();
            let target = if id.is_empty() { None } else { Some(SessionId(id)) };
            if let Err(e) = app.open(target).await {
                app.err(&e);
            }
            app.reboot().await;
        }
        "settings_put" => {
            match serde_json::from_value::<Settings>(v["settings"].clone()) {
                Ok(mut next) => {
                    if let Err(e) = next.save(&app.dir) {
                        return app.err(&format!("写 config.json 失败：{e}"));
                    }
                    // 从盘上再读一遍：这样环境变量覆盖、来源标注都是真实的那份，
                    // 而不是浏览器提交上来的那份。
                    next = Settings::load(&app.dir, &real_env);
                    *app.settings.write().await = next;
                    app.restart().await;
                    app.reboot().await;
                }
                Err(e) => app.err(&format!("配置解析不了：{e}")),
            }
        }
        // 只改可读目录。**不走 settings_put。**
        //
        // 输入区那个「应用」原来是把浏览器里整份 settings 发上来覆盖磁盘 ——
        // 而浏览器那份是上一次 boot 的快照。用户在别处改过模型配置之后再点它，
        // 就会把 roles 打回默认（anthropic），下一次重启报「缺密钥」，
        // 看起来像密钥判断错了，实际是配置被这一下覆盖掉了。
        //
        // 现在它只带一个路径：**盘上那份读出来，只动 tools.roots，再写回**。
        // 结构上就不可能再捎带覆盖别的字段。
        "roots_put" => {
            let path = v["path"].as_str().unwrap_or("").trim().to_string();
            if path.is_empty() {
                return app.err("目录不能为空");
            }
            let mut next = Settings::load(&app.dir, &real_env);
            let mut roots = next.tools.roots.clone();
            if roots.is_empty() {
                roots.push(path.clone());
            } else {
                roots[0] = path.clone();
            }
            next.tools.roots = roots;
            if let Err(e) = next.save(&app.dir) {
                return app.err(&format!("写 config.json 失败：{e}"));
            }
            *app.settings.write().await = Settings::load(&app.dir, &real_env);
            app.restart().await;
            app.reboot().await;
        }
        "secret_put" => {
            let provider = v["provider"].as_str().unwrap_or("").to_string();
            let key = v["key"].as_str().unwrap_or("").to_string();
            let mut sec = app.secrets.write().await;
            sec.put(&provider, &key);
            if let Err(e) = sec.save(&app.dir) {
                drop(sec);
                return app.err(&format!("写 secrets.json 失败：{e}"));
            }
            drop(sec);
            app.restart().await;
            app.reboot().await;
        }
        "memory_put" => {
            let file = v["file"].as_str().unwrap_or("");
            let text = v["text"].as_str().unwrap_or("");
            // 只允许写 memory/ 下面已知的那几个名字：这个入口收的是浏览器来的字符串，
            // 不设限等于开了一个任意路径写入。
            if !mem_allowed(file) {
                return app.err(&format!("不允许写 {file}"));
            }
            let path = app.dir.join("memory").join(file);
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            if let Err(e) = std::fs::write(&path, text) {
                return app.err(&format!("写 {file} 失败：{e}"));
            }
            app.push(json!({ "t": "memory", "files": memory_files(app) }));
        }
        "memory_del" => {
            let file = v["file"].as_str().unwrap_or("");
            if !file.starts_with("cases/") || !mem_allowed(file) {
                return app.err("只有 cases/ 下的条目可以删");
            }
            let _ = std::fs::remove_file(app.dir.join("memory").join(file));
            app.push(json!({ "t": "memory", "files": memory_files(app) }));
        }
        // 目录浏览器。**刻意不过 Policy**：它的用途正是「挑一个还没授权的目录
        // 加进白名单」，过闸门就永远只能在已授权范围里打转。
        //
        // 安全边界靠的是另外两条：这个服务只绑本地回环（见 bin/serve.rs），
        // 而且它是 **server 的指令、不是工具** —— 模型碰不到它，模型读文件
        // 仍然只能走 toolkit，仍然必须过 Policy。这里只回名字和是不是目录，
        // 不回任何文件内容。
        "browse" => app.push(browse(app, v["path"].as_str().unwrap_or(""))),

        // 拉这个 provider 当前在售的型号。
        //
        // **不靠写死的列表。** 内置那份是查证时点的快照，一定会过期 ——
        // 上一版里 moonshot-v1-8k 已经下线、deepseek-chat 已经废弃，
        // 而界面还在把它们当候选推给用户。厂商自己的 /models 才是事实。
        "models_probe" => {
            let name = v["provider"].as_str().unwrap_or("").to_string();
            let settings = app.settings.read().await.clone();
            let secrets = app.secrets.read().await.clone();
            let Some(p) = settings.providers.get(&name).cloned() else {
                return app.err(&format!("没有叫 {name} 的 provider"));
            };
            let Some((key, _)) = secrets.resolve(&name, &p, &real_env) else {
                return app.err(&format!("{name} 还没有密钥，填了才能拉型号"));
            };
            app.push(fetch_models(&name, &p, key).await);
        }

        "probe_web" => {
            let settings = app.settings.read().await.clone();
            let secrets = app.secrets.read().await.clone();
            let key = settings.web_key(&secrets, &real_env);
            let msg = match web::build(&settings.web, &settings.tools, key) {
                None => "没有配置联网后端".to_string(),
                Some(b) => {
                    let t = CancellationToken::new();
                    match b.probe(&t).await {
                        Ok(m) => m,
                        Err(e) => format!("不可用：{e}"),
                    }
                }
            };
            app.push(json!({ "t": "probe", "text": msg }));
        }
        "snap" => {
            if let Some(h) = h {
                if let Some(s) = h.session_snapshot().await {
                    let memory = Memory::load_or_bootstrap(&app.dir.join("memory")).await;
                    app.push(json!({ "t": "snap", "snap": snap_json(&s, &memory.prompts.graph) }));
                }
            }
        }
        "memory_get" => app.push(json!({ "t": "memory", "files": memory_files(app) })),
        "sync" => app.reboot().await,
        _ => {}
    }
}

/// `GET {base}/models`。两家的响应形状不同，都收。
///
/// 失败不当错误处理 —— 拉不到就继续用内置候选，用户照样能自己打型号名。
/// 型号名本来就是自由文本，这里只是省一次翻文档。
async fn fetch_models(name: &str, p: &crate::config::ProviderCfg, key: String) -> Value {
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let req = match p.api {
        crate::config::Api::Anthropic => http
            .get(&url)
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01"),
        crate::config::Api::OpenAiCompat => http.get(&url).bearer_auth(&key),
    };
    let out = match req.send().await {
        Err(e) => Err(format!("拉不到：{e}")),
        Ok(r) if !r.status().is_success() => {
            let code = r.status();
            let body = r.text().await.unwrap_or_default();
            Err(format!("{} {}", code.as_u16(), crate::client::clip(&body, 200)))
        }
        Ok(r) => match r.json::<Value>().await {
            Err(e) => Err(format!("响应不是 JSON：{e}")),
            Ok(v) => {
                let mut ids: Vec<String> = v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                ids.sort();
                ids.dedup();
                Ok(ids)
            }
        },
    };
    match out {
        Ok(ids) if !ids.is_empty() => {
            json!({ "t": "models", "provider": name, "models": ids })
        }
        Ok(_) => json!({ "t": "models", "provider": name, "models": [],
                         "error": "这家没有返回型号列表，继续用内置候选" }),
        Err(e) => json!({ "t": "models", "provider": name, "models": [], "error": e }),
    }
}

/// 改写 `playbook.toml` 里的一个场景。`del = true` 就是删掉它。
///
/// **整份读出来、改、校验、再写回**，不是往文件里做局部文本替换 ——
/// 后者遇到用户手写的注释和奇怪缩进就会错，而错法是把文件写坏。
/// 代价是用户写在 toml 里的注释会在这次写回时丢掉，所以只有点了改才走这条路。
fn scene_write(app: &Arc<App>, raw: &Value, del: bool) -> Value {
    let path = app.dir.join("memory").join("playbook.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut pb = match crate::scene::Playbook::from_toml_str(&text) {
        Ok(p) => p,
        Err(e) => return json!({ "t": "err", "msg": format!("现在的 playbook.toml 就读不出来：{e}") }),
    };
    let id = raw["id"].as_str().unwrap_or("").trim().to_string();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return json!({ "t": "err", "msg": "场景 id 只能用字母数字和 - _" });
    }
    if del {
        if id == "none" {
            return json!({ "t": "err", "msg": "none 是判不出场景时的回退，不能删" });
        }
        pb.scenes.remove(&id);
    } else {
        pb.scenes.insert(
            id.clone(),
            crate::scene::Scene {
                id: id.clone(),
                label: raw["label"].as_str().unwrap_or(&id).to_string(),
                when: raw["when"].as_str().unwrap_or("").to_string(),
                guidance: raw["guidance"].as_str().unwrap_or("").to_string(),
                tools: raw["tools"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            },
        );
    }
    let out = pb.to_toml_string();
    // 自己写出来的东西自己读一遍再落盘。这一步挡的是「写完下一轮场景全没了」。
    if let Err(e) = crate::scene::Playbook::from_toml_str(&out) {
        return json!({ "t": "err", "msg": format!("改完之后读不回来了，没保存：{e}") });
    }
    if let Err(e) = std::fs::write(&path, out) {
        return json!({ "t": "err", "msg": format!("写 playbook.toml 失败：{e}") });
    }
    scenes_json(&pb)
}

/// 给 UI 的场景库。**一处生成** —— boot、改场景、蒸馏追加，三条路都用它，
/// 少一个字段就是界面上少一块内容。
fn scenes_json(pb: &crate::scene::Playbook) -> Value {
    json!({
        "t": "scenes",
        "scenes": pb.scenes.values().map(|s| json!({
            "id": s.id, "label": s.label, "when": s.when,
            "guidance": s.guidance, "tools": s.tools,
        })).collect::<Vec<_>>(),
        "default_tools": pb.default_tools,
    })
}

/// 把蒸馏的若干节写回 `memory/`。返回一条给 UI 的结果。
///
/// # 为什么写之前要校验
///
/// `playbook.toml` 是追加写：追进去的东西不是合法 TOML 的话，整份场景库就废了，
/// 而那种失效要到**下一轮**才以「场景全没了、回退到内置目录」的形式冒出来 ——
/// 隔着一次交互的错最难查。案例文件同理：frontmatter 不对就整条读不出来，
/// 而且是静默的（`load_cases` 里 `parse_case` 返回 None 就跳过）。
fn distill_apply(app: &Arc<App>, sections: &Value) -> Value {
    let Some(arr) = sections.as_array() else {
        return json!({ "t": "applied", "ok": 0, "errors": ["没有要写的内容"] });
    };
    let root = app.dir.join("memory");
    let mut done: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();

    for s in arr {
        let file = s["file"].as_str().unwrap_or("").to_string();
        let text = s["text"].as_str().unwrap_or("").to_string();
        if !mem_allowed(&file) || crate::memory::write_mode_for(&file).is_none() {
            errs.push(format!("{file}：不是允许写入的持久层文件"));
            continue;
        }
        if text.trim().is_empty() {
            errs.push(format!("{file}：内容是空的，跳过"));
            continue;
        }
        let path = root.join(&file);
        let append = s["mode"].as_str() == Some("append");
        let merged = if append {
            let cur = std::fs::read_to_string(&path).unwrap_or_default();
            format!("{}\n\n{}\n", cur.trim_end(), text.trim())
        } else {
            format!("{}\n", text.trim())
        };
        if let Err(e) = crate::memory::validate(&file, &merged) {
            errs.push(format!("{file}：{e}"));
            continue;
        }
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        match std::fs::write(&path, &merged) {
            Ok(()) => done.push(file),
            Err(e) => errs.push(format!("{file}：写入失败 {e}")),
        }
    }

    app.push(json!({ "t": "memory", "files": memory_files(app) }));
    // 蒸馏往 playbook 里追加了场景 ⇒ 场景块也得跟着刷。
    // 不刷的话，界面上要到下次重启才看得到那条新场景 —— 而用户刚刚亲手写入了它。
    if done.iter().any(|f| f == "playbook.toml")
        && let Ok(text) = std::fs::read_to_string(root.join("playbook.toml"))
        && let Ok(pb) = crate::scene::Playbook::from_toml_str(&text)
    {
        // quiet：这次刷新是蒸馏顺带的，上面已经报过「写入 3 个文件」了，
        // 再弹一条「场景库已保存」是同一件事说两遍。
        let mut v = scenes_json(&pb);
        v["quiet"] = Value::Bool(true);
        app.push(v);
    }
    json!({ "t": "applied", "ok": done.len(), "files": done, "errors": errs })
}

/// 列一个目录。给操作者挑路径用，见调用点的说明。
fn browse(app: &Arc<App>, path: &str) -> Value {
    let target = if path.trim().is_empty() {
        app.dir.clone()
    } else {
        PathBuf::from(path)
    };
    let real = match target.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            return json!({ "t": "browse", "error": format!("{}：{e}", target.display()) });
        }
    };
    let mut dirs: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    match std::fs::read_dir(&real) {
        Ok(rd) => {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let row = json!({ "name": name, "dir": is_dir });
                if is_dir { dirs.push(row) } else { files.push(row) }
                // 一个 node_modules 能有几万项，列全了浏览器先卡死
                if dirs.len() + files.len() > 800 {
                    break;
                }
            }
        }
        Err(e) => return json!({ "t": "browse", "error": format!("读不了：{e}") }),
    }
    let key = |v: &Value| v["name"].as_str().unwrap_or("").to_lowercase();
    dirs.sort_by_key(key);
    files.sort_by_key(key);
    dirs.extend(files);
    json!({
        "t": "browse",
        "path": real.display().to_string(),
        "parent": real.parent().map(|p| p.display().to_string()),
        "entries": dirs,
        "shortcuts": [
            { "label": "工作目录", "path": app.dir.display().to_string() },
            { "label": "用户目录", "path": home().display().to_string() },
        ],
    })
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `memory/` 下允许浏览器写的名字。**白名单，不是黑名单。**
fn mem_allowed(f: &str) -> bool {
    if f.contains("..") || f.contains('\\') || f.contains(':') || f.starts_with('/') {
        return false;
    }
    matches!(f, "project.md" | "preferences.md" | "knowledge.md" | "playbook.toml" | "prompts.toml")
        || draft_allowed(f)
        || (f.starts_with("cases/")
            && f.matches('/').count() == 1
            && f.ends_with(".md")
            && !f["cases/".len()..].is_empty())
}

fn draft_allowed(f: &str) -> bool {
    f.strip_prefix("draft-").and_then(|s| s.strip_suffix(".md"))
        .is_some_and(|stamp| !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()))
}

impl App {
    async fn restart(self: &Arc<Self>) {
        let session = self.live.lock().await.as_ref().map(|l| l.session.clone());
        if let Err(e) = self.open(session).await {
            self.err(&format!("配置已保存，但会话未重启：{e}"));
        }
    }
    fn err(&self, msg: &str) {
        self.push(json!({ "t": "err", "msg": msg }));
    }

    /// 状态变了（换会话、改配置）⇒ 整份重推。比逐条打补丁可靠。
    async fn reboot(self: &Arc<Self>) {
        if let Some(v) = boot(self).await {
            self.push(v);
        }
    }
}

/// 前端三个文件都带 `no-cache`。
///
/// 它们是 `include_str!` 进来的，换一版就要重新编译；不加这条，浏览器会拿着
/// 上一版的 js/css 不放，改完看不到效果 —— 而那个症状看起来像是改错了地方。
/// 反正是本机回环，省这点带宽没有意义。
fn no_cache(ct: &'static str, s: &'static str) -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, ct),
            (axum::http::header::CACHE_CONTROL, "no-cache, must-revalidate"),
        ],
        s,
    )
}

fn js(s: &'static str) -> impl IntoResponse {
    no_cache("text/javascript; charset=utf-8", s)
}

fn css(s: &'static str) -> impl IntoResponse {
    no_cache("text/css; charset=utf-8", s)
}

/// 起服务。返回实际监听的地址（端口写 0 时由系统分配）。
pub async fn serve(app: Arc<App>, port: u16) -> Result<(), String> {
    // 第一次进来先把上一次的会话接上；没有就开个新的。
    // 失败（比如没配密钥）不致命：页面照样打得开，用户就是进来填密钥的。
    let last = app.store.list_sessions().unwrap_or_default().last().map(|s| s.id.clone());
    if let Err(e) = app.open(last).await {
        eprintln!("[serve] 会话还起不来：{e}");
    }
    let router = app.router();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener =
        tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("绑定 {addr} 失败：{e}"))?;
    let real = listener.local_addr().map_err(|e| e.to_string())?;
    println!("premortem UI → http://{real}");
    axum::serve(listener, router).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("premortem-ui-{}", uuid::Uuid::new_v4()));
        let app = App::new(dir).await.unwrap();
        let mut settings = Settings::default();
        for provider in settings.providers.values_mut() {
            provider.key_env = "PREMORTEM_UI_TEST_UNUSED_KEY".into();
        }
        settings.tools.net = false;
        settings.tools.roots = vec![app.dir.display().to_string()];
        settings.save(&app.dir).unwrap();
        *app.settings.write().await = settings;
        let mut secrets = app.secrets.write().await;
        secrets.put("anthropic", "test-only-no-request");
        secrets.put("openai", "test-only-no-request");
        drop(secrets);
        app
    }

    async fn cleanup(app: Arc<App>) {
        app.close().await;
        let dir = app.dir.clone();
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn answers_render_in_live_and_restored_history() {
        let body = Body::Answered { choice: "**yes** <img src=x onerror=alert(1)>".into() };
        let live = fanout(UiEvent::Appended { seq: crate::ids::Seq(2), turn: None, body: Box::new(body.clone()) }).unwrap();
        let restored = markdown::to_html(&markdown_of(&body).unwrap());
        assert_eq!(live["html"], restored);
        assert!(restored.contains("<strong>yes</strong>"));
        assert!(!restored.contains("onerror"));
    }

    #[test]
    fn memory_paths_reject_traversal_and_windows_streams() {
        for bad in ["../config.json", "cases/../../x.md", "cases/a:b.md", "cases/a\\b.md", "draft-../1.md", "draft-.md"] {
            assert!(!mem_allowed(bad), "{bad}");
        }
        for good in ["project.md", "cases/example.md", "draft-12345.md"] {
            assert!(mem_allowed(good), "{good}");
        }
    }

    #[tokio::test]
    async fn settings_restart_rebuilds_tools_and_preserves_session_history() {
        let app = fixture().await;
        app.open(None).await.unwrap();
        let old = app.live.lock().await.as_ref().unwrap().handle.clone();
        let id = old.session_snapshot().await.unwrap().session;
        old.session_set_mode(Mode::Go).await;
        old.session_edit(vec![Op::set("audit", "preserved")]).await.unwrap();
        let mut next = app.settings.read().await.clone();
        next.tools.net = true;
        next.web.fetch = web::Fetcher::Http;
        handle(&app, json!({"op":"settings_put", "settings":next})).await;
        assert!(old.session_snapshot().await.is_none(), "old Core still active");
        {
            let guard = app.live.lock().await;
            let live = guard.as_ref().unwrap();
            assert_eq!(live.session, id);
            assert_eq!(live.handle.session_snapshot().await.unwrap().mode, Mode::Go);
            assert!(live.tools.iter().any(|s| s == "web_fetch"));
            assert_eq!(live.handle.session_snapshot().await.unwrap().ws.fields[&crate::state::Path::from("audit")].value, "preserved");
        }
        cleanup(app).await;
    }

    /// 输入区那个「应用」只能动可读目录。
    ///
    /// 它原来是把浏览器手上那份完整 settings 发上来覆盖磁盘，而那是上一次 boot
    /// 的快照 —— 用户在别处把 roles 改到别家、填好密钥、跑起来之后再点一下它，
    /// roles 就被打回默认（anthropic），下一次重启报「provider 缺密钥」。
    /// 看起来像密钥判断写错了，实际是配置被这一下覆盖掉了。
    #[tokio::test]
    async fn roots_put_only_touches_roots() {
        let app = fixture().await;
        app.open(None).await.unwrap();
        // 用户把角色改到别家并存好
        {
            let mut s = app.settings.write().await;
            s.roles.judge.provider = "openai".into();
            s.roles.answer.provider = "openai".into();
            s.roles.subagent.provider = "openai".into();
            s.tools.roots = vec![app.dir.display().to_string(), "/second".into()];
            s.save(&app.dir).unwrap();
        }
        let target = app.dir.join("sub");
        std::fs::create_dir_all(&target).unwrap();
        handle(&app, json!({ "op": "roots_put", "path": target.display().to_string() })).await;

        let on_disk = Settings::load(&app.dir, &real_env);
        assert_eq!(on_disk.tools.roots[0], target.display().to_string(), "第一条换成新目录");
        assert_eq!(on_disk.tools.roots[1], "/second", "★ 其余授权原样保留");
        assert_eq!(on_disk.roles.answer.provider, "openai", "★ 模型配置一个字都没动");
        assert_eq!(on_disk.roles.judge.provider, "openai");
        assert_eq!(on_disk.roles.subagent.provider, "openai");
        // 空路径不写任何东西
        handle(&app, json!({ "op": "roots_put", "path": "   " })).await;
        let again = Settings::load(&app.dir, &real_env);
        assert_eq!(again.tools.roots[0], target.display().to_string(), "★ 空路径不把目录改回去");
        cleanup(app).await;
    }

    #[tokio::test]
    async fn invalid_open_preserves_live_core() {
        let app = fixture().await;
        app.open(None).await.unwrap();
        let old = app.live.lock().await.as_ref().unwrap().handle.clone();
        assert!(app.open(Some(SessionId("missing".into()))).await.is_err());
        assert!(old.session_snapshot().await.is_some());
        app.settings.write().await.roles.answer.provider = "missing".into();
        assert!(app.open(None).await.is_err());
        assert!(old.session_snapshot().await.is_some());
        cleanup(app).await;
    }

    #[tokio::test]
    async fn saving_first_key_starts_a_session_and_drafts_are_readable() {
        let app = fixture().await;
        handle(&app, json!({"op":"secret_put", "provider":"anthropic", "key":"new-test-key"})).await;
        assert!(app.live.lock().await.is_some());
        handle(&app, json!({"op":"memory_put", "file":"draft-123.md", "text":"review me"})).await;
        assert!(memory_files(&app).as_array().unwrap().iter().any(|v| v["file"] == "draft-123.md" && v["text"] == "review me"));
        cleanup(app).await;
    }
}
