//! `Core`：SessionActor。**唯一写权、唯一 seq 分配者**，全同步处理，微秒级返回。
//!
//! # 读这个文件时请守住一件事
//!
//! 每一个 match 臂都只做「追加事件 + 发 UI 事件 + 丢一个写盘任务」。
//! 它们**不含流程**——流程全在 [`crate::turn::run_turn`] 里。
//! 一旦某个臂里出现了 `.await`，或者出现了「先做 A 再做 B 然后等 C」，
//! 说明流程漏进来了，即时打断就会失效。
//!
//! 需要等落盘回执的地方（用户输入的 ack）用一个一次性小 task 转发，
//! 见 [`Core::ack_later`] —— Core 自己绝不 await。
//!
//! # Core 不认识存储
//!
//! 它一次也不读 `Store`，只往 writer 的 channel 里丢活。存储归 app 持有：
//! 一份给 writer，一份给启动恢复，一份给 UI 分页读。这条边界让 Core 的
//! 「不做磁盘 IO」不是靠自觉，而是靠拿不到那个对象。
//!
//! # 状态机只负责两件事
//!
//! **状态的稳定维持**（seq 分配、事务边界、崩溃恢复）和
//! **模型与 workspace 之间的稳定接口**（输入 / 追加 / 工具调用 / 插话）。
//! 它不负责「这一步该做什么」—— 那是模型的工作，见 [`crate::scene`]。
//!
//! # 这一版删掉了什么
//!
//! - **`Pending`**：模型的改动不再攒到轮末，当场生效。turn 只是回滚粒度。
//! - **`touched: HashMap<Path, Version>`**：跨轮累积、只增不减、且是
//!   `Field.version + origin` 的冗余副本。换成 [`Core::turn_edits`] ——
//!   一个 turn 内清空一次的 path 集合，作用域和它声称的一致。
//! - **`Version`**：三重身份的计数器。只剩 [`Seq`]，是位置不是版本。
//! - **`ws.scene` / `ws.scene_overrides`**：流水混进了状态，重启后 UI 会显示
//!   上次会话的场景。现在是时间线上的事件。

use crate::context::{ContextLimit, toks};
use crate::cost::CostLedger;
use crate::event::{Body, Draft, Event, now_ms};
use crate::handle::CoreHandle;
use crate::ids::{Seq, Session, SessionId, TurnId};
use crate::memory::Memory;
use crate::model::{Message, Mode, Models, Role, complete};
use crate::msg::{
    Ack, Applied, CoreMsg, Emit, Injected, OpenQuestion, SendMode, Snap, TurnView, UiEvent,
    WriteJob,
};
use crate::persist::{Restored, seal};
use crate::scene::SceneId;
use crate::state::{self, Op, Path, Workspace};
use crate::tools::{Registry, ToolConfig};
use crate::turn::{TurnCtx, run_turn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// turn 的三态。`Closing` 是**打断的两段式反馈**中间那一段：
/// 已经 cancel 了、但子任务还没收敛完。UI 在这段显示「正在停止」。
enum TurnPhase {
    Idle,
    Running { id: TurnId, token: CancellationToken },
    Closing { id: TurnId },
}

/// Core 的可观测指标。
///
/// 前三个是**异常指标**，正常跑一轮应该全是 0；不是 0 就说明有 bug 或有竞态。
#[derive(Debug, Clone, Default)]
pub struct CoreMetrics {
    /// 收到了对不上代际的消息。打断时偶发正常（子任务还在回流）；
    /// **持续增长说明代际判断漏了地方**。
    pub stale_msgs_dropped: u64,
    /// 用户在收敛期间说的话没被垂死的 turn 吞掉（正确行为）。
    /// 但它一直涨而 `inbox_taken` 不涨，说明 turn 卡在 Closing 出不来。
    pub injections_refused_closing: u64,
    /// 模型的改动撞上了用户本轮的编辑而被丢弃。
    /// **频繁出现说明用户在轮中改得很勤**，可以考虑缩短单轮时长而不是改仲裁策略。
    pub ops_dropped: u64,

    pub events: u64,
    pub turns_started: u64,
    pub turns_aborted: u64,
    pub user_inputs: u64,
    pub deduped_inputs: u64,
    pub answers: u64,
    /// 从 inbox 取走的消息总数。**包含每轮的首条输入**，不只是中途插话。
    pub inbox_taken: u64,
    pub ops_user: u64,
    pub ops_model: u64,
    pub phase_advances: u64,
    /// 用户一键更换场景的次数。**这是场景描述写得准不准的直接反馈。**
    pub scene_overrides: u64,
    pub compactions: u64,
    pub msgs_folded: u64,
    pub distills: u64,
    /// 用户从某一轮分叉出新会话的次数。**这是「这条路不对」的直接信号。**
    pub forks: u64,
    pub checkpoints: u64,
    /// 启动时补进去的修补事件（未闭合调用、开了没关的轮次）。
    pub repairs: u64,
}

impl CoreMetrics {
    pub fn line(&self) -> String {
        format!(
            "事件 {} / turns {}(aborted {}) / 输入 {}(去重 {}, 回答 {}) / 取走 {} / 换场景 {} / \
             折叠 {}次·{}条 / 分叉 {} / checkpoint {} / 修补 {} / ops: user {} model {} / \
             异常: stale_msg {} refuse_inj {} dropped_op {}",
            self.events,
            self.turns_started,
            self.turns_aborted,
            self.user_inputs,
            self.deduped_inputs,
            self.answers,
            self.inbox_taken,
            self.scene_overrides,
            self.compactions,
            self.msgs_folded,
            self.forks,
            self.checkpoints,
            self.repairs,
            self.ops_user,
            self.ops_model,
            self.stale_msgs_dropped,
            self.injections_refused_closing,
            self.ops_dropped,
        )
    }
}

pub struct CoreDeps {
    /// 判断 / 回答 / subagent 三个角色分别可配（R3）。
    pub models: Arc<Models>,
    pub registry: Arc<Registry>,
    /// 这个 Core 服务哪个会话。**一个 Core 一个 session。**
    ///
    /// 分叉出来的新会话由调用方另起一个 Core 打开 —— 让一个 Core 同时管两条
    /// 时间线，就等于把「唯一写权」变成「两个写权」，seq 也会立刻打架。
    pub session: SessionId,
    /// 持久层目录。**每轮重读**，所以用户改了文件下一轮立刻生效。
    /// None 表示不落盘，用 `memory` 兜底。
    pub memory_dir: Option<PathBuf>,
    /// 没有 `memory_dir` 时用的持久层。
    pub memory: Arc<Memory>,
    pub context_limit: ContextLimit,
    pub tool_config: ToolConfig,
    pub mode: Mode,
    pub writer: mpsc::UnboundedSender<WriteJob>,
    /// 启动恢复的结果。None = 全新会话。
    pub restored: Option<Restored>,
    /// 每几条事件打一个 checkpoint（turn 边界总是打一个）。
    pub checkpoint_every: u64,
    pub core_capacity: usize,
    pub ui_capacity: usize,
    /// UI 广播通道。
    ///
    /// **必须和 writer 共用一条**：writer 的降级提示（`PersistDegraded`）
    /// 是发给同一批订阅者的。各建各的，那条提示就永远到不了界面 ——
    /// 而「落盘挂了用户得知道」正是它存在的全部意义。
    pub ui: Option<broadcast::Sender<UiEvent>>,
}

impl CoreDeps {
    pub fn new(
        models: Arc<Models>,
        registry: Arc<Registry>,
        session: SessionId,
        memory: Arc<Memory>,
        writer: mpsc::UnboundedSender<WriteJob>,
        mode: Mode,
    ) -> Self {
        Self {
            models,
            registry,
            session,
            memory_dir: None,
            memory,
            context_limit: ContextLimit::default(),
            tool_config: ToolConfig::default(),
            mode,
            writer,
            restored: None,
            checkpoint_every: 200,
            core_capacity: 64,
            ui_capacity: 4096,
            ui: None,
        }
    }
}

pub struct Started {
    pub handle: CoreHandle,
    pub ui: broadcast::Receiver<UiEvent>,
    pub join: tokio::task::JoinHandle<CoreSummary>,
}

pub struct CoreSummary {
    pub session: SessionId,
    pub metrics: CoreMetrics,
    pub cost: CostLedger,
    pub ws: Workspace,
    pub seq: Seq,
    pub events: Arc<Vec<Event>>,
}

pub struct Core {
    /// 这个 Core 服务的会话。所有它写出去的事件都盖这个章。
    session: SessionId,
    /// 时间线的物化结果。**不是第二份真相** —— 丢掉从头重放必须得到同一份。
    ws: Workspace,
    /// 下一条事件的位置由它 bump 出来。Core 是唯一分配者。
    seq: Seq,
    /// 完整时间线。`Arc::make_mut` 追加：turn 持有期间第一次追加复制一次，
    /// 之后引用计数回到 1 又变成原地追加。**一轮至多复制一次。**
    events: Arc<Vec<Event>>,

    turn: TurnPhase,
    /// 本轮起点。UI 与 prompt 用 `field.seq > turn_start` 判断「本轮新增」，
    /// 这是删掉 `Pending` 之后高亮的依据。
    turn_start: Seq,
    /// **turn 内仲裁的全部机器。**
    ///
    /// 本轮里用户 apply 过的路径。模型要改这些路径就丢弃，时间线上只记生效的部分。
    /// 每轮开头清空 —— 仲裁的作用域就是一个 turn，跨轮没有意义
    /// （跨轮的保护是 prompt 里的 `[用户设定]` 标记，不是硬拒绝）。
    turn_edits: HashSet<Path>,
    /// 发出了还没返回的工具调用：`call_id → 那条 Called 的 seq`。
    ///
    /// 这就是「小生命周期」：返回事件靠它连回发出事件；turn 收尾时它非空就说明
    /// 有调用被截断，逐个补 `Aborted`。不需要额外的表。
    open_calls: HashMap<String, Seq>,
    open_questions: Vec<OpenQuestion>,
    /// 当前场景，派生自最近一条 `Judged` / `SceneOverridden`。不进 Workspace。
    scene: Option<SceneId>,
    /// 本轮的视图有没有被取过。`scene_override` 只在第一次给出，之后就消费掉了。
    view_taken: bool,
    /// 收到了退出请求、正在等当前 turn 收尾。
    ///
    /// **退出必须等 turn 把东西吐完**：cancel 之后 turn 还要落半截正文、
    /// 补未闭合的调用、发 `Finished`。上一版收到 Shutdown 就直接跳出循环，
    /// 于是这些全发给了一个已经没人接的 Core —— 用户看着正文吐了一半退出应用，
    /// 回来一片空白，正是这条路径。
    shutdown: Option<oneshot::Sender<()>>,
    /// 用户点了换场景，等下一轮生效。
    ///
    /// **不在轮中生效**：轮中改会重新引入「同一份输入在轮中变了」那类 bug，
    /// 而那正是这一版要消灭的。用户想立刻换就打断。
    pending_scene: Option<SceneId>,

    /// 待处理的用户输入。**存的就是时间线上那几条事件本身**，不是一份副本。
    inbox: VecDeque<Event>,
    /// 已见过的 client_id。启动时用整条链预热，所以跨重启的重发也挡得住。
    /// 一次会话的用户输入条数天然有界，不需要定容淘汰。
    seen: HashSet<String>,

    ui: broadcast::Sender<UiEvent>,
    cost: CostLedger,
    writer: mpsc::UnboundedSender<WriteJob>,
    self_tx: mpsc::Sender<CoreMsg>,
    models: Arc<Models>,
    registry: Arc<Registry>,
    memory: Arc<Memory>,
    memory_dir: Option<PathBuf>,
    context_limit: ContextLimit,
    tool_config: ToolConfig,
    mode: Mode,
    next_turn: u64,
    /// TaskId 的分配器。turn 侧直接取号，省一次到 Core 的往返。
    next_task: Arc<AtomicU64>,
    checkpoint_every: u64,
    last_checkpoint: Seq,
    metrics: CoreMetrics,
}

/// 这条事件体里还有没有未解析的别名。**不变量 I11 的运行时兜底。**
///
/// 别名（`$name`）只在一批 op 内有效，Core 在提交前把它换成真 id。漏一处的话
/// 症状不是当场报错，而是重放到那里时多出一个永远指不到的引用 —— 那种 bug
/// 事后极难定位，所以在源头 debug_assert 掉。
fn has_alias(body: &Body) -> bool {
    let ops: &[Op] = match body {
        Body::Edited { ops } => ops,
        Body::Inferred { ops, .. } => ops,
        _ => return false,
    };
    ops.iter().any(|op| {
        let mut op = op.clone();
        op.node_refs_mut().iter().any(|r| r.is_alias())
            || op.edge_refs_mut().iter().any(|r| r.is_alias())
    })
}

/// 退出时等 turn 收尾的宽限期。到点还没收完就丢下它走 —— 卡住的 turn
/// 不能变成「应用退不出去」。
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// 一键蒸馏的指令。产出是给**下一个新对话**快速入手用的。
const DISTILL_PROMPT: &str = "把这段会话沉淀成持久记忆的更新草稿。输出四节 Markdown：\n\
     ## 项目 —— 概述、当前阶段目标、进展（跑通并成立的 / 试过没成立的，各带方法概述）\n\
     ## 合作偏好 —— 这次交互里显现出来的偏好\n\
     ## 知识与经验评估 —— 用户在哪些点上有储备 / 没储备 / 看不出来，附依据\n\
     ## 值得入库的易犯错场景 —— 这次返工或纠正对应的场景与指令草稿\n\n\
     只写这段会话里真实发生过的事，不要补全、不要推测。\n\
     写给一个没参与过这段对话的新会话看：它读完应该能接手这个项目。";

/// 启动 Core。
pub fn start(deps: CoreDeps) -> Started {
    let (tx, rx) = mpsc::channel(deps.core_capacity);
    let ui_tx = deps
        .ui
        .clone()
        .unwrap_or_else(|| broadcast::channel(deps.ui_capacity).0);
    let ui_rx = ui_tx.subscribe();

    let mut core = Core {
        session: deps.session.clone(),
        ws: Workspace::new(),
        seq: Seq::ZERO,
        events: Arc::new(Vec::new()),
        turn: TurnPhase::Idle,
        turn_start: Seq::ZERO,
        turn_edits: HashSet::new(),
        open_calls: HashMap::new(),
        open_questions: Vec::new(),
        scene: None,
        view_taken: false,
        shutdown: None,
        pending_scene: None,
        inbox: VecDeque::new(),
        seen: HashSet::new(),
        ui: ui_tx,
        cost: CostLedger::default(),
        writer: deps.writer,
        self_tx: tx.clone(),
        models: deps.models,
        registry: deps.registry,
        memory: deps.memory,
        memory_dir: deps.memory_dir,
        context_limit: deps.context_limit,
        tool_config: deps.tool_config,
        mode: deps.mode,
        next_turn: 1,
        next_task: Arc::new(AtomicU64::new(1)),
        checkpoint_every: deps.checkpoint_every,
        last_checkpoint: Seq::ZERO,
        metrics: CoreMetrics::default(),
    };

    if let Some(r) = deps.restored {
        core.adopt(r);
    }

    let join = tokio::spawn(core.run(rx));
    Started { handle: CoreHandle::new(tx), ui: ui_rx, join }
}

impl Core {
    /// 接管恢复结果：物化好的图、时间线、待补的修补事件。
    ///
    /// **修补事件是真写进时间线的**，不是内存里的一个标记 —— 否则下次启动
    /// 还要再判一次「上次是不是崩了」，而且中间任何一次组装 prompt 都会缺 tool 消息。
    fn adopt(&mut self, r: Restored) {
        self.ws = r.ws;
        self.seq = r.seq;
        self.open_questions = r.open_questions;
        self.cost = CostLedger::from_events(&r.events);
        self.scene = r.events.iter().rev().find_map(|e| match &e.body {
            Body::SceneOverridden { to, .. } => Some(to.clone()),
            Body::Judged { scene, .. } => Some(scene.clone()),
            _ => None,
        });
        // 用户点了换场景、还没有哪一轮把它消费掉，进程就没了 ⇒ 重开之后它仍然有效。
        // 判据是「这条 SceneOverridden 之后再没开过轮」—— 开过轮就说明被读走了。
        // 和未回答的提问同一个道理：那是一份还没兑现的用户意图，不该随进程消失。
        self.pending_scene = r
            .events
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                Body::TurnOpened { .. } => Some(None),
                Body::SceneOverridden { to, .. } => Some(Some(to.clone())),
                _ => None,
            })
            .flatten();
        for c in &r.client_ids {
            self.seen.insert(c.clone());
        }
        self.next_turn = r
            .events
            .iter()
            .filter_map(|e| e.turn.map(|t| t.0))
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        self.metrics.events = r.events.len() as u64;

        let mut events = r.events;
        let crashed = r.crashed;
        let n_repairs = r.repairs.len();
        if n_repairs > 0 {
            let sealed = seal(&self.session, r.repairs, &mut self.seq);
            for e in &sealed {
                self.ws.apply(e);
            }
            events.extend(sealed.iter().cloned());
            self.metrics.repairs = n_repairs as u64;
            self.metrics.events += n_repairs as u64;
            // 修补必须落盘，而且是 must-flush：不落，下次启动还会再判一次
            // 「上次崩了」，而且中间任何一次组装 prompt 都会缺 tool 消息。
            let (wtx, _wrx) = oneshot::channel();
            let _ = self.writer.send(WriteJob::Append { evs: sealed, ack: Some(wtx) });
        }
        let n = events.len();
        let qs = self.open_questions.len();
        self.events = Arc::new(events);
        self.last_checkpoint = self.seq;
        let _ = self.ui.send(UiEvent::Recovered {
            events: n,
            crashed_turns: crashed,
            reopened_questions: qs,
        });
    }

    /// 事件循环。`handle` 全同步，所以这个 while 转得飞快。
    async fn run(mut self, mut rx: mpsc::Receiver<CoreMsg>) -> CoreSummary {
        while let Some(m) = rx.recv().await {
            if !self.handle(m) {
                break;
            }
        }
        CoreSummary {
            session: self.session,
            metrics: self.metrics,
            cost: self.cost,
            ws: self.ws,
            seq: self.seq,
            events: self.events,
        }
    }

    /// 返回 false 表示要退出循环。**全程同步，不 await。**
    fn handle(&mut self, m: CoreMsg) -> bool {
        match m {
            // ────────────────────────── 来自 UI ──────────────────────────
            CoreMsg::Send { client_id, text, mode, answering, reply } => {
                self.metrics.user_inputs += 1;
                let cid = client_id.to_string();
                if !self.seen.insert(cid.clone()) {
                    // 手抖点了两次。内存是快路径，库上的 UNIQUE 索引兜底跨重启的重发。
                    self.metrics.deduped_inputs += 1;
                    let _ = reply.send(None);
                    return true;
                }
                let body = match answering {
                    Some(_) => {
                        self.metrics.answers += 1;
                        Body::Answered { choice: text.clone() }
                    }
                    None => Body::Said { client_id: cid, text: text.clone() },
                };
                let draft = match answering {
                    Some(q) => Draft::reply_to(None, q, body),
                    None => Draft::new(None, body),
                };
                // must-flush：用户付出过动作的数据，落盘确认之后才 ack。
                let (wtx, wrx) = oneshot::channel();
                let (seq, evs) = self.commit(vec![draft], Some(wtx));
                Self::ack_later(reply, seq, wrx);
                self.inbox.extend(evs);

                // 先算出决定再执行，避免同时借用 self.turn 和 &mut self
                enum Decide {
                    Start,
                    Queue,
                    Interrupt(TurnId),
                }
                let d = match (&self.turn, mode) {
                    (TurnPhase::Idle, _) => Decide::Start,
                    (TurnPhase::Running { .. }, SendMode::Queue) => Decide::Queue,
                    (TurnPhase::Running { id, .. }, SendMode::InterruptAndSend) => {
                        Decide::Interrupt(*id)
                    }
                    // 正在收敛：新输入只排队，收敛完 Finished 会自然启动下一轮
                    (TurnPhase::Closing { .. }, _) => Decide::Queue,
                };
                match d {
                    Decide::Start => self.start_turn(),
                    Decide::Queue => {
                        let _ = self.ui.send(UiEvent::Queued { pending: self.inbox.len() });
                    }
                    Decide::Interrupt(id) => self.begin_interrupt(id),
                }
            }

            CoreMsg::Edit { ops, reply } => {
                if ops.is_empty() {
                    let _ = reply.send(None);
                    return true;
                }
                // 别名解析必须在占键与提交之前：仲裁比的是真 id，
                // 而落进时间线的也只能是真 id（不变量 I11）。
                let (ops, bad) = state::resolve(ops, &self.ws.flow, self.peek_seq());
                if !bad.is_empty() {
                    // UI 是照着当前图生成这些 op 的，理论上引用不到目标只会是 bug。
                    eprintln!("[core] 用户编辑有 {} 条引用不到目标，已丢弃：{bad:?}", bad.len());
                }
                if ops.is_empty() {
                    let _ = reply.send(None);
                    return true;
                }
                // 只有 turn 正在跑时才进仲裁集：仲裁的范围就是一个 turn。
                // 空闲时的编辑不需要仲裁 —— 下一轮模型看到的本来就是用户的值，
                // 约束由 prompt 里的 `[用户设定]` 标记承担。
                //
                // **必须在 apply 之前算键**：删除类 op 的键（端点对、归一化标签）
                // 要从当前图里查，图一变就查不到了。
                if !matches!(self.turn, TurnPhase::Idle) {
                    for op in &ops {
                        for k in op.keys(&self.ws.flow) {
                            self.turn_edits.insert(k);
                        }
                    }
                }
                self.metrics.ops_user += ops.len() as u64;
                let (wtx, wrx) = oneshot::channel();
                let (seq, _) = self.commit(vec![Draft::new(None, Body::Edited { ops })], Some(wtx));
                Self::ack_later(reply, seq, wrx);
            }

            CoreMsg::SetMode { to } => {
                // 只改下一轮的取向，不动正在跑的这一轮 —— 那一轮的 prompt 早拼好了，
                // 中途换 mode 只会让它前后不一致。
                self.mode = to;
            }

            CoreMsg::AdvancePhase { to } => {
                // 阶段闸门在用户手里。模型没有推进权，也没有阻断权。
                self.metrics.phase_advances += 1;
                self.commit(vec![Draft::new(None, Body::PhaseSet { to })], None);
            }

            CoreMsg::OverrideScene { to } => {
                let from = self.scene.clone().unwrap_or_else(|| "none".into());
                self.metrics.scene_overrides += 1;
                // 下一轮生效。UI 的标签立刻变（Appended 事件），但注入要等下一轮 ——
                // 轮中换会重新引入「同一份输入在轮中变了」那类 bug。
                self.pending_scene = Some(to.clone());
                self.commit(vec![Draft::new(None, Body::SceneOverridden { from, to })], None);
            }

            CoreMsg::Distill => self.start_distill(),

            CoreMsg::Fork { before_turn, title, reply } => {
                let _ = reply.send(self.fork(before_turn, title));
            }

            CoreMsg::Interrupt { turn } => self.begin_interrupt(turn),

            CoreMsg::Snapshot { reply } => {
                let _ = reply.send(self.snap());
            }

            CoreMsg::Flush { reply } => {
                // 空的 must-flush 批次 = 屏障：writer 见到 ack 就把攒着的冲掉。
                let (wtx, wrx) = oneshot::channel();
                let _ = self.writer.send(WriteJob::Append { evs: Vec::new(), ack: Some(wtx) });
                tokio::spawn(async move {
                    let _ = reply.send(wrx.await.unwrap_or(false));
                });
            }

            CoreMsg::Shutdown { reply } => {
                // 优雅退出与被 kill 的全部差别：cancel → **等 turn 收尾** →
                // 补齐未闭合的调用 → 把攒着的事件冲干净 → 才回执。
                match &self.turn {
                    TurnPhase::Idle => {
                        self.finish_shutdown(reply);
                        return false;
                    }
                    TurnPhase::Running { id, token } => {
                        let id = *id;
                        token.cancel();
                        let _ = self.ui.send(UiEvent::Stopping { turn: id });
                        self.turn = TurnPhase::Closing { id };
                    }
                    TurnPhase::Closing { .. } => {}
                }
                self.shutdown = Some(reply);
                // 兜底：turn 卡住也不能永远退不出去。
                let tx = self.self_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(SHUTDOWN_GRACE).await;
                    let _ = tx.send(CoreMsg::ShutdownNow).await;
                });
            }

            CoreMsg::ShutdownNow => {
                // turn 在宽限期内没收完。它已经 cancel 过了，剩下的产物只能不要。
                if let Some(reply) = self.shutdown.take() {
                    eprintln!("[core] turn 收尾超时，强制退出");
                    self.finish_shutdown(reply);
                    return false;
                }
            }

            // ──────────────────────── 来自 TurnTask ────────────────────────
            CoreMsg::View { turn, reply } => {
                if !self.is_running(turn) {
                    let _ = reply.send(None);
                    return true;
                }
                // scene_override 只在本轮第一次取视图时给出：它是一次性的决定，
                // 不是状态。turn 拿到之后自己记着用一整轮。
                let scene_override =
                    if self.view_taken { None } else { self.pending_scene.take() };
                self.view_taken = true;
                let _ = reply.send(Some(Box::new(TurnView {
                    turn,
                    seq: self.seq,
                    turn_start: self.turn_start,
                    // **当前**的，不是轮初冻结的那份。
                    ws: self.ws.clone(),
                    events: self.events.clone(),
                    mode: self.mode,
                    scene_override,
                })));
            }

            CoreMsg::TakeInjections { turn, reply } => {
                // 只返回计数：内容早就在时间线上了，turn 下一次取视图自然看到。
                // 上一版返回事件本身，于是 turn 顺手把它们又拼进了自己的局部副本。
                let v: Injected = if self.is_running(turn) {
                    self.metrics.inbox_taken += self.inbox.len() as u64;
                    let mut got = Injected::default();
                    for e in self.inbox.drain(..) {
                        match e.body {
                            Body::Answered { .. } => got.answered += 1,
                            _ => got.said += 1,
                        }
                    }
                    got
                } else {
                    if !self.inbox.is_empty() {
                        self.metrics.injections_refused_closing += 1;
                    }
                    Injected::default()
                };
                let _ = reply.send(v);
            }

            CoreMsg::Emit { turn, emit, reply } => {
                // is_current 而不是 is_running：正在收敛的 turn 产生的东西仍然要收下。
                // 打断杀死的是控制流，不是这一轮已经产生的内容。
                if !self.is_current(turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    let _ = reply.send(Applied::default());
                    return true;
                }
                let applied = self.absorb(turn, emit);
                let _ = reply.send(applied);
            }

            CoreMsg::Finished { turn, outcome } => {
                if !self.is_current(turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    return true;
                }
                // 收尾时还有没返回的调用 ⇒ 逐个补 Aborted。turn 侧的
                // close_open_calls 正常会做掉，这里是兜底：Core 才是唯一知道
                // 「哪些调用发出去了」的地方（open_calls 是它维护的）。
                self.close_open_calls("[interrupted] 用户在工具返回前打断");

                let stats = serde_json::to_string(&outcome.stats).unwrap_or_default();
                // must-flush：TurnClosed 是「这一轮真的收完了」的凭据。它没落盘而
                // 进程没了，下次启动会把一轮已经好好结束的对话标成异常退出。
                let (wtx, _wrx) = oneshot::channel();
                self.commit(
                    vec![Draft::new(
                        Some(turn),
                        Body::TurnClosed { aborted: outcome.aborted, stats },
                    )],
                    Some(wtx),
                );

                // turn 边界打 checkpoint —— 它同时是回滚粒度，两者对齐不是巧合：
                // 回滚只会落在轮边界上，checkpoint 打在轮边界就总能就近起跳。
                self.checkpoint();

                self.turn = TurnPhase::Idle;
                self.turn_edits.clear();
                if outcome.aborted {
                    self.metrics.turns_aborted += 1;
                }
                let _ = self.ui.send(UiEvent::TurnClosed { turn, aborted: outcome.aborted });

                // 正在退出 ⇒ turn 已经收完了，现在才是真的可以走。
                if let Some(reply) = self.shutdown.take() {
                    self.finish_shutdown(reply);
                    return false;
                }
                // 注入必有响应：收敛期间排队的话，现在开一轮去处理。
                if !self.inbox.is_empty() {
                    self.start_turn();
                }
            }
        }
        true
    }

    // ────────────────────────────── 追加 ──────────────────────────────

    /// 把 draft 变成事件：分配 seq → 物化 → 更新派生态 → 推 UI → 丢给 writer。
    ///
    /// **全项目只有这一个地方能推进 `seq`**，也只有这一个地方能改 `ws`。
    /// 下一条事件会落到的位置。
    ///
    /// 图 id 从它铸出来，而铸 id 必须发生在 `commit` **之前**（仲裁比的是真 id）。
    /// 带 ops 的 draft（`Edited` / `Inferred`）在两个调用点都是**单独提交**的，
    /// 所以它拿到的一定是这个值 —— `commit` 里的 `debug_assert` 兜底验这一条。
    fn peek_seq(&self) -> Seq {
        Seq(self.seq.0 + 1)
    }

    fn commit(
        &mut self,
        drafts: Vec<Draft>,
        ack: Option<oneshot::Sender<bool>>,
    ) -> (Seq, Vec<Event>) {
        if drafts.is_empty() {
            if let Some(a) = ack {
                let _ = a.send(true);
            }
            return (self.seq, Vec::new());
        }
        let mut evs = Vec::with_capacity(drafts.len());
        for d in drafts {
            debug_assert!(
                !has_alias(&d.body),
                "I11：未解析的别名不许进时间线 —— 它在重放时会变成永远指不到的引用"
            );
            let e = Event {
                session: self.session.clone(),
                seq: self.seq.bump(),
                turn: d.turn,
                at_ms: now_ms(),
                corr: d.corr,
                body: d.body,
            };
            self.ws.apply(&e);
            self.observe(&e);
            let _ = self.ui.send(UiEvent::Appended {
                seq: e.seq,
                turn: e.turn,
                body: Box::new(e.body.clone()),
            });
            Arc::make_mut(&mut self.events).push(e.clone());
            evs.push(e);
        }
        self.metrics.events += evs.len() as u64;
        let last = self.seq;
        let _ = self.writer.send(WriteJob::Append { evs: evs.clone(), ack });
        if self.seq.0.saturating_sub(self.last_checkpoint.0) >= self.checkpoint_every {
            self.checkpoint();
        }
        (last, evs)
    }

    /// 一条事件对派生态的全部影响。**集中在这一个函数里。**
    ///
    /// 散落在各个 match 臂里是上一版「同一事实两处存储」的来源：
    /// 加一个事件类型时很容易忘了某一处。这里漏了，症状是 UI 不更新，
    /// 而不是数据不一致 —— 因为真相只有时间线一份。
    fn observe(&mut self, e: &Event) {
        match &e.body {
            Body::Called { calls, .. } => {
                for c in calls {
                    self.open_calls.insert(c.id.clone(), e.seq);
                }
            }
            Body::Returned { call_id, .. } | Body::Aborted { call_id, .. } => {
                self.open_calls.remove(call_id);
            }
            Body::Asked { question, options } => {
                self.open_questions.push(OpenQuestion {
                    id: e.seq,
                    question: question.clone(),
                    options: options.clone(),
                });
            }
            Body::Answered { .. } => {
                if let Some(q) = e.corr {
                    self.open_questions.retain(|x| x.id != q);
                }
            }
            Body::Judged { scene, .. } => self.scene = Some(scene.clone()),
            Body::SceneOverridden { to, .. } => self.scene = Some(to.clone()),
            Body::PhaseSet { to } => {
                let _ = self.ui.send(UiEvent::PhaseChanged { to: *to });
            }
            Body::Edited { ops } => {
                let _ = self.ui.send(UiEvent::StateChanged {
                    seq: e.seq,
                    ops: ops.clone(),
                    dropped: vec![],
                });
            }
            Body::Inferred { ops, dropped } => {
                let _ = self.ui.send(UiEvent::StateChanged {
                    seq: e.seq,
                    ops: ops.clone(),
                    dropped: dropped.clone(),
                });
            }
            Body::Folded { folded, .. } => {
                self.metrics.compactions += 1;
                self.metrics.msgs_folded += *folded as u64;
                // 告知，不是征求同意 —— 征求同意就是一次不该有的打扰。
                let before: u32 =
                    self.events.iter().filter_map(|x| x.body.as_message(x.seq, x.corr)).map(|m| toks(&m.content)).sum();
                let _ = self.ui.send(UiEvent::Compacted {
                    folded: *folded,
                    before_tokens: before,
                    after_tokens: before,
                });
            }
            Body::Cost { role, usage, .. } => {
                // 刻意不校验代际：陈旧 turn 花掉的也是真钱，账不能因为它过期就不记。
                self.cost.add(*role, *usage);
                let _ = self.ui.send(UiEvent::CostTick {
                    role: *role,
                    usage: *usage,
                    session_total: self.cost.total(),
                });
            }
            _ => {}
        }
    }

    /// 把 turn 的一条 [`Emit`] 变成时间线上的事件。**仲裁在这里。**
    fn absorb(&mut self, turn: TurnId, emit: Emit) -> Applied {
        let t = Some(turn);
        let (drafts, dropped) = match emit {
            Emit::Judged { scene, rationale } => {
                (vec![Draft::new(t, Body::Judged { scene, rationale })], vec![])
            }
            Emit::Wrote { text, interrupted } => {
                (vec![Draft::new(t, Body::Wrote { text, interrupted })], vec![])
            }
            Emit::Called { text, calls } => {
                (vec![Draft::new(t, Body::Called { text, calls })], vec![])
            }
            Emit::Asked { call_id, question, options } => {
                // 连回发起它的那条 Called。找不到就不带 corr —— 提问本身仍然有效，
                // 只是审计上少一条边，不该因此把提问吞掉。
                let corr = self.open_calls.get(&call_id).copied();
                let mut d = Draft::new(t, Body::Asked { question, options });
                d.corr = corr;
                (vec![d], vec![])
            }
            Emit::Returned { call_id, name, content, outcome, task } => {
                let corr = self.open_calls.get(&call_id).copied();
                let mut d =
                    Draft::new(t, Body::Returned { call_id, name, content, outcome, task });
                d.corr = corr;
                (vec![d], vec![])
            }
            Emit::Aborted { call_id, why } => {
                let corr = self.open_calls.get(&call_id).copied();
                let mut d = Draft::new(t, Body::Aborted { call_id, why });
                d.corr = corr;
                (vec![d], vec![])
            }
            Emit::Noted { text } => (vec![Draft::new(t, Body::Noted { text })], vec![]),
            Emit::Folded { from, to, summary, folded } => {
                (vec![Draft::new(t, Body::Folded { from, to, summary, folded })], vec![])
            }
            Emit::Cost { role, task, usage } => {
                (vec![Draft::new(t, Body::Cost { role, task, usage })], vec![])
            }
            Emit::Infer { ops } => {
                // ★ 全项目唯一的仲裁点。
                //
                // 不是策略（不是「用户的字段不许模型改」），是「用户在这一轮里
                // 刚动过这个位置，模型基于旧值算出来的结果作废」。范围是一个 turn，
                // 跨轮的保护是 prompt 里的 `[用户设定]` 标记，不是硬拒绝。
                //
                // 一条 op 可能占**多个**键 —— 那正是堵漏洞的地方：边除了自己的 id
                // 还占一条有序端点对（模型换个新 id 把用户断开的连接加回来，撞的是
                // 这一条）；删节点除了 id 还占一条归一化标签键（模型换个 id 新建同名
                // 节点复活它，撞的是这一条）。撞上任意一个就整条丢。
                let (ops, mut dropped) =
                    state::resolve(ops, &self.ws.flow, self.peek_seq());
                let mut kept = Vec::new();
                for op in ops {
                    let keys = op.keys(&self.ws.flow);
                    if keys.iter().any(|k| self.turn_edits.contains(k)) {
                        dropped.extend(keys.into_iter().next());
                    } else {
                        kept.push(op);
                    }
                }
                self.metrics.ops_model += kept.len() as u64;
                self.metrics.ops_dropped += dropped.len() as u64;
                // 时间线只记生效的部分；dropped 附在同一条上作为审计与回喂的依据。
                (
                    vec![Draft::new(
                        t,
                        Body::Inferred { ops: kept, dropped: dropped.clone() },
                    )],
                    dropped,
                )
            }
        };
        let (seq, events) = self.commit(drafts, None);
        Applied { seq, dropped, events }
    }

    /// 未闭合的调用逐个补一条 `Aborted`。
    ///
    /// 不补的话，下一次请求里会有一条带 `tool_calls` 却没有对应 tool 消息的
    /// assistant，被 API 直接拒掉。
    fn close_open_calls(&mut self, why: &str) {
        if self.open_calls.is_empty() {
            return;
        }
        let turn = match &self.turn {
            TurnPhase::Running { id, .. } | TurnPhase::Closing { id } => Some(*id),
            TurnPhase::Idle => None,
        };
        let open: Vec<(String, Seq)> =
            self.open_calls.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let drafts = open
            .into_iter()
            .map(|(call_id, corr)| {
                Draft::reply_to(turn, corr, Body::Aborted { call_id, why: why.to_string() })
            })
            .collect();
        self.commit(drafts, None);
    }

    // ────────────────────────────── 其它 ──────────────────────────────

    /// 退出的最后一步：补齐未闭合的调用、让 writer 把攒着的全冲掉、然后才回执。
    fn finish_shutdown(&mut self, reply: oneshot::Sender<()>) {
        self.close_open_calls("[interrupted] 应用退出，工具未返回");
        let (wtx, wrx) = oneshot::channel();
        let _ = self.writer.send(WriteJob::Shutdown { ack: wtx });
        tokio::spawn(async move {
            let _ = wrx.await;
            let _ = reply.send(());
        });
    }

    /// 把落盘回执转成 UI 的 ack。**开一个一次性 task**，因为 Core 自己不能 await。
    fn ack_later(
        reply: oneshot::Sender<Option<Ack>>,
        seq: Seq,
        wrx: oneshot::Receiver<bool>,
    ) {
        tokio::spawn(async move {
            let durable = wrx.await.unwrap_or(false);
            let _ = reply.send(Some(Ack { seq, durable }));
        });
    }

    fn snap(&self) -> Snap {
        Snap {
            session: self.session.clone(),
            seq: self.seq,
            ws: self.ws.clone(),
            scene: self.scene.clone(),
            open_questions: self.open_questions.clone(),
            turn: match &self.turn {
                TurnPhase::Running { id, .. } | TurnPhase::Closing { id } => Some(*id),
                TurnPhase::Idle => None,
            },
            queued: self.inbox.len(),
            mode: self.mode,
        }
    }

    fn checkpoint(&mut self) {
        self.last_checkpoint = self.seq;
        self.metrics.checkpoints += 1;
        let _ = self.writer.send(WriteJob::Checkpoint {
            session: self.session.clone(),
            seq: self.seq,
            ws: Box::new(self.ws.clone()),
        });
    }

    /// R5：从某一轮分叉出一个新会话。
    ///
    /// # 回滚不是截断
    ///
    /// 原会话**一条事件都不动**。那条路走过就走过了 —— 它是「试过没成立的方法」的
    /// 原始记录，而这恰恰是这个项目里最值钱的一类数据（`memory/project.md` 里
    /// 专门有一栏放它，还标着「这一栏比上一栏值钱」）。删掉它去换一个干净的当前态，
    /// 是拿资产换整洁。
    ///
    /// 新会话记 `parent` + `forked_at`，读它的时候沿链把父会话分叉点之前的历史
    /// 一起带上（[`crate::store::Store::load_chain`]），所以分支一开口就有完整上下文。
    ///
    /// **本 Core 不切过去。** 一个 Core 一个 session；调用方拿着返回的 id
    /// 另起一个 Core 打开新分支。让一个 Core 同时管两条时间线，
    /// 就等于把「唯一写权」变成两个写权，seq 也会立刻打架。
    fn fork(&mut self, before: TurnId, title: String) -> Result<SessionId, String> {
        let opened = self
            .events
            .iter()
            .find(|e| e.turn == Some(before) && matches!(e.body, Body::TurnOpened { .. }))
            .map(|e| e.seq)
            .ok_or_else(|| format!("找不到 {before} 的起点"))?;
        // 分叉点 = 那条 `TurnOpened` 的**前一位**。
        //
        // 注意这会**保留触发这一轮的那句用户输入**：`Said` 是在 `TurnOpened` 之前
        // 提交的（用户按下发送时就落盘了，然后才开轮）。这是有意的 ——
        // 「从 t3 分叉」的意思是「t3 这一轮重来」，用户那句话是这一轮的输入，
        // 不是上一轮的产物。分支打开时它处于「说了还没被回答」的状态，
        // 正是重来的起点。要不要开轮去答它，是 app 的策略，不是 Core 的。
        let at = Seq(opened.0.saturating_sub(1));

        let child = SessionId::new();
        let title = if title.trim().is_empty() {
            format!("从 {before} 分叉")
        } else {
            title
        };
        let _ = self.writer.send(WriteJob::NewSession {
            session: Session {
                id: child.clone(),
                parent: Some(self.session.clone()),
                forked_at: at,
                title,
                created_ms: now_ms(),
            },
        });
        self.metrics.forks += 1;
        let _ = self.ui.send(UiEvent::Forked { session: child.clone(), from_turn: before, at });
        Ok(child)
    }

    fn is_running(&self, turn: TurnId) -> bool {
        matches!(&self.turn, TurnPhase::Running { id, .. } if *id == turn)
    }

    /// Running 或 Closing 都算「当前代」。
    fn is_current(&self, turn: TurnId) -> bool {
        match &self.turn {
            TurnPhase::Running { id, .. } => *id == turn,
            TurnPhase::Closing { id } => *id == turn,
            TurnPhase::Idle => false,
        }
    }

    /// 打断是**两段式反馈**：`Stopping` 立刻发（用户点了停就该马上看到反应），
    /// `TurnClosed` 等 `Finished` 回来再发。中间那段 UI 显示「正在停止」。
    fn begin_interrupt(&mut self, id: TurnId) {
        let token = match &self.turn {
            TurnPhase::Running { id: cur, token } if *cur == id => Some(token.clone()),
            _ => None,
        };
        if let Some(t) = token {
            t.cancel();
            let _ = self.ui.send(UiEvent::Stopping { turn: id });
            self.turn = TurnPhase::Closing { id };
        }
    }

    /// 一键蒸馏。**用户操作触发，不是自动行为。**
    ///
    /// 产出写成草稿文件而不是直接覆盖持久层：蒸馏是模型对整段会话的概括，
    /// 直接盖掉用户手写的记忆是越权。用户看过、改过再合并。
    fn start_distill(&mut self) {
        self.metrics.distills += 1;
        let Some(dir) = self.memory_dir.clone() else {
            let _ = self.ui.send(UiEvent::Distilled { draft: "(未配置持久层目录)".into() });
            return;
        };
        let client = self.models.subagent.clone();
        let ui = self.ui.clone();
        let events = self.events.clone();
        let ws = self.ws.clone();
        let back = CoreHandle::new(self.self_tx.clone());
        let stamp = now_ms() / 1000;

        tokio::spawn(async move {
            // 蒸馏也重读持久层：用户可能刚改过，蒸馏该基于他改后的版本增量。
            let mem = Memory::load_or_bootstrap(&dir).await;
            let mut msgs =
                vec![Message::system(DISTILL_PROMPT), Message::system(mem.prompt_block())];
            msgs.push(Message::system(format!(
                "本次会话的推断结果：\n{}",
                serde_json::to_string_pretty(&ws).unwrap_or_default()
            )));
            // 读全程 —— 折叠只影响拼给模型的那份上下文，原文一直在时间线上。
            msgs.extend(crate::event::assemble(&events));
            match complete(client.as_ref(), msgs, CancellationToken::new()).await {
                Ok((text, usage)) => {
                    back.cost_out(Role::Subagent, usage).await;
                    let p = crate::memory::draft_path(&dir, stamp);
                    let draft = match tokio::fs::write(&p, text).await {
                        Ok(()) => p.display().to_string(),
                        Err(e) => format!("(蒸馏草稿写入失败: {e})"),
                    };
                    let _ = ui.send(UiEvent::Distilled { draft });
                }
                Err(e) => {
                    let _ = ui.send(UiEvent::Distilled { draft: format!("(蒸馏失败: {e})") });
                }
            }
        });
    }

    fn start_turn(&mut self) {
        let id = TurnId(self.next_turn);
        self.next_turn += 1;
        let token = CancellationToken::new();
        self.turn = TurnPhase::Running { id, token: token.clone() };
        self.turn_start = self.seq;
        self.turn_edits.clear();
        self.view_taken = false;
        self.metrics.turns_started += 1;

        self.commit(vec![Draft::new(Some(id), Body::TurnOpened { mode: self.mode })], None);
        let _ = self.ui.send(UiEvent::TurnStarted { turn: id });

        let cacheable = toks(&self.memory.prompt_block()) + toks(&self.memory.playbook.catalog());
        let _ = self.ui.send(UiEvent::ContextFootprint {
            total: cacheable
                + crate::event::assemble(&self.events)
                    .iter()
                    .map(|m| toks(&m.content))
                    .sum::<u32>(),
            cacheable,
            events: self.events.len(),
        });

        let ctx = TurnCtx {
            id,
            token,
            core: CoreHandle::new(self.self_tx.clone()),
            ui: self.ui.clone(),
            models: self.models.clone(),
            registry: self.registry.clone(),
            memory_dir: self.memory_dir.clone(),
            memory: self.memory.clone(),
            tool_config: self.tool_config,
            context_limit: self.context_limit,
            next_task: self.next_task.clone(),
        };
        let back = CoreHandle::new(self.self_tx.clone());
        tokio::spawn(async move {
            let outcome = run_turn(ctx).await;
            back.finished(id, outcome).await;
        });
    }
}

/// 给 turn 侧取 TaskId 用。
pub fn next_task(counter: &AtomicU64) -> crate::ids::TaskId {
    crate::ids::TaskId(counter.fetch_add(1, Ordering::Relaxed))
}
