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
use crate::state::{Op, Phase};
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
        let app = Arc::new(App {
            dir,
            store,
            settings: RwLock::new(settings),
            secrets: RwLock::new(secrets),
            live: Mutex::new(None),
            commands: Mutex::new(()),
            out,
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
        let models = settings.build_models(&secrets, &real_env)?;

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
        UiEvent::Distilled { draft } => json!({ "t": "distilled", "draft": draft }),
        UiEvent::ContextFootprint { total, cacheable, events } => json!({
            "t": "footprint", "total": total, "cacheable": cacheable, "events": events
        }),
        UiEvent::PhaseChanged { to } => json!({ "t": "phase", "to": phase_str(to) }),
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

fn phase_str(p: Phase) -> &'static str {
    match p {
        Phase::Designing => "designing",
        Phase::Handoff => "handoff",
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
        "scenes": memory.playbook.scenes.values().map(|s| json!({"id":s.id,"label":s.label})).collect::<Vec<_>>(),
        "memory": memory_files(app),
    }))
}

fn snap_json(s: &crate::msg::Snap, style: &crate::memory::GraphStyle) -> Value {
    json!({
        "session": s.session.0,
        "seq": s.seq.0,
        "turn": s.turn.map(|t| t.0),
        "queued": s.queued,
        "scene": s.scene,
        "mode": match s.mode { Mode::Explore => "explore", Mode::Go => "go" },
        "phase": phase_str(s.ws.phase),
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
        "phase" => {
            if let Some(h) = h {
                let p = if v["to"].as_str() == Some("handoff") {
                    Phase::Handoff
                } else {
                    Phase::Designing
                };
                h.session_advance_phase(p).await;
            }
        }
        "scene" => {
            if let Some(h) = h {
                h.session_override_scene(v["to"].as_str().unwrap_or("none").to_string()).await;
            }
        }
        "distill" => {
            // 这是 `session_distill` 的第一个调用者 —— 之前它零引用。
            if let Some(h) = h {
                h.session_distill().await;
            }
        }
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
