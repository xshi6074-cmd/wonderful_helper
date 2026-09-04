//! 链路测试台。
//!
//! # 这一版换了断言的方式
//!
//! 上一版 15 个场景全是「我写了什么功能就断言什么功能」，正路走得很顺，
//! 于是恢复链路整条是死代码都没被发现（`restore()` 没有任何调用者）。
//!
//! 现在分三层：
//!
//! 1. **不变量**（[`inv`] 模块）：I1–I8，每个场景收尾都跑一遍。它们不关心
//!    具体走了哪条路，只关心「无论怎么走，这几条必须成立」。
//! 2. **故障注入**：`FaultStore` 让落盘失败、中途丢弃 Core 模拟被 kill。
//! 3. **对抗性 mock**：未知场景 id、重复 call_id、只吐工具不吐正文。
//!
//! 跑法：`cargo run`。全部在内存 SQLite 上跑，不碰真磁盘（除了显式指定目录的几个）。

use premortem::context::ContextLimit;
use premortem::core::{CoreDeps, CoreSummary, start};
use premortem::event::{Body, Event, assemble, crashed_turns, open_questions, unclosed_calls};
use premortem::handle::CoreHandle;
use premortem::ids::{Seq, SessionId, TurnId};
use premortem::memory::Memory;
use premortem::mock::{EchoTool, FlakyTool, MockModel, SlowTool, ask_call, call, default_judge};
use premortem::model::{JudgeOut, Message, Mode, Models, MsgRole, Role, StreamEvent, Usage};
use premortem::msg::{SendMode, UiEvent};
use premortem::persist::{restore, spawn_writer};
use premortem::scene::Playbook;
use premortem::state::{Op, Origin, Phase, Source, Workspace};
use premortem::store::{FaultStore, MemStore, SqliteStore, Store};
use premortem::tools::{Registry, ToolConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static PASS: AtomicUsize = AtomicUsize::new(0);
static FAIL: AtomicUsize = AtomicUsize::new(0);

fn ok(cond: bool, what: &str) {
    if cond {
        PASS.fetch_add(1, Ordering::Relaxed);
        println!("      ✓ {what}");
    } else {
        FAIL.fetch_add(1, Ordering::Relaxed);
        println!("      ✗ {what}");
    }
}

fn head(n: &str, title: &str) {
    println!("\n─── {n} · {title} ───");
}

// ══════════════════════════════ 不变量 ══════════════════════════════

mod inv {
    use super::*;

    /// I1 从头重放得到的 Workspace ≡ checkpoint / 运行时的那份。
    pub fn i1_replay_matches(events: &[Event], live: &Workspace) {
        let mut ws = Workspace::new();
        for e in events {
            ws.apply(e);
        }
        let a = serde_json::to_string(&ws).unwrap_or_default();
        let b = serde_json::to_string(live).unwrap_or_default();
        ok(a == b, "I1 重放 ≡ 运行时 workspace");
    }

    /// I3 每条带 tool_calls 的 assistant 都有配齐的 tool 消息。
    pub fn i3_calls_paired(events: &[Event]) {
        let msgs = assemble(events);
        let mut bad = 0;
        for (i, m) in msgs.iter().enumerate() {
            if !matches!(m.role, MsgRole::Assistant) || m.tool_calls.is_empty() {
                continue;
            }
            for c in &m.tool_calls {
                let paired = msgs[i + 1..]
                    .iter()
                    .any(|x| x.tool_call_id.as_deref() == Some(c.id.as_str()));
                if !paired {
                    bad += 1;
                }
            }
        }
        ok(bad == 0, "I3 tool_calls 全部配对（组装后）");
    }

    /// I4 seq 严格 +1，无空洞无重复。
    pub fn i4_seq_dense(events: &[Event]) {
        let mut expect = 1u64;
        let mut bad = 0;
        for e in events {
            if e.seq.0 != expect {
                bad += 1;
            }
            expect = e.seq.0 + 1;
        }
        ok(bad == 0, "I4 seq 严格递增无空洞");
    }

    /// I5 从事件重建的账本 ≡ 运行时账本。
    pub fn i5_cost_matches(events: &[Event], live: &premortem::cost::CostLedger) {
        let rebuilt = premortem::cost::CostLedger::from_events(events);
        ok(rebuilt.total() == live.total(), "I5 账本可从事件重建");
    }

    /// I7 用户写过的字段，经任何路径后仍然是 User。
    pub fn i7_origin_kept(events: &[Event], paths: &[&str]) {
        let mut ws = Workspace::new();
        for e in events {
            ws.apply(e);
        }
        let all = paths.iter().all(|p| {
            ws.fields
                .get(&premortem::state::Path::new(*p))
                .map(|f| f.origin == Origin::User)
                .unwrap_or(false)
        });
        ok(all, "I7 恢复后 [用户设定] 标记不丢");
    }

    /// I8 被折叠的原文仍在时间线上。
    pub fn i8_folded_kept(events: &[Event]) {
        let folded: Vec<(Seq, Seq)> = events
            .iter()
            .filter_map(|e| match &e.body {
                Body::Folded { from, to, .. } => Some((*from, *to)),
                _ => None,
            })
            .collect();
        if folded.is_empty() {
            return;
        }
        let all = folded
            .iter()
            .all(|(f, t)| events.iter().any(|e| e.seq >= *f && e.seq <= *t));
        ok(all, "I8 折叠区间的原文仍在时间线上");
    }

    /// 全套。
    pub fn all(events: &[Event], live_ws: &Workspace, cost: &premortem::cost::CostLedger) {
        i1_replay_matches(events, live_ws);
        i3_calls_paired(events);
        i4_seq_dense(events);
        i5_cost_matches(events, cost);
        i8_folded_kept(events);
    }
}

// ══════════════════════════════ 测试台 ══════════════════════════════

struct RigOpts {
    mode: Mode,
    tool_config: ToolConfig,
    context_limit: ContextLimit,
    memory_dir: Option<std::path::PathBuf>,
    store: Arc<dyn Store>,
    session: SessionId,
    checkpoint_every: u64,
}

impl Default for RigOpts {
    fn default() -> Self {
        Self {
            mode: Mode::Explore,
            tool_config: ToolConfig::default(),
            context_limit: ContextLimit::default(),
            memory_dir: None,
            store: Arc::new(SqliteStore::memory().expect("内存库")),
            session: SessionId::from("s0"),
            checkpoint_every: 200,
        }
    }
}

struct Rig {
    handle: CoreHandle,
    model: Arc<MockModel>,
    store: Arc<dyn Store>,
    session: SessionId,
    log: Arc<Mutex<Vec<UiEvent>>>,
    core_join: tokio::task::JoinHandle<CoreSummary>,
    writer_join: tokio::task::JoinHandle<()>,
}

impl Rig {
    async fn build(model: MockModel, registry: Registry, opts: RigOpts) -> Rig {
        let model = Arc::new(model);
        let store = opts.store.clone();
        let memory = match &opts.memory_dir {
            Some(d) => Arc::new(Memory::load_or_bootstrap(d).await),
            None => Arc::new(Memory::empty()),
        };
        // 恢复是启动路径的第一步，不是可选分支 —— 空库就是一条事件都没有。
        let sess = opts.session.clone();
        let restored = {
            let s = store.clone();
            let sid = sess.clone();
            tokio::task::spawn_blocking(move || restore(s.as_ref(), &sid).ok()).await.unwrap()
        };
        let restored = restored.filter(|r| !r.events.is_empty() || !r.repairs.is_empty());

        // writer 与 Core 共用一条 UI 通道 —— 各建各的，writer 的降级提示就到不了界面。
        let (ui_tx, _) = tokio::sync::broadcast::channel(8192);
        let (writer, writer_join) = spawn_writer(store.clone(), ui_tx.clone());

        let mut deps = CoreDeps::new(
            Arc::new(Models::uniform(model.clone())),
            Arc::new(registry),
            sess.clone(),
            memory,
            writer,
            opts.mode,
        );
        deps.tool_config = opts.tool_config;
        deps.context_limit = opts.context_limit;
        deps.memory_dir = opts.memory_dir.clone();
        deps.restored = restored;
        deps.checkpoint_every = opts.checkpoint_every;
        deps.ui = Some(ui_tx);

        let started = start(deps);
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let mut rx = started.ui;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => l.lock().unwrap().push(e),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        Rig {
            handle: started.handle,
            model,
            store,
            session: sess,
            log,
            core_join: started.join,
            writer_join,
        }
    }

    async fn new(model: MockModel, registry: Registry) -> Rig {
        Rig::build(model, registry, RigOpts::default()).await
    }

    /// 等 UI 出现满足条件的事件。超时返回 false。
    async fn wait<F: FnMut(&UiEvent) -> bool>(&self, mut f: F, ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if self.log.lock().unwrap().iter().any(&mut f) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        false
    }

    async fn quiet(&self, turns: u64) -> bool {
        self.wait(
            |e| matches!(e, UiEvent::TurnClosed { turn, .. } if turn.0 == turns),
            4000,
        )
        .await
    }

    fn tags(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                UiEvent::Appended { body, .. } => Some(body.tag().to_string()),
                _ => None,
            })
            .collect()
    }

    fn saw<F: FnMut(&UiEvent) -> bool>(&self, mut f: F) -> bool {
        self.log.lock().unwrap().iter().any(&mut f)
    }

    /// 从 store 把时间线读回来。先过一次落盘屏障，消除 best-effort 攒批带来的
    /// 时序不确定 —— 断言的是「最终会落盘」，不是「立刻落盘」。
    async fn timeline(&self) -> Vec<Event> {
        self.handle.session_flush().await;
        self.store.load_chain(&self.session).unwrap_or_default()
    }

    /// 优雅退出：cancel + 补齐未闭合 + flush。
    async fn finish(self) -> CoreSummary {
        self.handle.session_shutdown().await;
        let s = self.core_join.await.expect("core join");
        let _ = tokio::time::timeout(Duration::from_secs(2), self.writer_join).await;
        s
    }

    /// **模拟被 kill**：不 shutdown，直接丢掉 handle。
    /// 已经交给 writer 的事件会随 channel 关闭被冲掉，正在跑的 turn 就此中断。
    async fn kill(self) -> Arc<dyn Store> {
        let store = self.store.clone();
        drop(self.handle);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.core_join).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.writer_join).await;
        store
    }
}

fn chunks(parts: &[&str]) -> Vec<StreamEvent> {
    let mut v: Vec<StreamEvent> =
        parts.iter().map(|p| StreamEvent::Chunk((*p).to_string())).collect();
    v.push(StreamEvent::Done(Usage { prompt: 400, completion: 60, estimated: false }));
    v
}

fn judge_with(scene: &str, ops: Vec<Op>) -> JudgeOut {
    JudgeOut {
        scene: scene.into(),
        rationale: format!("判成 {scene}"),
        ops,
        retrieve: vec![],
        usage: Usage { prompt: 120, completion: 30, estimated: false },
    }
}

// ══════════════════════════════ 场景 ══════════════════════════════

async fn s01_normal_turn() {
    head("S01", "正常一轮：时间线形状");
    let m = MockModel::new().on_judge(default_judge()).on_answer(chunks(&["好", "的"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("帮我想一个实验", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");
    let tags = r.tags();
    ok(
        tags == vec!["said", "turn_open", "judged", "cost", "cost", "wrote", "turn_close"],
        &format!("事件序列正确：{tags:?}"),
    );
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s02_scene_injected() {
    head("S02", "场景选定后真的灌进 prompt");
    let m = MockModel::new()
        .on_judge(judge_with("check_assumption", vec![]))
        .on_answer(chunks(&["嗯"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("直接上大实验", SendMode::Queue).await;
    r.quiet(1).await;
    let ap = r.model.answer_prompt(0);
    let jp = r.model.judge_prompt(0);
    let g = Playbook::builtin().get("check_assumption").unwrap().guidance.clone();
    ok(ap.contains("本轮场景"), "回答段带场景标题");
    ok(ap.contains(g.lines().next().unwrap_or("")), "回答段带 guidance 正文");
    ok(!jp.contains(g.lines().next().unwrap_or("_x_")), "判断段不含 guidance（两段式省的就是这个）");
    ok(jp.contains("直接上大实验"), "判断段含完整对话");
    r.finish().await;
}

async fn s03_tool_lifecycle() {
    head("S03", "工具的小生命周期：发出→返回，corr 连得上");
    let m = MockModel::new()
        .on_judge(default_judge())
        .on_answer(vec![
            StreamEvent::Chunk("查一下".into()),
            StreamEvent::ToolCalls(vec![call("c1", "echo"), call("c2", "echo")]),
        ])
        .on_judge(default_judge())
        .on_answer(chunks(&["查完了"]));
    let r = Rig::new(m, Registry::new().with(Arc::new(EchoTool))).await;
    r.handle.session_send("读一下仓库", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");
    let tl = r.timeline().await;
    let called = tl.iter().find(|e| e.body.tag() == "called").map(|e| e.seq);
    let rets: Vec<&Event> = tl.iter().filter(|e| e.body.tag() == "returned").collect();
    ok(rets.len() == 2, "两条返回都落盘了");
    ok(rets.iter().all(|e| e.corr == called), "返回的 corr 指回发起它的那条 Called");
    let tasks: Vec<u64> = tl
        .iter()
        .filter_map(|e| match &e.body {
            Body::Returned { task, .. } => Some(task.0),
            _ => None,
        })
        .collect();
    ok(tasks.len() == 2 && tasks[0] != tasks[1], &format!("TaskId 分到具体调用：{tasks:?}"));
    ok(unclosed_calls(&tl).is_empty(), "没有未闭合的调用");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s04_injection() {
    head("S04", "插话被吸收，且插话本身先落盘");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(60))
        .on_judge(default_judge())
        .on_answer(chunks(&["第一次"]))
        .on_judge(default_judge())
        .on_answer(chunks(&["带上插话回答"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("第一句", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. }), 1000).await;
    let ack = r.handle.session_send("等一下，补充一点", SendMode::Queue).await;
    ok(ack.as_ref().map(|a| a.durable).unwrap_or(false), "插话落盘确认后才 ack（must-flush）");
    ok(r.quiet(1).await, "一轮跑完");
    let last = r.model.answer_prompt(r.model.answer_calls() - 1);
    ok(last.contains("补充一点"), "插话进了后续 prompt");
    let s = r.finish().await;
    ok(s.metrics.inbox_taken >= 2, &format!("取走了 {} 条输入", s.metrics.inbox_taken));
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s05_interrupt() {
    head("S05", "打断：两段式反馈 + 半截正文落盘");
    let m = MockModel::new()
        .chunk_delay(Duration::from_millis(40))
        .on_judge(default_judge())
        .on_answer(chunks(&["一", "二", "三", "四", "五"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("说点长的", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::Delta { .. }), 2000).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    r.handle.session_interrupt(TurnId(1)).await;
    ok(
        r.wait(|e| matches!(e, UiEvent::Stopping { .. }), 500).await,
        "Stopping 先到（不等子任务收敛）",
    );
    ok(r.quiet(1).await, "TurnClosed 随后到");
    ok(
        r.saw(|e| matches!(e, UiEvent::TurnClosed { aborted, .. } if *aborted)),
        "标记为 aborted",
    );
    let tl = r.timeline().await;
    let half = tl.iter().any(|e| matches!(&e.body, Body::Wrote { interrupted, .. } if *interrupted));
    ok(half, "半截正文落成 Wrote{interrupted}");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s06_interrupt_mid_tool() {
    head("S06", "工具未返回就被打断 ⇒ 记 Aborted，组装后仍配对");
    let m = MockModel::new().on_judge(default_judge()).on_answer(vec![
        StreamEvent::Chunk("跑个慢的".into()),
        StreamEvent::ToolCalls(vec![call("slow1", "slow")]),
    ]);
    let reg = Registry::new().with(Arc::new(SlowTool::parallel("slow", Duration::from_secs(30))));
    let r = Rig::new(m, reg).await;
    r.handle.session_send("跑吧", SendMode::Queue).await;
    ok(
        r.wait(|e| matches!(e, UiEvent::Appended { body, .. } if body.tag() == "called"), 2000)
            .await,
        "Called 发出即落盘",
    );
    r.handle.session_interrupt(TurnId(1)).await;
    ok(r.quiet(1).await, "收敛完成");
    let tl = r.timeline().await;
    ok(tl.iter().any(|e| e.body.tag() == "aborted" || e.body.tag() == "returned"), "补上了闭合事件");
    ok(unclosed_calls(&tl).is_empty(), "没有遗留的未闭合调用");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s07_arbitration() {
    head("S07", "turn 内仲裁：撞上用户改动就丢，时间线只记生效的");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(80))
        .on_judge(judge_with(
            "none",
            vec![Op::set("spec.claim", "模型写的"), Op::set("spec.metric", "BWT")],
        ))
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("开始", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. }), 1000).await;
    // 判断段还在跑的时候用户 apply 了同一个 path
    let ack = r.handle.session_edit(vec![Op::set("spec.claim", "用户写的")]).await;
    ok(ack.is_some(), "apply 后才产生事件，且有 ack");
    ok(r.quiet(1).await, "一轮跑完");

    let tl = r.timeline().await;
    let inferred: Vec<&Event> = tl.iter().filter(|e| e.body.tag() == "inferred").collect();
    let recorded_paths: Vec<String> = inferred
        .iter()
        .filter_map(|e| match &e.body {
            Body::Inferred { ops, .. } => Some(ops.iter().map(|o| o.path().to_string()).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    let dropped: Vec<String> = inferred
        .iter()
        .filter_map(|e| match &e.body {
            Body::Inferred { dropped, .. } => Some(dropped.iter().map(|p| p.to_string()).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    ok(!recorded_paths.contains(&"spec.claim".to_string()), "被丢的那条没进事件的 ops");
    ok(recorded_paths.contains(&"spec.metric".to_string()), "没撞车的那条正常生效");
    ok(dropped.contains(&"spec.claim".to_string()), "被丢的路径记在 dropped 里（审计）");
    ok(
        tl.iter().any(|e| matches!(&e.body, Body::Noted{text} if text.contains("没有生效"))),
        "被丢的改动回喂给了模型",
    );

    let s = r.finish().await;
    let claim = s.ws.fields.get(&premortem::state::Path::new("spec.claim")).unwrap();
    ok(claim.value == serde_json::json!("用户写的"), "最终值是用户的");
    ok(claim.origin == Origin::User, "origin 是 User");
    inv::all(&s.events, &s.ws, &s.cost);
    inv::i7_origin_kept(&s.events, &["spec.claim"]);
}

async fn s08_edit_outside_turn() {
    head("S08", "轮外编辑不进仲裁集：模型下一轮可以改");
    let m = MockModel::new()
        .on_judge(judge_with("none", vec![Op::set("spec.claim", "模型改的")]))
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    // 空闲时编辑
    r.handle.session_edit(vec![Op::set("spec.claim", "用户先写的")]).await;
    r.handle.session_send("你看看", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");
    let s = r.finish().await;
    let claim = s.ws.fields.get(&premortem::state::Path::new("spec.claim")).unwrap();
    ok(claim.value == serde_json::json!("模型改的"), "轮外编辑不触发仲裁，模型的改动生效");
    ok(s.metrics.ops_dropped == 0, "没有 op 被丢");
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s09_question_answer() {
    head("S09", "提问是持久实体，回答连得回去");
    let m = MockModel::new()
        .on_judge(default_judge())
        .on_answer(vec![
            StreamEvent::Chunk("有个问题".into()),
            StreamEvent::ToolCalls(vec![ask_call("q1", "用哪个数据集？", &["CIFAR100", "ImageNet"])]),
        ])
        .on_judge(default_judge())
        .on_answer(chunks(&["收到"]));
    let r = Rig::new(m, Registry::new().with(Arc::new(premortem::tools::AskUser))).await;
    r.handle.session_send("开始设计", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let snap = r.handle.session_snapshot().await.unwrap();
    ok(snap.open_questions.len() == 1, "提问在 open_questions 里挂着");
    let qid = snap.open_questions[0].id;
    ok(snap.open_questions[0].options.len() == 2, "选项也在");

    r.handle.session_answer(qid, "CIFAR100", SendMode::Queue).await;
    ok(r.quiet(2).await, "回答开了新一轮");
    let snap2 = r.handle.session_snapshot().await.unwrap();
    ok(snap2.open_questions.is_empty(), "回答之后提问不再挂着");

    let tl = r.timeline().await;
    let ans = tl.iter().find(|e| e.body.tag() == "answered").unwrap();
    ok(ans.corr == Some(qid), "回答的 corr 指回提问");
    let prompt = r.model.answer_prompt(r.model.answer_calls() - 1);
    ok(prompt.contains("对提问"), "组装 prompt 时写明了这是对提问的回答");
    ok(open_questions(&tl).is_empty(), "扫时间线也认为没有未答提问");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s10_dedupe() {
    head("S10", "同 client_id 重发被去重");
    let m = MockModel::new().on_judge(default_judge()).on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    let id = uuid::Uuid::new_v4();
    r.handle.session_resend(id, "手抖了", SendMode::Queue).await;
    let again = r.handle.session_resend(id, "手抖了", SendMode::Queue).await;
    ok(again.is_none(), "第二次被丢弃");
    ok(r.quiet(1).await, "只跑了一轮");
    let s = r.finish().await;
    ok(s.metrics.deduped_inputs == 1, "去重计数 = 1");
    ok(s.events.iter().filter(|e| e.body.tag() == "said").count() == 1, "时间线上只有一条");
}

async fn s11_fold() {
    head("S11", "折叠：区间落成事件，原文不删");
    let mut limit = ContextLimit::default();
    limit.max_prompt_tokens = 260;
    limit.trigger_at = 0.5;
    limit.keep_recent_turns = 1;
    let mut m = MockModel::new();
    for _ in 0..4 {
        m = m.on_judge(default_judge()).on_answer(chunks(&["这是一段比较长的回答用来把上下文撑起来"]));
    }
    let opts = RigOpts { context_limit: limit, ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    for i in 0..4 {
        r.handle.session_send(format!("第 {i} 句，内容要够长才能把上下文撑到水位线以上"), SendMode::Queue).await;
        r.quiet(i + 1).await;
    }
    let tl = r.timeline().await;
    let folds: Vec<&Event> = tl.iter().filter(|e| e.body.tag() == "folded").collect();
    ok(!folds.is_empty(), &format!("触发了折叠（{} 次）", folds.len()));
    if let Some(Body::Folded { from, to, .. }) = folds.first().map(|e| &e.body) {
        ok(
            tl.iter().any(|e| e.seq >= *from && e.seq <= *to && e.body.tag() == "said"),
            "被折叠区间的原文仍在时间线上",
        );
        let msgs = assemble(&tl);
        ok(
            msgs.iter().any(|m| m.content.contains("已折叠早期对话")),
            "组装时注入了摘要",
        );
        // ★ 之前这里只断言「摘要出现了」，没断言「原文不见了」——
        // 于是「摘要和原文同时进 prompt、一个 token 没省下」这个 bug 溜了过去。
        let folded_texts: Vec<String> = tl
            .iter()
            .filter(|e| e.seq >= *from && e.seq <= *to)
            .filter_map(|e| match &e.body {
                Body::Said { text, .. } => Some(text.clone()),
                Body::Wrote { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        ok(!folded_texts.is_empty(), "折叠区间里确实有对话");
        ok(
            !folded_texts
                .iter()
                .any(|t| msgs.iter().any(|m| m.content.contains(t.as_str()))),
            "★ 被折叠的原文不再出现在 prompt 里（摘要是代替，不是追加）",
        );
    }
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s12_graceful_restart() {
    head("S12", "优雅退出后重开：状态与对话都在");
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    {
        let m = MockModel::new()
            .on_judge(judge_with("none", vec![Op::grounded(
                "spec.dataset",
                "CIFAR100",
                Source::Repo("configs/a.yaml:12".into()),
                0.9,
            )]))
            .on_answer(chunks(&["记下了"]));
        let opts = RigOpts { store: store.clone(), ..Default::default() };
        let r = Rig::build(m, Registry::new(), opts).await;
        r.handle.session_send("用 CIFAR100", SendMode::Queue).await;
        r.quiet(1).await;
        r.handle.session_edit(vec![Op::set("spec.claim", "用户的主张")]).await;
        r.handle.session_advance_phase(Phase::Handoff).await;
        let s = r.finish().await;
        ok(s.events.len() >= 7, &format!("第一段产生了 {} 条事件", s.events.len()));
    }
    // 重开
    let m = MockModel::new().on_judge(default_judge()).on_answer(chunks(&["继续"]));
    let opts = RigOpts { store: store.clone(), ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    ok(
        r.wait(|e| matches!(e, UiEvent::Recovered { events, .. } if *events >= 7), 1000).await,
        "启动即恢复（不是可选分支）",
    );
    let snap = r.handle.session_snapshot().await.unwrap();
    ok(snap.ws.phase == Phase::Handoff, "阶段恢复了");
    ok(
        snap.ws.fields.get(&premortem::state::Path::new("spec.dataset")).is_some(),
        "推断图恢复了",
    );
    let claim = snap.ws.fields.get(&premortem::state::Path::new("spec.claim")).unwrap();
    ok(claim.origin == Origin::User, "★ 用户填的字段恢复后仍是 [用户设定]");
    ok(
        claim.source == Some(Source::User),
        "★ 来源也没丢",
    );
    r.handle.session_send("接着说", SendMode::Queue).await;
    ok(r.quiet(2).await, "接着跑第二轮");
    let prompt = r.model.judge_prompt(0);
    ok(prompt.contains("用 CIFAR100"), "★ 上一次会话的对话进了 prompt");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
    inv::i7_origin_kept(&s.events, &["spec.claim"]);
}

async fn s13_crash_restart() {
    head("S13", "被 kill 之后重开：补 Aborted、标 crashed、输入不丢");
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    {
        let m = MockModel::new().on_judge(default_judge()).on_answer(vec![
            StreamEvent::Chunk("我去查一下".into()),
            StreamEvent::ToolCalls(vec![call("c9", "slow")]),
        ]);
        let reg =
            Registry::new().with(Arc::new(SlowTool::parallel("slow", Duration::from_secs(30))));
        let opts = RigOpts { store: store.clone(), ..Default::default() };
        let r = Rig::build(m, reg, opts).await;
        r.handle.session_send("查一下这个", SendMode::Queue).await;
        r.wait(|e| matches!(e, UiEvent::Appended { body, .. } if body.tag() == "called"), 2000)
            .await;
        r.kill().await; // 不 shutdown
    }
    let before = store.load_chain(&SessionId::from("s0")).unwrap();
    ok(
        before.iter().any(|e| e.body.tag() == "said"),
        "★ 已 ack 的用户输入活下来了（I2）",
    );
    ok(unclosed_calls(&before).len() == 1, "盘上留下了一个未闭合的调用");
    ok(crashed_turns(&before).len() == 1, "盘上留下了一个开了没关的轮次");

    let m = MockModel::new().on_judge(default_judge()).on_answer(chunks(&["接着来"]));
    let opts = RigOpts { store: store.clone(), ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    ok(
        r.wait(|e| matches!(e, UiEvent::Recovered { crashed_turns, .. } if *crashed_turns == 1), 1000)
            .await,
        "启动时报出了上次异常退出",
    );
    let tl = r.timeline().await;
    ok(unclosed_calls(&tl).is_empty(), "★ 未闭合调用被补上了 Aborted");
    ok(crashed_turns(&tl).is_empty(), "★ 崩掉的轮次被补上了 TurnClosed");
    inv::i3_calls_paired(&tl);
    r.handle.session_send("再来一次", SendMode::Queue).await;
    ok(r.quiet(2).await, "恢复后能继续跑（turn 编号接着上次）");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s14_persist_degraded() {
    head("S14", "落盘失败：降级提示，对话继续，恢复后重试成功");
    let inner: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    // 从第 1 次 append 起连续失败 4 次，之后恢复正常
    let faulty: Arc<dyn Store> = Arc::new(FaultStore::new(inner.clone(), 1, 4));
    let m = MockModel::new()
        .on_judge(default_judge())
        .on_answer(chunks(&["还能答"]))
        .on_judge(default_judge())
        .on_answer(chunks(&["还能答二"]));
    let opts = RigOpts { store: faulty.clone(), ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    r.handle.session_send("盘挂了也要能说话", SendMode::Queue).await;
    ok(r.quiet(1).await, "★ 落盘失败不影响这一轮跑完");
    ok(
        r.wait(|e| matches!(e, UiEvent::PersistDegraded { .. }), 2000).await,
        "发出了降级提示（不是弹窗打扰）",
    );
    r.handle.session_send("第二句", SendMode::Queue).await;
    ok(r.quiet(2).await, "第二轮照常");
    ok(r.wait(|e| matches!(e, UiEvent::PersistOk), 3000).await, "恢复正常后清除降级");
    let s = r.finish().await;
    let on_disk = inner.load_chain(&SessionId::from("s0")).unwrap();
    ok(
        on_disk.len() == s.events.len(),
        &format!("★ 攒着的事件一条没丢，全部补写（盘 {} / 内存 {}）", on_disk.len(), s.events.len()),
    );
    inv::i4_seq_dense(&on_disk);
}

async fn s15_fork() {
    head("S15", "R5 分叉：原会话一条不动，新分支带着分叉点之前的历史");
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let mut m = MockModel::new();
    for i in 0..3 {
        m = m
            .on_judge(judge_with("none", vec![Op::set(format!("f{i}"), format!("v{i}"))]))
            .on_answer(chunks(&["好"]));
    }
    let opts = RigOpts { store: store.clone(), ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    for i in 0..3u64 {
        r.handle.session_send(format!("第{i}句"), SendMode::Queue).await;
        r.quiet(i + 1).await;
    }
    let before = r.handle.session_snapshot().await.unwrap();
    ok(before.ws.fields.len() == 3, "分叉前有 3 个字段");

    let child = r.handle.session_fork(TurnId(3), "改走小规模复现").await.unwrap();
    ok(child.is_ok(), &format!("分叉成功：{child:?}"));
    let child = child.unwrap();
    ok(r.saw(|e| matches!(e, UiEvent::Forked { .. })), "推了 Forked");

    // ★ 原会话一条事件都不动
    let after = r.handle.session_snapshot().await.unwrap();
    ok(after.ws.fields.len() == 3, "★ 原会话不受影响，字段还是 3 个");
    let parent_tl = r.timeline().await;
    ok(parent_tl.len() == before.seq.0 as usize, "★ 原会话时间线一条没少");
    let s_parent = r.finish().await;

    // 打开新分支
    let m2 = MockModel::new().on_judge(default_judge()).on_answer(chunks(&["换个方向"]));
    let opts2 =
        RigOpts { store: store.clone(), session: child.clone(), ..Default::default() };
    let rc = Rig::build(m2, Registry::new(), opts2).await;
    let snap = rc.handle.session_snapshot().await.unwrap();
    ok(snap.ws.fields.len() == 2, "★ 新分支只带到分叉点：2 个字段");
    ok(snap.ws.fields.get(&premortem::state::Path::new("f2")).is_none(), "第三轮的推断不在分支里");
    ok(snap.session == child, "快照里带着自己的 session id");

    // 分支上「第2句」还在、但没有回答 —— 分叉点在 TurnOpened(t3) 之前，
    // 而触发 t3 的那句输入是在开轮之前落盘的。「从 t3 分叉」= t3 这一轮重来。
    let branch_tl = rc.timeline().await;
    ok(
        branch_tl.iter().any(|e| matches!(&e.body, Body::Said{text,..} if text.contains("第2句"))),
        "★ 触发那一轮的用户输入留在分支上（它是这一轮的输入，不是上一轮的产物）",
    );
    ok(
        !branch_tl.iter().any(|e| e.turn == Some(TurnId(3))),
        "★ 但 t3 本身（判定 / 回答 / 推断）一条都不在分支里",
    );

    rc.handle.session_send("在分支上继续", SendMode::Queue).await;
    ok(rc.quiet(3).await, "分支上能继续跑，轮号接着分叉点");
    let jp = rc.model.judge_prompt(0);
    ok(
        jp.contains("第0句") && jp.contains("第1句"),
        "★ 分支一开口就有分叉点之前的完整上下文",
    );
    ok(!jp.contains("v2"), "t3 推断出的字段不在分支的推断图里");

    let s_child = rc.finish().await;
    ok(
        s_child.events.iter().any(|e| e.session == child),
        "分支自己的新事件盖的是分支的章",
    );
    ok(
        s_child.events.iter().any(|e| e.session != child),
        "★ 链上带着父会话的事件，事件自描述得出它是谁的",
    );
    let sessions = store.list_sessions().unwrap();
    ok(sessions.len() == 1 && sessions[0].parent.is_some(), "会话表里记了 parent 与 forked_at");
    inv::i1_replay_matches(&s_parent.events, &s_parent.ws);
    inv::i1_replay_matches(&s_child.events, &s_child.ws);
}

async fn s16_memory_hot_reload() {
    head("S16", "持久层每轮重读：改了文件下一轮生效");
    let dir = std::env::temp_dir().join(format!("premortem-mem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let m = MockModel::new()
        .on_judge(default_judge())
        .on_answer(chunks(&["一"]))
        .on_judge(default_judge())
        .on_answer(chunks(&["二"]));
    let opts = RigOpts { memory_dir: Some(dir.clone()), ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    r.handle.session_send("第一轮", SendMode::Queue).await;
    r.quiet(1).await;
    ok(dir.join("prompts.toml").exists(), "prompts.toml 被 bootstrap 出来了");
    ok(dir.join("playbook.toml").exists(), "playbook.toml 也在");
    ok(!r.model.judge_prompt(0).contains("我在做持续学习的遗忘曲线"), "第一轮还没有新内容");

    // 用户直接改文件
    std::fs::write(dir.join("project.md"), "# 项目\n我在做持续学习的遗忘曲线\n").unwrap();
    r.handle.session_send("第二轮", SendMode::Queue).await;
    r.quiet(2).await;
    ok(
        r.model.judge_prompt(1).contains("我在做持续学习的遗忘曲线"),
        "★ 改了 project.md，下一轮 prompt 就是新的（不用重启）",
    );

    // 改坏一个文件
    std::fs::write(dir.join("playbook.toml"), "这不是合法 toml [[[").unwrap();
    r.handle.session_send("第三轮", SendMode::Queue).await;
    r.quiet(3).await;
    ok(
        r.wait(|e| matches!(e, UiEvent::MemoryDegraded { file, .. } if file == "playbook.toml"), 2000)
            .await,
        "★ 改坏了要像界面报错一样报出来（不是只 eprintln）",
    );
    r.finish().await;
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s17_scene_override() {
    head("S17", "换场景：下一轮真的注入新场景的材料");
    let m = MockModel::new()
        .on_judge(judge_with("none", vec![]))
        .on_answer(chunks(&["一"]))
        .on_judge(judge_with("none", vec![]))
        .on_answer(chunks(&["二"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("第一轮", SendMode::Queue).await;
    r.quiet(1).await;
    let g = Playbook::builtin().get("trace_code").unwrap().guidance.clone();
    let key = g.lines().next().unwrap_or("_x_").to_string();
    ok(!r.model.answer_prompt(0).contains(&key), "第一轮没有 trace_code 的 guidance");

    r.handle.session_override_scene("trace_code").await;
    r.handle.session_send("第二轮", SendMode::Queue).await;
    r.quiet(2).await;
    ok(
        r.model.answer_prompt(1).contains(&key),
        "★ 换了场景之后新场景的 guidance 真的灌进去了（不只是换标签）",
    );
    let s = r.finish().await;
    ok(s.metrics.scene_overrides == 1, "换场景被记了一次（场景描述准不准的反馈）");
    ok(
        s.events.iter().any(|e| e.body.tag() == "scene_override"),
        "换场景是时间线上的事件，不是 workspace 里的字段",
    );
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s18_judge_failure() {
    head("S18", "判断段失败：降级继续，留明账，不打扰用户");
    let m = MockModel::new().on_judge_err("上游 503").on_answer(chunks(&["照样答"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("问点什么", SendMode::Queue).await;
    ok(r.quiet(1).await, "★ 判断段挂了，这一轮照样跑完");
    let tl = r.timeline().await;
    ok(
        tl.iter().any(|e| matches!(&e.body, Body::Noted { text } if text.contains("判断段失败"))),
        "留了明账",
    );
    ok(
        tl.iter().any(|e| matches!(&e.body, Body::Judged { scene, .. } if scene == "none")),
        "降级为 none 场景",
    );
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s19_adversarial_model() {
    head("S19", "对抗性 mock：未知场景 / 重复 call_id / 只吐工具不吐正文");
    let m = MockModel::new()
        .on_judge(judge_with("这个场景根本不存在", vec![]))
        .on_answer(vec![StreamEvent::ToolCalls(vec![
            call("dup", "echo"),
            call("dup", "echo"),
            call("x", "根本没有这个工具"),
        ])])
        .on_judge(default_judge())
        .on_answer(chunks(&["收拾干净了"]));
    let r = Rig::new(m, Registry::new().with(Arc::new(EchoTool))).await;
    r.handle.session_send("来点脏的", SendMode::Queue).await;
    ok(r.quiet(1).await, "★ 没有 panic，跑完了");
    let tl = r.timeline().await;
    ok(
        tl.iter().any(|e| matches!(&e.body, Body::Judged { scene, .. } if scene == "none")),
        "未知场景回退到 none",
    );
    ok(
        tl.iter().any(|e| matches!(&e.body, Body::Returned { outcome, .. } if outcome == "not_found")),
        "不存在的工具作为结果返回给模型（不抛异常）",
    );
    ok(unclosed_calls(&tl).is_empty(), "重复 call_id 也被闭合了");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s20_tool_failures() {
    head("S20", "工具失败/超时：一律回给模型，不中断 turn 不问用户");
    let m = MockModel::new()
        .on_judge(default_judge())
        .on_answer(vec![StreamEvent::ToolCalls(vec![
            call("f1", "flaky"),
            call("s1", "slow"),
        ])])
        .on_judge(default_judge())
        .on_answer(chunks(&["我换个做法"]));
    let cfg = ToolConfig {
        heartbeat: Duration::from_millis(30),
        hard_limit: Duration::from_millis(120),
    };
    let reg = Registry::new()
        .with(Arc::new(FlakyTool))
        .with(Arc::new(SlowTool::parallel("slow", Duration::from_secs(30))));
    let opts = RigOpts { tool_config: cfg, ..Default::default() };
    let r = Rig::build(m, reg, opts).await;
    r.handle.session_send("跑工具", SendMode::Queue).await;
    ok(r.quiet(1).await, "★ 工具失败与超时都没有中断这一轮");
    ok(r.saw(|e| matches!(e, UiEvent::StillRunning { .. })), "长任务期间有心跳");
    let tl = r.timeline().await;
    let outcomes: Vec<String> = tl
        .iter()
        .filter_map(|e| match &e.body {
            Body::Returned { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    ok(outcomes.contains(&"failed".to_string()), "失败作为结果返回");
    ok(outcomes.contains(&"timeout".to_string()), "超时作为结果返回");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s21_open_parked() {
    head("S21", "待落定/搁置走同一条 op 通道，享受同一套仲裁");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(80))
        .on_judge(judge_with(
            "none",
            vec![Op::open(vec!["模型加的问题".into()]), Op::parked(vec!["模型搁置的".into()])],
        ))
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("整理一下", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. }), 1000).await;
    r.handle.session_edit(vec![Op::open(vec!["用户加的问题".into()])]).await;
    ok(r.quiet(1).await, "一轮跑完");
    let s = r.finish().await;
    ok(
        s.ws.open == vec!["用户加的问题".to_string()],
        &format!("★ 用户加的没被静默盖掉：{:?}", s.ws.open),
    );
    ok(s.ws.parked == vec!["模型搁置的".to_string()], "没撞车的那个正常生效");
    ok(s.metrics.ops_dropped == 1, "撞车的那条被记为丢弃");
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s22_checkpoint_equivalence() {
    head("S22", "checkpoint 只是加速：删光也能重放出同一份");
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let mut m = MockModel::new();
    for i in 0..3 {
        m = m
            .on_judge(judge_with("none", vec![Op::set(format!("k{i}"), i as i64)]))
            .on_answer(chunks(&["好"]));
    }
    let opts = RigOpts { store: store.clone(), checkpoint_every: 3, ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    for i in 0..3u64 {
        r.handle.session_send(format!("第{i}"), SendMode::Queue).await;
        r.quiet(i + 1).await;
    }
    let s = r.finish().await;
    let ck = store.latest_checkpoint(&SessionId::from("s0")).unwrap();
    ok(ck.is_some(), "打了 checkpoint");
    if let Some((cs, ckws)) = ck {
        let mut ws = Workspace::new();
        for e in s.events.iter().filter(|e| e.seq <= cs) {
            ws.apply(e);
        }
        ok(
            serde_json::to_string(&ws).unwrap() == serde_json::to_string(&ckws).unwrap(),
            "★ I1 重放到 checkpoint 的位置 ≡ checkpoint 本身",
        );
    }
    // 同一串事件喂进一个没有任何 checkpoint 的新库 ⇒ 只能从 0 全量重放
    let bare: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    bare.append(&s.events).unwrap();
    ok(
        bare.latest_checkpoint(&SessionId::from("s0")).unwrap().is_none(),
        "新库里没有 checkpoint",
    );
    let restored = restore(bare.as_ref(), &SessionId::from("s0")).unwrap();
    ok(
        serde_json::to_string(&restored.ws).unwrap() == serde_json::to_string(&s.ws).unwrap(),
        "★ 从 0 全量重放也得到同一份 workspace（checkpoint 纯属加速）",
    );
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s23_memstore_parity() {
    head("S23", "两个 Store 实现喂同一串事件，物化结果必须一样");
    let a: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let b: Arc<dyn Store> = Arc::new(MemStore::new());
    let m = MockModel::new()
        .on_judge(judge_with("none", vec![Op::set("x", "1"), Op::open(vec!["q".into()])]))
        .on_answer(chunks(&["好"]));
    let opts = RigOpts { store: a.clone(), ..Default::default() };
    let r = Rig::build(m, Registry::new(), opts).await;
    r.handle.session_send("走一轮", SendMode::Queue).await;
    r.quiet(1).await;
    let s = r.finish().await;

    b.append(&s.events).unwrap();
    let ra = restore(a.as_ref(), &SessionId::from("s0")).unwrap();
    let rb = restore(b.as_ref(), &SessionId::from("s0")).unwrap();
    ok(
        serde_json::to_string(&ra.ws).unwrap() == serde_json::to_string(&rb.ws).unwrap(),
        "★ SqliteStore 与 MemStore 物化结果一致",
    );
    ok(ra.events.len() == rb.events.len(), "事件条数一致");
    ok(
        assemble(&ra.events).len() == assemble(&rb.events).len(),
        "组装出的消息条数一致",
    );
}

async fn s24_message_assembly() {
    head("S24", "组装：状态与观测事件不进 prompt，对话事件全进");
    let m = MockModel::new()
        .on_judge(judge_with("none", vec![Op::set("a", "1")]))
        .on_answer(chunks(&["答"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("问", SendMode::Queue).await;
    r.quiet(1).await;
    r.handle.session_edit(vec![Op::set("b", "2")]).await;
    let s = r.finish().await;
    let msgs = assemble(&s.events);
    let roles: Vec<String> = msgs.iter().map(|m| format!("{:?}", m.role)).collect();
    ok(roles == vec!["User", "Assistant"], &format!("只有对话进 prompt：{roles:?}"));
    ok(
        s.events.iter().any(|e| e.body.tag() == "inferred"),
        "推断改动在时间线上（作为 metadata 动态拼，不作为消息）",
    );
    ok(
        s.events.iter().any(|e| e.body.tag() == "edited"),
        "用户编辑在时间线上",
    );
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s25_answer_sees_judge() {
    head("S25", "★ 回答段必须看得见判断段刚写下的东西");
    // 判断段推断出两个字段、回喂一条 Noted、检索到一段材料，
    // 这些全部发生在「取视图 ①」之后。回答段的 prompt 必须都有。
    let m = MockModel::new()
        .on_judge(JudgeOut {
            scene: "none".into(),
            rationale: "顺手记两个字段".into(),
            ops: vec![
                Op::set("spec.dataset", "CIFAR100-仅判断段写的"),
                Op::set("spec.metric", "BWT-仅判断段写的"),
            ],
            retrieve: vec![call("r1", "echo")],
            usage: Usage { prompt: 120, completion: 30, estimated: false },
        })
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new().with(Arc::new(EchoTool))).await;
    r.handle.session_send("开始", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let jp = r.model.judge_prompt(0);
    let ap = r.model.answer_prompt(0);
    ok(!jp.contains("CIFAR100-仅判断段写的"), "判断段自己的 prompt 里当然没有（那时还没写）");
    ok(
        ap.contains("CIFAR100-仅判断段写的") && ap.contains("BWT-仅判断段写的"),
        "★ 判断段写进 workspace 的推断，回答段 prompt 里看得见",
    );
    ok(ap.contains("为本轮检索到的材料"), "★ 判断段检索到的材料也在");
    ok(ap.contains("(本轮)"), "本轮新写的字段被标出来了（field.seq > turn_start）");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s26_answer_sees_injection_and_drop() {
    head("S26", "★ 插话与被丢弃的改动，也都从视图走，不靠 turn 自己攒");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(80))
        .on_judge(judge_with("none", vec![Op::set("spec.claim", "模型写的")]))
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("第一句", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. }), 1000).await;
    // 判断段跑着的时候：插一句话 + 改同一个字段
    r.handle.session_send("插一句", SendMode::Queue).await;
    r.handle.session_edit(vec![Op::set("spec.claim", "用户写的")]).await;
    ok(r.quiet(1).await, "一轮跑完");

    let ap = r.model.answer_prompt(r.model.answer_calls() - 1);
    ok(ap.contains("插一句"), "★ 插话经视图进了回答段（turn 不再自己拼局部副本）");
    ok(ap.contains("没有生效"), "★ 被丢弃的改动的回喂也在");
    ok(ap.contains("用户写的"), "★ 推断图里是用户的值");
    ok(!ap.contains("= \"模型写的\""), "模型那条确实没生效");
    let s = r.finish().await;
    ok(s.metrics.ops_dropped == 1, "仲裁记了一次丢弃");
    inv::all(&s.events, &s.ws, &s.cost);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    println!("Premortem 链路测试 —— 主时间线版");
    println!("{}", "═".repeat(64));

    s01_normal_turn().await;
    s02_scene_injected().await;
    s03_tool_lifecycle().await;
    s04_injection().await;
    s05_interrupt().await;
    s06_interrupt_mid_tool().await;
    s07_arbitration().await;
    s08_edit_outside_turn().await;
    s09_question_answer().await;
    s10_dedupe().await;
    s11_fold().await;
    s12_graceful_restart().await;
    s13_crash_restart().await;
    s14_persist_degraded().await;
    s15_fork().await;
    s16_memory_hot_reload().await;
    s17_scene_override().await;
    s18_judge_failure().await;
    s19_adversarial_model().await;
    s20_tool_failures().await;
    s21_open_parked().await;
    s22_checkpoint_equivalence().await;
    s23_memstore_parity().await;
    s24_message_assembly().await;
    s25_answer_sees_judge().await;
    s26_answer_sees_injection_and_drop().await;

    let p = PASS.load(Ordering::Relaxed);
    let f = FAIL.load(Ordering::Relaxed);
    println!("\n{}", "═".repeat(64));
    println!("26 个场景 · {} 条断言：{p} 通过，{f} 失败", p + f);
    if f > 0 {
        std::process::exit(1);
    }
}

// 让未使用的导入有个去处（这些是给后续场景预留的）
#[allow(dead_code)]
fn _unused(_: Message, _: Role, _: CoreHandle) {}
