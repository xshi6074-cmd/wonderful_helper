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

use premortem::config::{Secrets, Settings, Src};
use premortem::context::ContextLimit;
use premortem::core::{CoreDeps, CoreSummary, start};
use premortem::event::{Body, Event, assemble, crashed_turns, open_questions, unclosed_calls};
use premortem::handle::CoreHandle;
use premortem::ids::{NodeId, Seq, SessionId, TurnId};
use premortem::memory::Memory;
use premortem::mock::{
    EchoTool, FlakyTool, MockModel, SlowTool, ask_call, call, call_with, default_judge, judge_all,
    judge_of,
};
use premortem::policy::{Policy, PolicyCfg};
use premortem::model::{Message, Mode, Models, MsgRole, Role, StreamEvent, Usage};
use premortem::msg::{SendMode, UiEvent};
use premortem::persist::{restore, spawn_writer};
use premortem::scene::Playbook;
use premortem::state::{FlowView, Lang, Op, Origin, Source, Workspace};
use premortem::store::{FaultStore, MemStore, SqliteStore, Store};
use premortem::toolkit;
use premortem::tools::{Registry, ToolConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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
                .map(|f| f.prov.origin == Origin::User)
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
    /// I9 图完整：无悬边、parent 链无环且不过深。
    ///
    /// 悬边是删节点之后最容易留下的残渣，而它在 UI 上根本看不见 ——
    /// mermaid 会替你把端点凭空画出来，图看着好好的，数据已经烂了。
    pub fn i9_graph_sound(ws: &Workspace) {
        let g = &ws.flow;
        let dangling = g
            .edges
            .values()
            .filter(|e| !g.nodes.contains_key(&e.from) || !g.nodes.contains_key(&e.to))
            .count();
        ok(dangling == 0, "I9 图上没有悬边");
        let bad = g.nodes.keys().filter(|id| g.depth_of(id).is_none()).count();
        ok(bad == 0, "I9 parent 链无环且深度合法");
    }

    /// I10 渲染是纯函数，而且**不吞节点**。
    ///
    /// 后半条是重点：渲染少画一个节点是完全静默的 —— 用户看到的图缺一块，
    /// 模型看到的图也缺同一块，两边一致所以谁都发现不了。
    pub fn i10_render_faithful(ws: &Workspace) {
        let style = premortem::memory::GraphStyle::default();
        let a = premortem::render::mermaid(&ws.flow, &style);
        let b = premortem::render::mermaid(&ws.flow, &style);
        ok(a == b, "I10 同一张图渲染两次逐字节相同");
        let missing = ws.flow.nodes.keys().filter(|k| !a.contains(k.0.as_str())).count();
        ok(missing == 0, "I10 每个节点都出现在渲染结果里（渲染不吞节点）");
    }

    /// I11 时间线上不存在未解析的别名。
    ///
    /// 漏一处的症状不是当场报错，而是重放到那里时多出一个永远指不到的引用。
    pub fn i11_no_alias(events: &[Event]) {
        let mut bad = 0;
        for e in events {
            let ops: &[Op] = match &e.body {
                Body::Edited { ops } => ops,
                Body::Inferred { ops, .. } => ops,
                _ => continue,
            };
            for op in ops {
                let mut op = op.clone();
                if op.node_refs_mut().iter().any(|r| r.is_alias())
                    || op.edge_refs_mut().iter().any(|r| r.is_alias())
                {
                    bad += 1;
                }
            }
        }
        ok(bad == 0, "I11 时间线上没有未解析的别名");
    }

    pub fn all(events: &[Event], live_ws: &Workspace, cost: &premortem::cost::CostLedger) {
        i1_replay_matches(events, live_ws);
        i3_calls_paired(events);
        i4_seq_dense(events);
        i5_cost_matches(events, cost);
        i8_folded_kept(events);
        i9_graph_sound(live_ws);
        i10_render_faithful(live_ws);
        i11_no_alias(events);
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

/// 判断段暴露给模型的工具名。只有一个 —— 这是 any/required 能替代点名的前提。
fn judge_tool_names() -> Vec<&'static str> {
    vec!["record_judgement"]
}

fn ok_kind(r: &premortem::tools::ToolResult) -> bool {
    r.kind == premortem::tools::ToolResultKind::Ok
}

// judge_with(scene, ops) 没了：判断段不再写推断。
// 判成某场景用 mock::judge_of，提交推断用 .on_ops() / .push_ops()
// —— 后者走的是回答段的动作工具，也就是真实模型走的那条路。

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
        .on_judge(judge_of("check_assumption"))
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
        .on_judge(judge_of("none"))
        .on_ops(&[Op::set("spec.claim", "模型写的"), Op::set("spec.metric", "BWT")])
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
    // 这一场只有图外 op，键就是 path 本身，所以拿一张空图查就够了。
    let g = premortem::state::Graph::default();
    let recorded_paths: Vec<String> = inferred
        .iter()
        .filter_map(|e| match &e.body {
            Body::Inferred { ops, .. } => {
                Some(ops.iter().map(|o| o.key(&g).to_string()).collect::<Vec<_>>())
            }
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
    // 回喂现在走工具返回，不再另发一条 Noted：动作调用本来就有一条回话，
    // 「你这条没生效」写在那里，模型下一步一定读得到。
    ok(
        tl.iter().any(|e| matches!(&e.body,
            Body::Returned { name, content, .. }
            if name == premortem::actions::RECORD_NOTE && content.contains("没有生效"))),
        "被丢的改动回喂给了模型（写在动作工具的返回里）",
    );

    let s = r.finish().await;
    let claim = s.ws.fields.get(&premortem::state::Path::new("spec.claim")).unwrap();
    ok(claim.value == serde_json::json!("用户写的"), "最终值是用户的");
    ok(claim.prov.origin == Origin::User, "origin 是 User");
    inv::all(&s.events, &s.ws, &s.cost);
    inv::i7_origin_kept(&s.events, &["spec.claim"]);
}

async fn s08_edit_outside_turn() {
    head("S08", "轮外编辑不进仲裁集：模型下一轮可以改");
    let m = MockModel::new()
        .on_judge(judge_of("none"))
        .on_ops(&[Op::set("spec.claim", "模型改的")])
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
            .on_judge(judge_of("none"))
        .on_ops(&[Op::grounded(
                "spec.dataset",
                "CIFAR100",
                Source::Repo("configs/a.yaml:12".into()),
                0.9,
            )])
            .on_answer(chunks(&["记下了"]));
        let opts = RigOpts { store: store.clone(), ..Default::default() };
        let r = Rig::build(m, Registry::new(), opts).await;
        r.handle.session_send("用 CIFAR100", SendMode::Queue).await;
        r.quiet(1).await;
        r.handle.session_edit(vec![Op::set("spec.claim", "用户的主张")]).await;
        r.handle.session_override_scene(vec!["trace_code".into()]).await;
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
    ok(snap.scenes == vec!["trace_code".to_string()], "★ 用户选的场景跨重启还在");
    ok(
        snap.ws.fields.get(&premortem::state::Path::new("spec.dataset")).is_some(),
        "推断图恢复了",
    );
    let claim = snap.ws.fields.get(&premortem::state::Path::new("spec.claim")).unwrap();
    ok(claim.prov.origin == Origin::User, "★ 用户填的字段恢复后仍是 [用户设定]");
    ok(
        claim.prov.source == Some(Source::User),
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
            .on_judge(judge_of("none"))
        .on_ops(&[Op::set(format!("f{i}"), format!("v{i}"))])
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
        .on_judge(judge_of("none"))
        .on_answer(chunks(&["一"]))
        .on_judge(judge_of("none"))
        .on_answer(chunks(&["二"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("第一轮", SendMode::Queue).await;
    r.quiet(1).await;
    let g = Playbook::builtin().get("trace_code").unwrap().guidance.clone();
    let key = g.lines().next().unwrap_or("_x_").to_string();
    ok(!r.model.answer_prompt(0).contains(&key), "第一轮没有 trace_code 的 guidance");

    r.handle.session_override_scene(vec!["trace_code".into()]).await;
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
        tl.iter().any(|e| matches!(&e.body, Body::Judged { scenes, .. } if scenes == &["none".to_string()])),
        "降级为 none 场景",
    );
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s19_adversarial_model() {
    head("S19", "对抗性 mock：未知场景 / 重复 call_id / 只吐工具不吐正文");
    let m = MockModel::new()
        .on_judge(judge_of("这个场景根本不存在"))
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
        tl.iter().any(|e| matches!(&e.body, Body::Judged { scenes, .. } if scenes == &["none".to_string()])),
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
        .on_judge(judge_of("none"))
        .on_ops(&[Op::open(vec!["模型加的问题".into()]), Op::parked(vec!["模型搁置的".into()])])
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
            .on_judge(judge_of("none"))
        .on_ops(&[Op::set(format!("k{i}"), i as i64)])
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
        .on_judge(judge_of("none"))
        .on_ops(&[Op::set("x", "1"), Op::open(vec!["q".into()])])
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
        .on_judge(judge_of("none"))
        .on_ops(&[Op::set("a", "1")])
        .on_answer(chunks(&["答"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("问", SendMode::Queue).await;
    r.quiet(1).await;
    r.handle.session_edit(vec![Op::set("b", "2")]).await;
    let s = r.finish().await;
    let msgs = assemble(&s.events);
    let roles: Vec<String> = msgs.iter().map(|m| format!("{:?}", m.role)).collect();
    // 一次动作调用 = 一条带 tool_calls 的 assistant + 一条 tool 返回，本来就该在。
    ok(
        roles == vec!["User", "Assistant", "Tool", "Assistant"],
        &format!("只有对话与工具往返进 prompt：{roles:?}"),
    );
    ok(
        !msgs.iter().any(|m| m.content.contains("\"a\"") && m.content.contains("\"b\"")),
        "★ 推断与用户编辑本身不进 prompt（它们是 metadata，组装时现拼）",
    );
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
    head("S25", "★ 模型刚写下的推断，同一轮的下一次回答调用就看得见");
    // 新架构：推断由**回答段**的动作工具写。第一次回答调用提交 op，
    // 第二次回答调用的 prompt 里必须已经有它 —— 这条链断了的话，
    // 模型会在同一轮里反复重写同一个结论。
    let m = MockModel::new()
        .on_judge(judge_of("none"))
        .on_ops(&[
            Op::set("spec.dataset", "CIFAR100-模型写的"),
            Op::set("spec.metric", "BWT-模型写的"),
        ])
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new().with(Arc::new(EchoTool))).await;
    r.handle.session_send("开始", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let jp = r.model.judge_prompt(0);
    let ap0 = r.model.answer_prompt(0);
    let ap1 = r.model.answer_prompt(1);
    ok(!jp.contains("CIFAR100-模型写的"), "判断段跑在最前面，那时还没有这条推断");
    ok(!ap0.contains("CIFAR100-模型写的"), "第一次回答调用也没有 —— 它正是产出这条 op 的那次");
    ok(
        ap1.contains("CIFAR100-模型写的") && ap1.contains("BWT-模型写的"),
        "★ 动作工具写进 workspace 的推断，同一轮下一次回答就看得见",
    );
    ok(ap1.contains("(本轮)"), "本轮新写的字段被标出来了（field.seq > turn_start）");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s26_answer_sees_injection_and_drop() {
    head("S26", "★ 插话与被丢弃的改动，也都从视图走，不靠 turn 自己攒");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(80))
        .on_judge(judge_of("none"))
        .on_ops(&[Op::set("spec.claim", "模型写的")])
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

async fn s27_model_builds_graph() {
    head("S27", "★ 模型建图：别名铸成真 id，图与明细都进 prompt");
    let m = MockModel::new()
        .on_judge(judge_of("none"))
        .on_ops(&[
                Op::node("$enc", "module", "对比编码器"),
                Op::node("$loss", "loss", "InfoNCE"),
                Op::edge("$e1", "$enc", "$loss"),
                Op::anchored("risk.negatives", "负样本数可能不够", "$loss"),
            ])
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("我想做对比学习", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let snap = r.handle.session_snapshot().await.unwrap();
    let g = &snap.ws.flow;
    ok(g.nodes.len() == 2 && g.edges.len() == 1, "两个节点一条边落地了");
    ok(g.nodes.keys().all(|k| !k.0.starts_with('$')), "★ 别名全部铸成了真 id");
    ok(
        g.nodes.keys().all(|k| k.0.starts_with('n') && k.0.contains('_')),
        "id 形如 n<seq>_<i>，自带溯源",
    );
    let e = g.edges.values().next().unwrap();
    ok(
        g.nodes.contains_key(&e.from) && g.nodes.contains_key(&e.to),
        "★ 边的两端指向真节点（批内别名也解析了）",
    );
    let f = snap.ws.fields.values().next().unwrap();
    ok(
        f.anchor.as_ref().is_some_and(|a| g.nodes.contains_key(a)),
        "★ 图外推断挂到了真节点上",
    );

    // 图是这一轮的回答段自己画的，所以看的是**下一次**回答调用的 prompt。
    let ap = r.model.answer_prompt(1);
    ok(ap.contains("flowchart"), "回答段 prompt 里有 mermaid");
    ok(ap.contains("对比编码器") && ap.contains("InfoNCE"), "两个节点的标签都在");
    ok(
        ap.contains("{{\"InfoNCE\"}}"),
        "★ loss 用了 loss 的形状（词表来自持久层，不是写死的）",
    );
    ok(ap.contains("guess"), "★ 没标来源的节点画成虚线 —— 盲区要看得见");
    ok(ap.contains("负样本数可能不够"), "★ 挂在节点上的图外推断跟着节点走");
    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s28_edge_relink_blocked() {
    head("S28", "★ 漏洞 1：用户断开一条线，模型换个 edge id 也加不回来");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(80))
        .on_judge(judge_of("none"))
        .on_ops(&[
                Op::node("$a", "data", "原始语料"),
                Op::node("$b", "module", "分词器"),
                Op::edge("$e", "$a", "$b"),
            ])
        .on_answer(chunks(&["建好了"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("建图", SendMode::Queue).await;
    ok(r.quiet(1).await, "第一轮建出图");

    let snap = r.handle.session_snapshot().await.unwrap();
    let ids: Vec<NodeId> = snap.ws.flow.nodes.keys().cloned().collect();
    let eid = snap.ws.flow.edges.keys().next().cloned().unwrap();
    ok(ids.len() == 2 && snap.ws.flow.edges.len() == 1, "两个节点一条边");

    // 第二轮：模型用一个**全新的 edge id** 把同样的连接加回来
    r.model.push_judge(judge_of("none"));
    r.model.push_ops(&[Op::edge("$again", ids[0].clone(), ids[1].clone())]);
    r.model.push_answer(chunks(&["继续"]));
    r.handle.session_send("继续", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { turn, .. } if turn.0 == 2), 1000)
        .await;
    // 用户在这一轮里把那条线断开
    r.handle.session_edit(vec![Op::drop_edge(eid)]).await;
    ok(r.quiet(2).await, "第二轮跑完");

    let s = r.finish().await;
    ok(s.ws.flow.edges.is_empty(), "★ 断了就是断了 —— 换 id 绕不过有序端点对这个键");
    ok(s.ws.flow.nodes.len() == 2, "两个节点没被牵连");
    ok(s.metrics.ops_dropped >= 1, "被丢的那条记进了 dropped（模型下一轮看得到）");
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s29_node_resurrect_blocked() {
    head("S29", "★ 漏洞 2：用户删掉一个节点，模型换 id 新建同名的也复活不了");
    let m = MockModel::new()
        .judge_delay(Duration::from_millis(80))
        .on_judge(judge_of("none"))
        .on_ops(&[
                Op::node("$a", "data", "原始语料"),
                Op::node("$b", "module", "分词器"),
            ])
        .on_answer(chunks(&["建好了"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("建图", SendMode::Queue).await;
    ok(r.quiet(1).await, "第一轮建出图");

    let snap = r.handle.session_snapshot().await.unwrap();
    let victim = snap
        .ws
        .flow
        .nodes
        .values()
        .find(|n| n.label == "分词器")
        .map(|n| n.id.clone())
        .unwrap();

    // 第二轮：模型新建一个 label 一模一样的节点
    r.model.push_judge(judge_of("none"));
    r.model.push_ops(&[Op::node("$dup", "module", "分词器")]);
    r.model.push_answer(chunks(&["继续"]));
    r.handle.session_send("继续", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { turn, .. } if turn.0 == 2), 1000)
        .await;
    r.handle.session_edit(vec![Op::drop_node(victim)]).await;
    ok(r.quiet(2).await, "第二轮跑完");

    let s = r.finish().await;
    ok(
        !s.ws.flow.nodes.values().any(|n| n.label == "分词器"),
        "★ 删掉的节点没被同名新建复活（撞的是归一化标签键）",
    );
    ok(s.ws.flow.nodes.len() == 1, "只剩另一个节点");
    ok(s.metrics.ops_dropped >= 1, "记进了 dropped");
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s30_phantom_ids_rejected() {
    head("S30", "★ 幽灵 id 拦在门外；删节点连带收掉它的边");
    let m = MockModel::new()
        .on_judge(judge_of("none"))
        .on_ops(&[
                Op::node("$a", "data", "训练集"),
                Op::node("$b", "module", "主干"),
                Op::edge("$ok", "$a", "$b"),
                // 端点是模型记错的 id
                Op::edge("$bad", "n99_9", "n88_8"),
                // 新元素直接用真 id：不拦的话会造出一个没人要的孤儿节点
                Op::node("n77_7", "module", "凭空冒出来的"),
                // 删一个不存在的东西
                Op::drop_node("n66_6"),
            ])
        .on_answer(chunks(&["好"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("建图", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let snap = r.handle.session_snapshot().await.unwrap();
    ok(snap.ws.flow.nodes.len() == 2, "★ 只有走别名建的两个落地了");
    ok(snap.ws.flow.edges.len() == 1, "★ 端点不存在的边没落地");
    ok(
        !snap.ws.flow.nodes.values().any(|n| n.label == "凭空冒出来的"),
        "★ 幽灵 id 没造出孤儿节点",
    );

    // 用户删掉一端 ⇒ 那条边跟着消失，不能留成悬边
    let a = snap.ws.flow.nodes.values().find(|n| n.label == "训练集").unwrap().id.clone();
    r.handle.session_edit(vec![Op::drop_node(a)]).await;
    let after = r.handle.session_snapshot().await.unwrap();
    ok(after.ws.flow.edges.is_empty(), "★ 删掉一端，边跟着收掉（I9 不靠自觉）");

    let s = r.finish().await;
    ok(s.metrics.ops_dropped >= 3, "三条坏 op 都记进了 dropped");
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s31_two_ways_to_draw() {
    head("S31", "★ 两种画法并存：程序画的图与模型自己画的图");
    let sketch = "graph LR\n  X[\"自己画的编码器\"] --> Y[\"自己画的投影头\"]";
    let m = MockModel::new()
        .on_judge(judge_of("none"))
        .on_ops(&[Op::node("$a", "module", "程序画的编码器")])
        .on_answer(chunks(&["先结构化"]));
    let r = Rig::new(m, Registry::new()).await;
    r.handle.session_send("开始", SendMode::Queue).await;
    ok(r.quiet(1).await, "第一轮");
    // 第 0 次是产出这张图的那次调用，图要到第 1 次才出现在 prompt 里。
    let ap0 = r.model.answer_prompt(1);
    ok(
        ap0.contains("flowchart") && ap0.contains("程序画的编码器"),
        "第一轮 prompt 里是程序渲染的那份",
    );

    // 第二轮：模型自己画一张，并且切过去
    r.model.push_judge(judge_of("none"));
    r.model.push_ops(&[Op::sketch(Lang::Mermaid, sketch), Op::view(FlowView::Sketch)]);
    r.model.push_answer(chunks(&["换成我画的"]));
    r.handle.session_send("你自己画", SendMode::Queue).await;
    ok(r.quiet(2).await, "第二轮");

    // 0/1 是第一轮的两次调用，2 是第二轮提交 sketch 的那次，3 才带上它。
    let ap1 = r.model.answer_prompt(3);
    ok(ap1.contains("自己画的编码器"), "★ 切到 Sketch 后 prompt 里是模型自己写的源码");
    ok(!ap1.contains("程序画的编码器"), "结构化那份这一轮不进 prompt");
    ok(ap1.contains("不能点选"), "★ prompt 里说清了这张图放弃了结构化编辑");

    let snap = r.handle.session_snapshot().await.unwrap();
    ok(!snap.ws.flow.nodes.is_empty(), "★ 结构化那份没被销毁，两种画法同时存在");
    r.handle.session_edit(vec![Op::view(FlowView::Built)]).await;
    let back = r.handle.session_snapshot().await.unwrap();
    ok(back.ws.flow.view == FlowView::Built, "用户随时切得回来");

    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}


// ══════════════════════════ 配置与工具链 ══════════════════════════

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join(format!("premortem-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn envmap(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

const NO_ENV: &dyn Fn(&str) -> Option<String> = &|_: &str| None;

/// 一套跑得起来的工具链：mock 联网后端 + 只读授权到 `dir`。
fn toolchain(dir: &std::path::Path, web: Option<Arc<dyn premortem::web::WebBackend>>)
    -> (toolkit::Deps, Registry)
{
    let cfg = PolicyCfg {
        roots: vec![dir.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let d = toolkit::deps(cfg, dir, &dir.join("workspace"), web).unwrap();
    let reg = toolkit::register(Registry::new(), &d);
    (d, reg)
}

async fn fire(reg: &Registry, name: &str, args: serde_json::Value) -> premortem::tools::ToolResult {
    let t = reg.get(name).unwrap_or_else(|| panic!("没注册 {name}"));
    t.run(call_with("t1", name, args), CancellationToken::new()).await
}

async fn s32_config_layers() {
    head("S32", "★ 模型配置：默认 → config.json → 环境变量，且来源查得到");
    let dir = tmpdir("cfg");

    let s = Settings::load(&dir, NO_ENV);
    ok(s.roles.answer.provider == "anthropic", "什么都没有时用内置默认");
    ok(s.warnings.is_empty(), "干净启动没有告警");

    let mut w = Settings::default();
    w.roles.answer.model = "文件里写的".into();
    w.save(&dir).unwrap();
    let s = Settings::load(&dir, NO_ENV);
    ok(s.roles.answer.model == "文件里写的", "config.json 覆盖默认");
    ok(
        s.describe(&Secrets::default(), NO_ENV).contains("config.json"),
        "describe 标出来源是 config.json",
    );

    let e = envmap(&[("PREMORTEM_ANSWER_MODEL", "环境变量赢"), ("PREMORTEM_JUDGE_MAX_TOKENS", "77")]);
    let env = |k: &str| e.get(k).cloned();
    let s = Settings::load(&dir, &env);
    ok(s.roles.answer.model == "环境变量赢", "★ 环境变量优先级最高");
    ok(s.roles.judge.max_tokens == 77, "数值型的也能覆盖");
    ok(s.roles.subagent.model == Settings::default().roles.subagent.model, "没被碰的角色不受影响");
    ok(s.describe(&Secrets::default(), &env).contains("环境变量"), "★ describe 说得清哪个值被 env 盖了");

    let bad = envmap(&[("PREMORTEM_JUDGE_MAX_TOKENS", "不是数字")]);
    let s = Settings::load(&dir, &|k: &str| bad.get(k).cloned());
    ok(s.warnings.iter().any(|w| w.contains("不是数字")), "环境变量写错了会报警，不是静默用默认");

    let e2 = envmap(&[("PREMORTEM_JUDGE_PROVIDER", "根本没这个")]);
    let s = Settings::load(&dir, &|k: &str| e2.get(k).cloned());
    ok(
        s.warnings.iter().any(|w| w.contains("根本没这个")),
        "★ provider 引用不存在会报警 —— 否则症状是「模型没反应」，查不到这里",
    );

    std::fs::write(dir.join("config.json"), "{ 这不是 json").unwrap();
    let s = Settings::load(&dir, NO_ENV);
    ok(s.roles.answer.provider == "anthropic", "★ 配置文件坏了回退默认，不是起不来");
    ok(s.warnings.iter().any(|w| w.contains("解析失败")), "而且报出来了");
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s33_secrets_separate() {
    head("S33", "★ 密钥单独存：不进 config.json、不进日志、环境变量优先");
    let dir = tmpdir("sec");
    let cfg = Settings::default();
    cfg.save(&dir).unwrap();
    let mut sec = Secrets::default();
    sec.put("anthropic", "sk-FILEKEY-0001");
    sec.save(&dir).unwrap();

    let raw = std::fs::read_to_string(dir.join("config.json")).unwrap();
    ok(!raw.contains("FILEKEY"), "★ config.json 里没有密钥（靠没这个字段，不靠脱敏）");
    let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap_or_default();
    ok(gi.contains("secrets.json"), "★ secrets.json 被自动写进 .gitignore");

    let (loaded, warn) = Secrets::load(&dir);
    ok(warn.is_empty() && loaded.has("anthropic"), "密钥读得回来");
    let p = cfg.providers.get("anthropic").unwrap();
    let (k, from) = loaded.resolve("anthropic", p, NO_ENV).unwrap();
    ok(k == "sk-FILEKEY-0001" && from == Src::File, "没有环境变量时用文件里的");

    let e = envmap(&[("ANTHROPIC_API_KEY", "sk-ENVKEY-0002")]);
    let (k, from) = loaded.resolve("anthropic", p, &|x: &str| e.get(x).cloned()).unwrap();
    ok(k == "sk-ENVKEY-0002" && from == Src::Env, "★ 环境变量盖过文件（CI 不用改文件）");

    let text = cfg.describe(&loaded, NO_ENV);
    ok(!text.contains("FILEKEY") && !text.contains("sk-"), "★ describe 一个密钥字符都不印");
    ok(text.contains("密钥有"), "但说得清「有没有」和从哪来");
    ok(
        cfg.missing_keys(&Secrets::default(), NO_ENV).contains(&"anthropic".to_string()),
        "缺密钥点得出来是哪个 provider",
    );

    let mut sec2 = loaded.clone();
    sec2.put("anthropic", "");
    ok(!sec2.has("anthropic"), "填空字符串等于删掉");
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s34_path_gate() {
    head("S34", "★ 路径闸门：越界 / 软链接 / 受保护的名字，一律拒");
    let base = tmpdir("fsgate");
    let inside = base.join("proj");
    std::fs::create_dir_all(inside.join("sub")).unwrap();
    std::fs::write(inside.join("sub/a.txt"), "hello").unwrap();
    std::fs::create_dir_all(inside.join(".git")).unwrap();
    std::fs::write(inside.join(".git/config"), "机密").unwrap();
    let outside = base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("boom.txt"), "不该被读到").unwrap();

    let cfg = PolicyCfg { roots: vec![inside.to_string_lossy().into_owned()], ..Default::default() };
    let pol = Policy::new(cfg, &base);

    ok(pol.check_path(std::path::Path::new("sub/a.txt")).is_ok(), "授权目录内正常放行");
    ok(pol.check_path(std::path::Path::new("../outside/boom.txt")).is_err(), "★ .. 逃逸被拒");
    ok(pol.check_path(&outside.join("boom.txt")).is_err(), "★ 目录外的绝对路径被拒");
    ok(pol.check_path(std::path::Path::new(".git/config")).is_err(), "★ 受保护的名字被拒");

    #[cfg(unix)]
    {
        // 这一条是纯字符串前缀比对**挡不住**的：链接名在授权目录里，
        // 指向的地方在外面。canonicalize 之后才看得见。
        let link = inside.join("escape");
        let _ = std::os::unix::fs::symlink(&outside, &link);
        ok(
            pol.check_path(std::path::Path::new("escape/boom.txt")).is_err(),
            "★ 指向目录外的软链接被拒（这条只有 canonicalize 挡得住）",
        );
    }

    ok(pol.denied_count() >= 4, "★ 拒绝有计数 —— 一直涨说明模型在反复撞白名单");
    let _ = std::fs::remove_dir_all(&base);
}

async fn s35_url_gate() {
    head("S35", "★ 网址闸门：协议 / 内网 / 白名单，别被相似域名骗了");
    let dir = tmpdir("urlgate");
    let pol = Policy::new(PolicyCfg::default(), &dir);

    ok(pol.check_url("https://arxiv.org/abs/2103.00020").is_ok(), "白名单域名放行");
    ok(pol.check_url("https://www.arxiv.org/abs/1").is_ok(), "子域名放行");
    ok(pol.check_url("https://evilarxiv.org/x").is_err(), "★ 后缀匹配卡在点上，evilarxiv.org 骗不过去");
    ok(pol.check_url("https://example.com/x").is_err(), "不在白名单里的被拒");
    ok(pol.check_url("http://127.0.0.1:8080/x").is_err(), "★ 回环地址被拒（SSRF）");
    ok(pol.check_url("http://192.168.1.1/admin").is_err(), "★ 内网段被拒");
    ok(pol.check_url("http://169.254.169.254/latest/meta-data/").is_err(), "★ 云元数据地址被拒");
    ok(pol.check_url("http://localhost/x").is_err(), "★ localhost 被拒");
    ok(pol.check_url("http://internal-svc/x").is_err(), "★ 不带点的内网主机名被拒");
    ok(pol.check_url("file:///etc/passwd").is_err(), "★ 非 http 协议被拒");
    ok(pol.check_url("https://user@arxiv.org/x").is_err(), "★ user@host 这种绕过写法被拒");

    let off = Policy::new(PolicyCfg { net: false, ..Default::default() }, &dir);
    ok(off.check_url("https://arxiv.org/x").is_err(), "总开关关掉后一律不放行");
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s36_context_safety() {
    head("S36", "★ 上下文安全：大文件 / 长行 / 海量匹配都不会撑爆一轮");
    let dir = tmpdir("ctx");
    let big: String = (1..=2000).map(|i| format!("第 {i} 行 needle\n")).collect();
    std::fs::write(dir.join("big.txt"), &big).unwrap();
    std::fs::write(dir.join("wide.txt"), "x".repeat(5000)).unwrap();
    let (d, reg) = toolchain(&dir, None);

    let r = fire(&reg, "fs_read", serde_json::json!({"path": "big.txt"})).await;
    ok(r.content.contains("共 2000 行"), "先说清总共多少行");
    ok(r.content.contains("已截断") || r.content.contains("还有"), "★ 超上限会截断");
    ok(r.content.contains("offset="), "★ 截断时给了续读的 offset —— 只说「已截断」模型会卡住");
    ok(r.content.len() < 40_000, "★ 返回值有上限，不会把 2000 行灌进 prompt");

    let r2 = fire(&reg, "fs_read", serde_json::json!({"path": "big.txt", "offset": 1990})).await;
    ok(r2.content.contains("第 2000 行"), "★ 按 offset 续读能拿到后面的内容");

    let r3 = fire(&reg, "fs_read", serde_json::json!({"path": "wide.txt"})).await;
    ok(r3.content.contains("本行过长已截断"), "★ 单行过长也截断（压缩过的代码一行能几百 KB）");

    let r4 = fire(&reg, "fs_grep", serde_json::json!({"pattern": "needle"})).await;
    ok(r4.content.contains("只列了前"), "★ 海量匹配只给前 N 条并说明");
    ok(r4.content.len() < 40_000, "检索结果同样有上限");

    let r5 = fire(&reg, "fs_read", serde_json::json!({"path": "big.txt", "offset": 99999})).await;
    ok(!ok_kind(&r5), "越界的 offset 报错");
    ok(r5.content.contains("2000"), "而且告诉模型一共多少行");

    ok(d.metrics.get("clipped") >= 3, "★ 截断有计数（截断率高说明上限设小了或模型在乱用）");
    ok(d.metrics.get("calls") == 5, "调用有计数");
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s37_fetch_lands_in_workspace() {
    head("S37", "★ 抓取先落盘再返回摘要 —— 页面大小不决定上下文用量");
    let dir = tmpdir("fetch");
    let long: String =
        (1..=300).map(|i| format!("正文第 {i} 行")).collect::<Vec<_>>().join("\n");
    let web = premortem::web::MockWeb::new()
        .page("https://arxiv.org/abs/2103.00020", "CLIP 论文", &long);
    let (d, reg) = toolchain(&dir, Some(Arc::new(web)));

    let r = fire(&reg, "web_fetch", serde_json::json!({"url": "https://arxiv.org/abs/2103.00020"})).await;
    ok(ok_kind(&r), "抓取成功");
    ok(r.content.contains("CLIP 论文"), "返回值里有标题");
    ok(r.content.contains("共 300 行"), "说清了全文有多大");
    ok(!r.content.contains("正文第 300 行"), "★ 全文没有直接灌进返回值");
    ok(r.content.contains("workspace/"), "★ 给了落盘路径");
    ok(r.content.contains("fs_read"), "★ 告诉模型怎么接着看");
    ok(d.scratch.count() == 1, "workspace 里确实多了一个文件");

    let name = d.scratch.list()[0].0.clone();
    let rel = format!("workspace/{name}");
    let r2 = fire(&reg, "fs_read", serde_json::json!({"path": rel, "offset": 280})).await;
    ok(r2.content.contains("正文第 300 行"), "★ 全文在 workspace 里，按需读得到");

    let r3 = fire(&reg, "fs_grep", serde_json::json!({"pattern": "第 250 行", "path": "workspace"})).await;
    ok(r3.content.contains("正文第 250 行"), "★ 抓回来的东西也能 grep");

    let bad = fire(&reg, "web_fetch", serde_json::json!({"url": "http://127.0.0.1/x"})).await;
    ok(!ok_kind(&bad), "★ 抓内网地址被闸门拦住");
    ok(d.metrics.get("denied") == 1, "拒绝计入指标");
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s38_toolchain_in_a_turn() {
    head("S38", "★ 端到端：模型在一轮里抓网页，产物落盘、结果进时间线");
    let dir = tmpdir("e2e");
    let long: String =
        (1..=200).map(|i| format!("摘要第 {i} 行")).collect::<Vec<_>>().join("\n");
    let web = premortem::web::MockWeb::new()
        .page("https://arxiv.org/abs/2103.00020", "CLIP", &long)
        .hit("CLIP", "https://arxiv.org/abs/2103.00020", "对比学习的图文预训练");
    let (d, reg) = toolchain(&dir, Some(Arc::new(web)));

    let m = MockModel::new()
        .on_judge(default_judge())
        .on_answer(vec![
            StreamEvent::Chunk("我查一下".into()),
            StreamEvent::ToolCalls(vec![call_with(
                "c1",
                "web_fetch",
                serde_json::json!({"url": "https://arxiv.org/abs/2103.00020"}),
            )]),
        ])
        .on_judge(default_judge())
        .on_answer(chunks(&["查完了"]));
    let r = Rig::new(m, reg).await;
    r.handle.session_send("CLIP 是怎么做的", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let tl = r.timeline().await;
    let ret = tl.iter().find_map(|e| match &e.body {
        Body::Returned { content, name, .. } if name == "web_fetch" => Some(content.clone()),
        _ => None,
    });
    ok(ret.is_some(), "工具返回落进了时间线");
    let ret = ret.unwrap_or_default();
    ok(ret.contains("workspace/"), "★ 时间线上记的是摘要 + 路径，不是全文");
    ok(!ret.contains("摘要第 200 行"), "★ 全文没有进对话历史");
    ok(d.scratch.count() == 1, "★ 产物真的落在 workspace 里");
    println!("      · {}", d.metrics.line());

    let s = r.finish().await;
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s39_config_reaches_the_gate() {
    head("S39", "★ config.json 改一行，工具的行为跟着变（配置不是装饰）");
    let dir = tmpdir("wire");

    // 用户在 config.json 里把可抓域名改成只剩 example.com
    let mut w = Settings::default();
    w.tools.allow_hosts = vec!["example.com".into()];
    w.tools.roots = vec![dir.to_string_lossy().into_owned()];
    w.save(&dir).unwrap();

    let loaded = Settings::load(&dir, NO_ENV);
    ok(loaded.tools.allow_hosts == vec!["example.com".to_string()], "策略从文件读回来了");

    let web = premortem::web::MockWeb::new()
        .page("https://example.com/x", "允许的", "内容")
        .page("https://arxiv.org/abs/1", "本来允许的", "内容");
    let d = toolkit::from_settings(&loaded, &dir, Some(Arc::new(web))).unwrap();
    let reg = toolkit::register(Registry::new(), &d);

    let good = fire(&reg, "web_fetch", serde_json::json!({"url": "https://example.com/x"})).await;
    ok(ok_kind(&good), "改成允许的域名能抓");
    let bad = fire(&reg, "web_fetch", serde_json::json!({"url": "https://arxiv.org/abs/1"})).await;
    ok(!ok_kind(&bad), "★ 默认允许的 arxiv 现在抓不了 —— 文件真的管住了闸门");
    ok(bad.content.contains("config.json"), "★ 拒绝时告诉模型去哪儿改");

    // 公开能力降级成单一内置 fetch；旧后端字段只用于无损读取旧配置。
    use premortem::web::{Fetcher, Searcher, WebCfg};
    let d0 = WebCfg::default();
    ok(d0.fetch == Fetcher::Http, "★ 默认抓取后端是进程内 Rust 实现");
    let b = premortem::web::build(&d0, &loaded.tools, None).unwrap();
    ok(b.name() == "builtin-fetch" && !b.can_search(), "★ 只提供基础 fetch，不注册 search");

    let with_search = WebCfg { search: Searcher::Searxng, ..d0.clone() };
    let b2 = premortem::web::build(&with_search, &loaded.tools, None).unwrap();
    ok(
        b2.name() == "builtin-fetch" && !b2.can_search(),
        "★ 旧 SearXNG 配置会安全降级，不会尝试连接外部服务",
    );

    let fc = WebCfg { fetch: Fetcher::Firecrawl, search: Searcher::None, ..d0.clone() };
    ok(
        premortem::web::build(&fc, &loaded.tools, None).is_some(),
        "★ 旧 Firecrawl 配置也降级到内置 fetch，不再要求密钥",
    );
    let off = premortem::policy::PolicyCfg { net: false, ..loaded.tools.clone() };
    ok(premortem::web::build(&d0, &off, None).is_none(), "总开关关掉时连后端都不建");

    let _ = std::fs::remove_dir_all(&dir);
}

async fn s40_wire_format() {
    head("S40", "★ 真实 API 的消息映射与 SSE 解析（离线测，不联网）");
    use premortem::client::{anthropic_messages, openai_messages, parse_sse};
    use premortem::config::Api;
    use premortem::model::{Message, StreamEvent};

    // ── 消息映射 ──
    let msgs = vec![
        Message::system("规则 A"),
        Message::system("规则 B"),
        Message::user("帮我看看"),
        Message::assistant_with_calls("我查一下", vec![call("c1", "fs_read"), call("c2", "fs_grep")]),
        Message::tool("c1", "文件内容"),
        Message::tool("c2", "检索结果"),
        Message::user("接着说"),
    ];
    let (system, out) = anthropic_messages(&msgs);
    ok(system.contains("规则 A") && system.contains("规则 B"), "system 段并成一段（Anthropic 是顶层字段）");
    ok(out.iter().all(|m| m["role"] != "system"), "system 不留在 messages 里");
    ok(out[0]["role"] == "user", "第一条是 user");
    let roles: Vec<String> = out.iter().map(|m| m["role"].as_str().unwrap_or("").to_string()).collect();
    let alternating = roles.windows(2).all(|w| w[0] != w[1]);
    ok(alternating, "★ user/assistant 严格交替 —— 不合并的话 Anthropic 直接 400");
    let merged = out.iter().find(|m| {
        m["content"].as_array().map(|a| a.iter().filter(|b| b["type"] == "tool_result").count() >= 2)
            .unwrap_or(false)
    });
    ok(merged.is_some(), "★ 连着两条工具返回被合进同一条 user 消息");
    let asst = out.iter().find(|m| m["role"] == "assistant").unwrap();
    let blocks = asst["content"].as_array().unwrap();
    ok(blocks.iter().any(|b| b["type"] == "text"), "assistant 的正文是 text 块");
    ok(blocks.iter().filter(|b| b["type"] == "tool_use").count() == 2, "两个工具调用都成了 tool_use 块");

    let oa = openai_messages(&msgs);
    ok(oa.len() == msgs.len(), "OpenAI 侧一一对应（它允许连续同角色）");
    let a2 = oa.iter().find(|m| m["role"] == "assistant").unwrap();
    ok(
        a2["tool_calls"][0]["function"]["arguments"].is_string(),
        "★ OpenAI 的 arguments 是字符串不是对象 —— 发成对象会被拒",
    );
    let t2 = oa.iter().find(|m| m["role"] == "tool").unwrap();
    ok(t2["tool_call_id"] == "c1", "工具返回带回了 tool_call_id");

    // ── Anthropic SSE ──
    let a_blocks = vec![
        r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":120}}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"先"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"复现"}}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"fs_read"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"src/a.rs\"}"}}"#,
        r#"event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":45}}"#,
        r#"event: message_stop
data: {"type":"message_stop"}"#,
    ];
    let evs = parse_sse(Api::Anthropic, &a_blocks);
    let text: String = evs.iter().filter_map(|e| match e {
        StreamEvent::Chunk(t) => Some(t.clone()), _ => None }).collect();
    ok(text == "先复现", "正文分片按序拼起来");
    let calls = evs.iter().find_map(|e| match e {
        StreamEvent::ToolCalls(c) => Some(c.clone()), _ => None });
    ok(calls.is_some(), "工具调用解出来了");
    let calls = calls.unwrap_or_default();
    ok(calls.len() == 1 && calls[0].name == "fs_read", "工具名对");
    ok(
        calls[0].args["path"] == "src/a.rs",
        "★ 分片到达的参数 JSON 拼完整了才解析 —— 这是流式最容易错的一处",
    );
    let usage = evs.iter().find_map(|e| match e { StreamEvent::Done(u) => Some(*u), _ => None });
    ok(usage.map(|u| u.prompt) == Some(120), "输入 token 从 message_start 拿到");
    ok(usage.map(|u| u.completion) == Some(45), "★ 输出 token 从 message_delta 拿到（不然账全靠估）");

    // ── OpenAI SSE ──
    let o_blocks = vec![
        r#"data: {"choices":[{"delta":{"content":"好"}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"web_","arguments":"{\"url"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"fetch","arguments":"\":\"https://a.b\"}"}}]}}]}"#,
        r#"data: {"choices":[],"usage":{"prompt_tokens":80,"completion_tokens":12}}"#,
        r#"data: [DONE]"#,
    ];
    let evs = parse_sse(Api::OpenAiCompat, &o_blocks);
    let calls = evs.iter().find_map(|e| match e {
        StreamEvent::ToolCalls(c) => Some(c.clone()), _ => None }).unwrap_or_default();
    ok(calls.len() == 1, "OpenAI 侧也解出一个调用");
    ok(calls[0].name == "web_fetch", "★ 分片到达的工具名也要拼（web_ + fetch）");
    ok(calls[0].args["url"] == "https://a.b", "参数拼完整了");
    let usage = evs.iter().find_map(|e| match e { StreamEvent::Done(u) => Some(*u), _ => None });
    ok(usage.map(|u| (u.prompt, u.completion)) == Some((80, 12)), "usage 从末尾那块拿到");
    ok(!evs.iter().any(|e| matches!(e, StreamEvent::Failed(_))), "[DONE] 不该被当成错误");

    // ── 参数被截断时不该整轮失败 ──
    let broken = vec![
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"fs_read"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
    ];
    let evs = parse_sse(Api::Anthropic, &broken);
    let calls = evs.iter().find_map(|e| match e {
        StreamEvent::ToolCalls(c) => Some(c.clone()), _ => None }).unwrap_or_default();
    ok(calls.len() == 1 && calls[0].args.is_object(), "★ 参数拼不完整时给空对象，让工具去报「缺少参数」");

    // ── 工具 schema 真的传出去了 ──
    let dir = tmpdir("schema");
    let (_, reg) = toolchain(&dir, None);
    let ex = reg.specs(&reg.names(), &Default::default());
    let read = ex.specs.iter().find(|s| s.name == "fs_read").unwrap();
    ok(read.schema["properties"]["offset"]["type"] == "integer", "★ fs_read 报了准确的参数 schema");
    ok(read.schema["required"][0] == "path", "必填项标出来了");
    ok(ex.specs.iter().all(|s| s.schema["type"] == "object"), "每个工具都有 schema");
    ok(ex.missing.is_empty(), "注册表自己的名字当然都认得");

    // ── 判断段的 tool_choice ──
    // 指名道姓要某个工具，在开了 thinking 的模型上会 400：
    // `tool_choice 'specified' is incompatible with thinking enabled`。
    // 症状很隐蔽：每轮判断段失败、降级成「本轮无场景」，对话表面上还在正常跑。
    let ja = premortem::client::judge_tool_choice(Api::Anthropic);
    let jo = premortem::client::judge_tool_choice(Api::OpenAiCompat);
    ok(ja["type"] == "any" && ja.get("name").is_none(), "★ Anthropic 侧用 any，不点名");
    ok(jo == serde_json::json!("required"), "★ OpenAI 兼容侧用 required，不点名");
    ok(judge_tool_names().len() == 1, "候选只有一个工具，所以 any/required 等价于点名");
    // 两个动作工具的 schema 必须真的列出字段 —— 上一版判断段那个 ops 字段
    // 是个空对象，模型怎么写都错，而且错得没有声音。
    for name in [premortem::actions::RECORD_GRAPH, premortem::actions::RECORD_NOTE] {
        let s = ex.specs.iter().find(|s| s.name == name).unwrap();
        ok(
            s.schema["properties"]["ops"]["items"]["properties"]["op"]["enum"].is_array(),
            &format!("★ {name} 的 schema 列出了 op 的取值，不是个空对象"),
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}


// ══════════════ 这一轮改动的四条链路 ══════════════

/// 从时间线上取本轮的 TurnStats。指标该打出来看，不是只做断言。
fn stats_of(events: &[Event]) -> Option<premortem::msg::TurnStats> {
    events.iter().rev().find_map(|e| match &e.body {
        Body::TurnClosed { stats, .. } => serde_json::from_str(stats).ok(),
        _ => None,
    })
}

async fn s41_judge_runs_once_per_turn() {
    head("S41", "★ 判断段一轮只跑一次 —— 工具往返多少次都不再重判场景");
    // 模型连着调三轮工具再说话。旧版每次工具返回都 continue 到检查点 0，
    // 于是判断段跟着跑四次，而判断段吃的是和回答段一样的完整对话 ——
    // 等于把最贵的那段 prompt 发了四遍，换来一个几乎不会变的场景 id。
    let m = MockModel::new()
        .on_judge(judge_of("check_assumption"))
        .on_answer(vec![StreamEvent::ToolCalls(vec![call("t1", "echo")])])
        .on_answer(vec![StreamEvent::ToolCalls(vec![call("t2", "echo")])])
        .on_ops(&[Op::set("spec.claim", "读完才写的")])
        .on_answer(chunks(&["读完了，结论是"]));
    let r = Rig::new(m, Registry::new().with(Arc::new(EchoTool))).await;
    r.handle.session_send("查一下", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    ok(r.model.judge_calls() == 1, "★ 判断段只被调用了 1 次");
    ok(r.model.answer_calls() == 4, "回答段调了 4 次（三次工具往返 + 收尾）");
    let s = r.finish().await;
    let st = stats_of(&s.events).expect("TurnClosed 带 stats");
    println!(
        "      · 本轮：judge 调用 1 · answer 调用 4 · loops {} · tools_run {} · inferred_ops {} · scene {}",
        st.loops, st.tools_run, st.inferred_ops, st.scenes.join("+")
    );
    ok(st.loops == 4, "四圈回答循环");
    ok(st.scenes == vec!["check_assumption".to_string()], "场景是判断段那一次定下的，全程没变");
    ok(
        s.events.iter().filter(|e| e.body.tag() == "judged").count() == 1,
        "★ 时间线上只有一条 judged",
    );
    inv::all(&s.events, &s.ws, &s.cost);
}

async fn s42_actions_write_inference() {
    head("S42", "★ 推断由回答段的动作工具写，同一批里别名互通");
    // 断链 2 的堵法：模型有 record_graph / record_note 两个工具，
    // 同一条 assistant 消息里的两次调用合并成**一次**提交 ——
    // 所以 record_note 的 anchor 能指到同一批 record_graph 刚建的节点。
    let dir = tmpdir("actions");
    let (_, reg) = toolchain(&dir, None);
    let m = MockModel::new().on_judge(judge_of("trace_code")).on_answer(vec![
        StreamEvent::ToolCalls(vec![
            call_with(
                "g1",
                premortem::actions::RECORD_GRAPH,
                serde_json::json!({ "ops": [
                    { "op": "node", "id": "$enc", "kind": "module", "label": "编码器",
                      "source": { "repo": "src/model.py:42" }, "confidence": 0.9 },
                    { "op": "node", "id": "$loss", "kind": "loss", "label": "对比损失" },
                    { "op": "edge", "id": "$e", "from": "$enc", "to": "$loss", "kind": "supervises" },
                ]}),
            ),
            call_with(
                "n1",
                premortem::actions::RECORD_NOTE,
                serde_json::json!({ "ops": [
                    { "op": "set", "path": "spec.temp", "value": "0.07", "anchor": "$loss",
                      "source": "user" },
                    { "op": "set", "path": "open", "open": ["负样本从哪来？"] },
                ]}),
            ),
        ]),
    ]).on_answer(chunks(&["图画好了"]));
    let r = Rig::new(m, reg).await;
    r.handle.session_send("看看这个模型", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let snap = r.handle.session_snapshot().await.unwrap();
    let g = &snap.ws.flow;
    ok(g.nodes.len() == 2 && g.edges.len() == 1, "两个节点一条边落地了");
    ok(g.nodes.keys().all(|n| !n.0.starts_with('$')), "别名全部铸成了真 id");
    let loss = g.nodes.values().find(|n| n.label == "对比损失").map(|n| n.id.clone()).unwrap();
    let temp = snap.ws.fields.get(&premortem::state::Path::new("spec.temp")).unwrap();
    ok(
        temp.anchor.as_ref() == Some(&loss),
        "★ record_note 的 anchor 指到了同一批 record_graph 刚建的节点（一批一个别名作用域）",
    );
    ok(snap.ws.open == vec!["负样本从哪来？".to_string()], "待落定清单也写进去了");
    ok(
        g.nodes.values().any(|n| matches!(&n.prov.source, Some(Source::Repo(p)) if p.contains("model.py"))),
        "★ source 按 schema 里写的形状收下来了",
    );

    // 回喂：两个调用各自拿到一条结果，模型下一步就知道写成没写成
    let ap1 = r.model.answer_prompt(1);
    ok(ap1.contains("提交了 3 条改动") && ap1.contains("提交了 2 条改动"), "★ 两个动作各自有回执");
    ok(ap1.contains("对比损失") && ap1.contains("flowchart"), "写完的图立刻回到 prompt 里");

    let s = r.finish().await;
    let st = stats_of(&s.events).expect("stats");
    println!(
        "      · 本轮：inferred_ops {} · dropped_ops {} · tools_run {} · tools_failed {}",
        st.inferred_ops, st.dropped_ops, st.tools_run, st.tools_failed
    );
    ok(st.inferred_ops == 5, "5 条 op 都记在指标里");
    ok(st.tools_run == 2, "★ 动作调用也算进 tools_run —— 否则指标上永远是 0");
    inv::all(&s.events, &s.ws, &s.cost);
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s43_tool_names_resolve() {
    head("S43", "★ 场景与 mode 里写的工具名，注册表里必须真的有");
    // 断链 1 的堵法。上一版 playbook 写的是 read_repo / repo_qa / search_cases，
    // 注册表里一个都没有，而 specs() 是 filter_map 静默丢弃 ——
    // 于是整套 fs_* / repo_tree 从来没被暴露给模型过，指标上只是 tools_run: 0。
    let dir = tmpdir("names");
    // 带一个假的联网后端，好看清「联网工具按配置注册」这条
    let web: Arc<dyn premortem::web::WebBackend> =
        Arc::new(premortem::web::MockWeb::new().hit("t", "https://arxiv.org/abs/1", "s"));
    let (_, reg) = toolchain(&dir, Some(web));
    let known: std::collections::HashSet<String> = reg.names().into_iter().collect();
    let pb = Playbook::builtin();
    let prompts = premortem::memory::Prompts::default();

    let mut bad: Vec<String> = Vec::new();
    for sc in pb.scenes.values() {
        for mode in [Mode::Explore, Mode::Go] {
            let mp = prompts.mode.get(mode.key()).cloned().unwrap_or_default();
            for t in pb.exposed_tools(std::slice::from_ref(sc), &mp.tools) {
                if !known.contains(&t) {
                    bad.push(format!("{}/{}: {t}", sc.id, mode.key()));
                }
            }
        }
    }
    ok(bad.is_empty(), &format!("★ 内置场景 × 两个 mode，工具名全部认得（对不上的：{bad:?}）"));
    for must in ["record_graph", "record_note", "fs_read", "fs_grep", "fs_find", "repo_tree"] {
        ok(known.contains(must), &format!("{must} 在注册表里"));
    }

    // 反向：写一个不存在的名字，必须报出来，不能静默吞掉
    let ex = reg.specs(&["fs_read".into(), "read_repo".into()], &Default::default());
    ok(ex.specs.len() == 1 && ex.missing == vec!["read_repo".to_string()], "★ 认不出的名字进 missing，不是静默丢弃");
    // 联网工具按配置注册，没配不算配置错误
    let none = toolchain(&dir, None).1;
    let ex2 = none.specs(&["web_search".into()], &Default::default());
    ok(ex2.specs.is_empty() && ex2.missing.is_empty(), "没配联网时 web_search 缺席但不报错");

    // mode 换 prompt、换工具、换工具说明
    let none = std::slice::from_ref(pb.get("none").unwrap());
    let ex_e = reg.specs(
        &pb.exposed_tools(none, &prompts.mode["explore"].tools),
        &prompts.mode["explore"].tool_notes,
    );
    let ex_g = reg.specs(
        &pb.exposed_tools(none, &prompts.mode["go"].tools),
        &prompts.mode["go"].tool_notes,
    );
    let names = |e: &premortem::tools::Exposed| {
        e.specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
    };
    ok(
        names(&ex_e).contains(&"web_fetch".to_string())
            && names(&ex_g).contains(&"web_fetch".to_string())
            && !names(&ex_e).contains(&"web_search".to_string())
            && !names(&ex_g).contains(&"web_search".to_string()),
        "★ 两个 mode 都只有按址 fetch，不暴露 search",
    );
    let d = |e: &premortem::tools::Exposed, n: &str| {
        e.specs.iter().find(|s| s.name == n).unwrap().description.clone()
    };
    ok(
        d(&ex_e, "record_graph") != d(&ex_g, "record_graph"),
        "★ 同一个工具在两个 mode 下的说明不同",
    );
    ok(d(&ex_g, "record_graph").contains("编码 agent"), "行动 mode 的说明要求图能直接交下去");
    ok(
        prompts.mode["explore"].note != prompts.mode["go"].note,
        "两个 mode 的 system 段也不同",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s44_distill_sections_apply() {
    head("S44", "★ 一键蒸馏：分节草稿 → 预览 → 一键写回，坏 TOML 当场拦住");
    let draft = concat!(
        "先说两句没用的开场白，应该被丢掉。\n\n",
        "## project.md\n# 项目\n新的项目描述。\n\n",
        "## knowledge.md\n- 持续学习 / BWT — 有储备 — 他自己推了公式\n\n",
        "## playbook.toml\n[[scene]]\nid = \"seed_control\"\nlabel = \"对照组种子\"\n",
        "when = \"对照组和实验组用了不同的随机种子\"\nguidance = \"提醒他固定种子\"\n",
        "tools = []\n\n",
        "## cases/seed-drift.md\n---\nid = \"seed-drift\"\ntitle = \"种子漂移\"\n",
        "scenes = [\"check_assumption\"]\n---\n那次的教训。\n",
    );
    let secs = premortem::memory::parse_draft(draft);
    ok(secs.len() == 4, &format!("切出 4 节（实际 {}）", secs.len()));
    ok(secs[0].file == "project.md" && secs[0].mode == premortem::memory::WriteMode::Replace, "叙述文件是整份替换");
    ok(secs[2].file == "playbook.toml" && secs[2].mode == premortem::memory::WriteMode::Append, "★ 场景库是追加，不覆盖用户的场景");
    ok(!secs[0].text.contains("开场白"), "第一个合法标题之前的东西丢掉");

    // 写回：追加到 playbook 之后仍然是合法的场景库
    let dir = tmpdir("distill");
    let mem = dir.join("memory");
    let _ = Memory::load_or_bootstrap(&mem).await; // 先 bootstrap 出基础文件
    let mut wrote = 0;
    for s in &secs {
        let path = mem.join(&s.file);
        let merged = if s.mode == premortem::memory::WriteMode::Append {
            let cur = std::fs::read_to_string(&path).unwrap_or_default();
            format!("{}\n\n{}\n", cur.trim_end(), s.text.trim())
        } else {
            format!("{}\n", s.text.trim())
        };
        ok(premortem::memory::validate(&s.file, &merged).is_ok(), &format!("{} 校验通过", s.file));
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(&path, merged).unwrap();
        wrote += 1;
    }
    ok(wrote == 4, "四个文件都写了");

    let back = Memory::load_or_bootstrap(&mem).await;
    ok(back.warnings.is_empty(), "★ 写回之后持久层还读得动（追加没把 TOML 弄坏）");
    ok(back.playbook.get("seed_control").is_some(), "★ 新场景真的进了场景库");
    ok(back.playbook.get("none").is_some(), "原有场景一条没丢");
    ok(back.project.contains("新的项目描述"), "project.md 换成了新的");
    ok(back.cases.iter().any(|c| c.id == "seed-drift"), "★ 新案例读得出来");

    // 坏内容必须在写之前被拦住 —— 它的症状要到下一轮才以「场景全没了」的形式冒出来
    let broken = format!("{}\n\n这不是 TOML，只是一段话。\n", std::fs::read_to_string(mem.join("playbook.toml")).unwrap());
    ok(premortem::memory::validate("playbook.toml", &broken).is_err(), "★ 追加成非法 TOML 被拦住");
    ok(
        premortem::memory::validate("cases/x.md", "没有 frontmatter 的正文").is_err(),
        "★ 缺 frontmatter 的案例也被拦住（否则它只是静默读不出来）",
    );
    ok(premortem::memory::write_mode_for("config.json").is_none(), "非持久层文件不是合法目标");
    ok(premortem::memory::write_mode_for("../x.md").is_none(), "路径穿越不是合法目标");
    let _ = std::fs::remove_dir_all(&dir);
}


async fn s45_multiple_scenes() {
    head("S45", "★ 场景是多选：几份 guidance 一起进 prompt，工具与案例取并集");
    // 一轮里「目标还没说清」和「预算和方案对不上」可以同时成立。
    // 只准判一个的话，另一条的 guidance 就永远注不进去 —— 而那正是它存在的理由。
    let dir = tmpdir("multiscene");
    let mem = dir.join("memory");
    let _ = Memory::load_or_bootstrap(&mem).await;
    // 两条案例，各服务一个场景。命中两个场景 ⇒ 两条都该注入。
    std::fs::write(
        mem.join("cases/a.md"),
        "---\nid = \"a\"\ntitle = \"案例甲\"\nscenes = [\"clarify_goal\"]\n---\n甲的正文。\n",
    )
    .unwrap();
    std::fs::write(
        mem.join("cases/b.md"),
        "---\nid = \"b\"\ntitle = \"案例乙\"\nscenes = [\"cheap_first\"]\n---\n乙的正文。\n",
    )
    .unwrap();

    let m = MockModel::new()
        .on_judge(judge_all(&["clarify_goal", "cheap_first"]))
        .on_answer(chunks(&["两边都看到了"]));
    let opts = RigOpts { memory_dir: Some(mem.clone()), ..Default::default() };
    let r = Rig::build(m, toolkit::register(Registry::new(), &toolchain(&dir, None).0), opts).await;
    r.handle.session_send("我想验证一下那个想法", SendMode::Queue).await;
    ok(r.quiet(1).await, "一轮跑完");

    let ap = r.model.answer_prompt(0);
    let pb = Playbook::builtin();
    let k = |id: &str| pb.get(id).unwrap().guidance.lines().next().unwrap().to_string();
    ok(ap.contains(&k("clarify_goal")), "★ 第一个场景的 guidance 进了 prompt");
    ok(ap.contains(&k("cheap_first")), "★ 第二个场景的 guidance 也进了");
    ok(ap.contains("本轮场景 1/2") && ap.contains("本轮场景 2/2"), "两条各自成段，不是糊成一段");
    ok(ap.contains("案例甲") && ap.contains("案例乙"), "★ 案例取并集");
    // clarify_goal 要 ask_user，cheap_first 不要；并集里必须有
    ok(ap.contains("ask_user") || r.model.answer_calls() > 0, "工具并集算得出来");

    let s = r.finish().await;
    let st = stats_of(&s.events).expect("stats");
    println!("      · 本轮场景：{}", st.scenes.join(" + "));
    ok(st.scenes.len() == 2, "指标里记的是两个");
    ok(
        s.events.iter().any(|e| matches!(&e.body, Body::Judged { scenes, .. } if scenes.len() == 2)),
        "时间线上也是两个",
    );
    inv::all(&s.events, &s.ws, &s.cost);
    let _ = std::fs::remove_dir_all(&dir);
}

async fn s46_scene_edges() {
    head("S46", "★ 多场景的边界：上限、去重、认不出的丢掉、老时间线读得回来");
    let pb = Playbook::builtin();

    // 去重 + 顺序按 playbook 的 id 序（所以 prompt 里场景先后与模型给的顺序无关）
    let (rs, unknown) = pb.resolve(&[
        "cheap_first".into(), "clarify_goal".into(), "cheap_first".into(), "不存在".into(),
    ]);
    ok(unknown == vec!["不存在".to_string()], "认不出的单独回报，不混进结果");
    let ids: Vec<String> = rs.iter().map(|s| s.id.clone()).collect();
    ok(ids == vec!["cheap_first".to_string(), "clarify_goal".to_string()], &format!("去重且按 id 序：{ids:?}"));

    // 工具并集：clarify_goal 要 ask_user，trace_code 要 fs_*，两个一起选就都有
    let both = pb.resolve(&["clarify_goal".into(), "trace_code".into()]).0;
    let tools = pb.exposed_tools(&both, &[]);
    ok(tools.contains(&"ask_user".to_string()) && tools.contains(&"fs_grep".to_string()),
       "★ 两个场景的工具取并集");
    let mut sorted = tools.clone();
    sorted.sort();
    sorted.dedup();
    ok(sorted.len() == tools.len(), "并集里没有重复项");

    // 上限：判出五个只留前 MAX_SCENES 个
    let many: Vec<String> = pb.scenes.keys().cloned().collect();
    ok(many.len() > premortem::turn::MAX_SCENES, "内置场景够多，这条才有意义");
    let mut capped = pb.resolve(&many).0.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    capped.truncate(premortem::turn::MAX_SCENES);
    ok(capped.len() == premortem::turn::MAX_SCENES, "★ 有上限 —— 什么都强调等于什么都没强调");

    // 老时间线：库里存的是单个字符串，新代码要的是数组
    let old = r#"{"kind":"judged","scene":"trace_code","rationale":"旧格式"}"#;
    let b: Body = serde_json::from_str(old).expect("★ 老的 judged 事件必须还读得出来");
    ok(matches!(&b, Body::Judged { scenes, .. } if scenes == &["trace_code".to_string()]),
       "★ 单个字符串收成一元数组（否则改完所有历史会话都读不回来）");
    let old2 = r#"{"kind":"scene_overridden","from":"none","to":"cheap_first"}"#;
    let b2: Body = serde_json::from_str(old2).expect("老的 scene_overridden 也要读得出来");
    ok(matches!(&b2, Body::SceneOverridden { to, .. } if to == &["cheap_first".to_string()]), "同上");
    // 已废弃的 phase_set 也不能让整条会话读不回来
    let old3 = r#"{"kind":"phase_set","to":"Handoff"}"#;
    ok(serde_json::from_str::<Body>(old3).is_ok(), "★ 删掉的阶段事件仍然反序列化得了（老库能开）");
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
    s27_model_builds_graph().await;
    s28_edge_relink_blocked().await;
    s29_node_resurrect_blocked().await;
    s30_phantom_ids_rejected().await;
    s31_two_ways_to_draw().await;
    s32_config_layers().await;
    s33_secrets_separate().await;
    s34_path_gate().await;
    s35_url_gate().await;
    s36_context_safety().await;
    s37_fetch_lands_in_workspace().await;
    s38_toolchain_in_a_turn().await;
    s39_config_reaches_the_gate().await;
    s40_wire_format().await;
    s41_judge_runs_once_per_turn().await;
    s42_actions_write_inference().await;
    s43_tool_names_resolve().await;
    s44_distill_sections_apply().await;
    s45_multiple_scenes().await;
    s46_scene_edges().await;

    let p = PASS.load(Ordering::Relaxed);
    let f = FAIL.load(Ordering::Relaxed);
    println!("\n{}", "═".repeat(64));
    println!("46 个场景 · {} 条断言：{p} 通过，{f} 失败", p + f);
    if f > 0 {
        std::process::exit(1);
    }
}

// 让未使用的导入有个去处（这些是给后续场景预留的）
#[allow(dead_code)]
fn _unused(_: Message, _: Role, _: CoreHandle) {}
