//! `Core`：SessionActor。**唯一写权**，全同步处理，微秒级返回。
//!
//! # 读这个文件时请守住一件事
//!
//! 每一个 match 臂都只做「改状态 + 发事件 + 丢一个写盘任务」。
//! 它们**不含流程**——流程全在 [`crate::turn::run_turn`] 里。
//! 一旦某个臂里出现了 `.await` 一个长任务，或者出现了「先做 A 再做 B 然后等 C」，
//! 说明流程漏进来了，R4 的即时打断就会失效。
//!
//! # 状态机只负责两件事
//!
//! **状态的稳定维持**（写权仲裁、事务边界、崩溃恢复）和
//! **模型与 workspace 之间的稳定接口**（快照 / 提交 / 工具调用 / 插话）。
//! 它不负责「这一步该做什么」—— 那是模型的工作，见 [`crate::scene`]。
//!
//! # 这里删掉过两样东西
//!
//! - **`locked: HashSet<Path>`**：把「用户碰过的字段不许模型改」这条策略摊进了
//!   `apply`、`Unlock`、快照、恢复、UI 五个地方。约束已下沉到 prompt。
//!   [`Core::apply`] 里现在**只剩一个拒绝条件**，搜 `★`，而且它不是策略是 lost-update 防护。
//! - **`Budget` / `TryIntervene`**：用限额换安静，等于在预算耗尽时用降质减少打扰。
//!   不该有的打扰权限已逐个掐掉，清单见 [`crate::turn`] 头部，掐干净就不需要限额。

use crate::context::{ContextLimit, toks};
use crate::cost::CostLedger;
use crate::handle::CoreHandle;
use crate::ids::{LruSet, TurnId, Version};
use crate::memory::{Memory, draft_path};
use crate::model::{Message, Mode, Models, Role, complete};
use crate::msg::{ApplyReport, CoreMsg, Reason, SendMode, UiEvent, UserMsg, WriteJob};
use crate::state::{Op, Origin, Patch, Path, Pending, StateView, Workspace};
use crate::tools::{Registry, ToolConfig};
use crate::turn::{TurnCtx, run_turn};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
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
/// 前四个是**异常指标**，正常跑一轮应该全是 0；不是 0 就说明有 bug 或有竞态。
#[derive(Debug, Clone, Default)]
pub struct CoreMetrics {
    /// 收到了对不上代际的消息。打断时偶发正常（子任务还在回流）；
    /// **持续增长说明代际判断漏了地方**。
    pub stale_msgs_dropped: u64,
    /// 用户在收敛期间说的话没被垂死的 turn 吞掉（正确行为）。
    /// 但它一直涨而 `inbox_taken` 不涨，说明 turn 卡在 Closing 出不来。
    pub injections_refused_closing: u64,
    /// 模型的改动撞上了用户的编辑。
    /// **频繁出现说明用户在轮中改得很勤**，可以考虑缩短单轮时长而不是改冲突策略。
    pub ops_rejected_stale: u64,
    /// 用户改了模型本轮正打算改的字段。
    pub pending_dropped_by_user_edit: u64,

    pub turns_started: u64,
    pub turns_aborted: u64,
    pub user_inputs: u64,
    pub deduped_inputs: u64,
    /// 从 inbox 取走的消息总数。**包含每轮的首条输入**，不只是中途插话。
    pub inbox_taken: u64,
    pub ops_applied_user: u64,
    pub ops_pending_model: u64,
    pub ops_committed: u64,
    pub phase_advances: u64,
    /// 用户一键更换本轮场景的次数。**这是场景描述写得准不准的直接反馈。**
    pub scene_overrides: u64,
    /// 触发过几次上下文折叠，折叠了多少条消息。
    pub compactions: u64,
    pub msgs_folded: u64,
    /// 用户按了几次一键蒸馏。
    pub distills: u64,
    pub writes_emitted: u64,
}

impl CoreMetrics {
    pub fn line(&self) -> String {
        format!(
            "turns {}(aborted {}) / inputs {}(去重 {}) / 取走输入 {} / 换场景 {} / 折叠 {}次·{}条 / \
             ops: user {} pending {} committed {} / 异常: stale_msg {} refuse_inj {} stale_op {} pend_drop {}",
            self.turns_started,
            self.turns_aborted,
            self.user_inputs,
            self.deduped_inputs,
            self.inbox_taken,
            self.scene_overrides,
            self.compactions,
            self.msgs_folded,
            self.ops_applied_user,
            self.ops_pending_model,
            self.ops_committed,
            self.stale_msgs_dropped,
            self.injections_refused_closing,
            self.ops_rejected_stale,
            self.pending_dropped_by_user_edit,
        )
    }
}

pub struct CoreDeps {
    /// 判断 / 回答 / subagent 三个角色分别可配（R3）。
    pub models: Arc<Models>,
    pub registry: Arc<Registry>,
    /// 持久层：场景库、案例、用户偏好、知识评估、项目概述与进展。
    pub memory: Arc<Memory>,
    /// 持久层所在目录。一键蒸馏的草稿写在这里；None 表示不落盘（测试）。
    pub memory_dir: Option<PathBuf>,
    /// 上下文上限与压缩水位。
    pub context_limit: ContextLimit,
    pub tool_config: ToolConfig,
    pub mode: Mode,
    pub writer: mpsc::UnboundedSender<WriteJob>,
    /// 恢复用：从最新 snapshot 读回来的 workspace。新会话传 None。
    pub restored: Option<(Workspace, Version)>,
    pub core_capacity: usize,
    pub ui_capacity: usize,
}

impl CoreDeps {
    pub fn new(
        models: Arc<Models>,
        registry: Arc<Registry>,
        memory: Arc<Memory>,
        writer: mpsc::UnboundedSender<WriteJob>,
        mode: Mode,
    ) -> Self {
        Self {
            models,
            registry,
            memory,
            memory_dir: None,
            context_limit: ContextLimit::default(),
            tool_config: ToolConfig::default(),
            mode,
            writer,
            restored: None,
            core_capacity: 64,
            ui_capacity: 1024,
        }
    }
}

pub struct Started {
    pub handle: CoreHandle,
    pub ui: broadcast::Receiver<UiEvent>,
    pub join: tokio::task::JoinHandle<CoreSummary>,
}

pub struct CoreSummary {
    pub metrics: CoreMetrics,
    pub cost: CostLedger,
    pub ws: Workspace,
    pub version: Version,
    pub history: Vec<Message>,
}

pub struct Core {
    /// 唯一可变副本。
    ws: Workspace,
    version: Version,
    /// 本轮模型改动，未提交。
    pending: Pending,
    /// 用户最近一次写某个路径时的版本。**进程内，不落盘。**
    ///
    /// 只服务一件事：判断「模型取快照之后，用户动过这个路径吗」。这个问题的作用域
    /// 天然只有一轮，跨会话没有意义，所以 resume 之后为空是正确的 —— 而不是像早期
    /// 那把锁一样，把用户一次可能搞错的操作永久留在磁盘上。
    touched: HashMap<Path, Version>,
    turn: TurnPhase,
    /// 待处理 / 待注入的用户输入。
    inbox: VecDeque<UserMsg>,
    /// 完整会话历史。R5 的载体，也是上下文的第五段。
    ///
    /// **不做滑动窗口。** 用户会引用三十轮之前定下的东西，只送最近 K 轮不现实。
    /// 接近模型上限时由 turn 侧折叠早期部分（`CompactHistory`）。
    history: Vec<Message>,
    /// client_id 去重（用户手抖点两次发送）。
    seen: LruSet<uuid::Uuid>,
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
    metrics: CoreMetrics,
}

/// 一键蒸馏的指令。产出是给**下一个新对话**快速入手用的。
const DISTILL_PROMPT: &str = "把这段会话沉淀成持久记忆的更新草稿。输出四节 Markdown：\n\
     ## 项目 —— 概述、当前阶段目标、进展（跑通并成立的 / 试过没成立的，各带方法概述）\n\
     ## 合作偏好 —— 这次交互里显现出来的偏好\n\
     ## 知识与经验评估 —— 用户在哪些点上有储备 / 没储备 / 看不出来，附依据\n\
     ## 值得入库的易犯错场景 —— 这次返工或纠正对应的场景与指令草稿\n\n\
     只写这段会话里真实发生过的事，不要补全、不要推测。\n\
     写给一个没参与过这段对话的新会话看：它读完应该能接手这个项目。";

/// 启动 Core，返回 handle / UI 订阅 / join。
pub fn start(deps: CoreDeps) -> Started {
    let (tx, rx) = mpsc::channel(deps.core_capacity);
    let (ui_tx, ui_rx) = broadcast::channel(deps.ui_capacity);
    let (ws, version) = deps.restored.unwrap_or_else(|| (Workspace::new(), Version::ZERO));

    let core = Core {
        ws,
        version,
        pending: Pending::default(),
        touched: HashMap::new(),
        turn: TurnPhase::Idle,
        inbox: VecDeque::new(),
        history: Vec::new(),
        seen: LruSet::new(256),
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
        metrics: CoreMetrics::default(),
    };
    let join = tokio::spawn(core.run(rx));
    Started { handle: CoreHandle::new(tx), ui: ui_rx, join }
}

impl Core {
    /// 事件循环。`handle` 全同步，所以这个 while 转得飞快。
    async fn run(mut self, mut rx: mpsc::Receiver<CoreMsg>) -> CoreSummary {
        while let Some(m) = rx.recv().await {
            if !self.handle(m) {
                break;
            }
        }
        let _ = self.writer.send(WriteJob::Shutdown);
        CoreSummary {
            metrics: self.metrics,
            cost: self.cost,
            ws: self.ws,
            version: self.version,
            history: self.history,
        }
    }

    /// 返回 false 表示要退出循环。**全程同步，不 await。**
    fn handle(&mut self, m: CoreMsg) -> bool {
        match m {
            // ────────────────────────── 来自 UI ──────────────────────────
            CoreMsg::UserInput { client_id, text, mode } => {
                self.metrics.user_inputs += 1;
                if !self.seen.insert(client_id) {
                    self.metrics.deduped_inputs += 1;
                    return true; // 手抖点了两次
                }
                self.inbox.push_back(UserMsg { client_id, text, at: self.version });

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
                    // 正在收敛：新输入只排队，收敛完 TurnFinished 会自然启动下一轮
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

            CoreMsg::UserEdit { patch } => {
                // 立即写进 state 并落盘 —— UI 要即时反映，崩了不能丢。
                // 但**不推给正在跑的 turn**：那一轮已经取过快照了，用户的修改
                // 随下一轮上下文一起发出去。用户的修改行为不可预知，
                // 让模型在一轮之内追着变化跑没有好处。
                let rep = self.apply(patch);
                self.emit_snapshot();
                let _ = self.ui.send(UiEvent::StateChanged(rep));
            }

            CoreMsg::AdvancePhase { to } => {
                // 阶段闸门在用户手里。这条消息**只从 UI 来**，turn task 没有对应的方法。
                // 注意反过来也成立：模型没有阻断权，不能因为「我觉得还没准备好」拦住用户。
                self.ws.phase = to;
                self.version.bump();
                self.metrics.phase_advances += 1;
                self.emit_snapshot();
                let _ = self.ui.send(UiEvent::PhaseChanged { to });
            }

            CoreMsg::OverrideScene { from, to } => {
                // 用户一键更换本轮场景。记下来用于回头修正场景描述。
                self.ws.scene = Some(to.clone());
                self.ws.scene_overrides.push((from, to));
                self.metrics.scene_overrides += 1;
                self.version.bump();
                self.emit_snapshot();
            }

            CoreMsg::Distill => self.start_distill(),

            CoreMsg::Interrupt { turn } => self.begin_interrupt(turn),

            CoreMsg::Shutdown => {
                if let TurnPhase::Running { token, .. } = &self.turn {
                    token.cancel();
                }
                return false;
            }

            // ──────────────────────── 来自 TurnTask ────────────────────────
            CoreMsg::Snapshot { reply } => {
                let _ = reply.send(Arc::new(self.view()));
            }

            CoreMsg::TakeInjections { turn, reply } => {
                // 只有**正在运行**的 turn 能取。
                // 一个正在收敛（Closing）的 turn 把用户的新输入吞掉，那句话就永远没人回了。
                let v: Vec<UserMsg> = if self.is_running(turn) {
                    self.metrics.inbox_taken += self.inbox.len() as u64;
                    self.inbox.drain(..).collect()
                } else {
                    if !self.inbox.is_empty() {
                        self.metrics.injections_refused_closing += 1;
                    }
                    Vec::new()
                };
                let _ = reply.send(v);
            }

            CoreMsg::SceneDecided { turn, scene, rationale } => {
                if !self.is_running(turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    return true;
                }
                // Core 只是存下来给 UI 显示，**不据此改变任何流程**。
                self.ws.scene = Some(scene.clone());
                self.version.bump();
                let _ = self.ui.send(UiEvent::SceneChosen { turn, scene, rationale });
            }

            CoreMsg::Questions { turn, open, parked } => {
                if !self.is_running(turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    return true;
                }
                self.ws.open = open;
                self.ws.parked = parked;
                self.version.bump();
                let _ = self.ui.send(UiEvent::QuestionsChanged {
                    open: self.ws.open.len(),
                    parked: self.ws.parked.len(),
                });
            }

            CoreMsg::Propose { patch, reply } => {
                // 这里用 is_current 而不是 is_running：一个正在收敛的 turn 提交的改动
                // 仍然要收下。turn 里已经产生的东西该落盘落盘，打断杀死的是控制流，
                // 不是这一轮的产物。
                if !self.is_current(patch.turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    let _ = reply.send(ApplyReport {
                        applied: vec![],
                        rejected: vec![],
                        version: self.version,
                    });
                    return true;
                }
                let rep = self.apply(patch);
                let _ = reply.send(rep.clone());
                // 侧栏先更新，不等 turn 结束
                let _ = self.ui.send(UiEvent::PendingChanged(rep));
            }

            CoreMsg::CompactHistory { turn, summary, keep, folded } => {
                if !self.is_running(turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    return true;
                }
                let before: u32 = self.history.iter().map(|m| toks(&m.content)).sum();
                let mut next = Vec::with_capacity(keep.len() + 1);
                next.push(summary);
                next.extend(keep);
                self.history = next;
                let after: u32 = self.history.iter().map(|m| toks(&m.content)).sum();
                self.metrics.compactions += 1;
                self.metrics.msgs_folded += folded as u64;
                // 告知，不是征求同意 —— 征求同意就是一次不该有的打扰。
                let _ = self.ui.send(UiEvent::Compacted {
                    folded,
                    before_tokens: before,
                    after_tokens: after,
                });
            }

            CoreMsg::Cost { turn: _, role, usage } => {
                // 刻意**不**校验代际：陈旧 turn 花掉的也是真钱，账不能因为它过期就不记。
                self.cost.add(role, usage);
                let _ = self.ui.send(UiEvent::CostTick {
                    role,
                    usage,
                    session_total: self.cost.total(),
                });
            }

            CoreMsg::TurnFinished { turn, outcome } => {
                if !self.is_current(turn) {
                    self.metrics.stale_msgs_dropped += 1;
                    return true;
                }
                // ① 先写历史。先写日志再改状态：崩溃点的最坏情况是「历史里有但 state
                //    没应用」，重放即可；反过来就会丢。
                let commits = self.pending.as_ops();
                self.history.extend(outcome.msgs.iter().cloned());
                let _ = self.writer.send(WriteJob::AppendHistory {
                    turn,
                    version: self.version,
                    msgs: outcome.msgs,
                    commits,
                });
                self.metrics.writes_emitted += 1;

                // ② 再动 state。**打断也提交**，没有 mark_pending_orphan。
                //    打断杀死的是 turn 的控制流，不是 workspace。
                let n = self.commit_pending();
                self.metrics.ops_committed += n as u64;

                // ③ 最后拍快照。
                self.emit_snapshot();

                self.turn = TurnPhase::Idle;
                if outcome.aborted {
                    self.metrics.turns_aborted += 1;
                }
                let _ = self.ui.send(UiEvent::TurnClosed { turn, aborted: outcome.aborted });

                // 注入必有响应：收敛期间排队的话，现在开一轮去处理。
                if !self.inbox.is_empty() {
                    self.start_turn();
                }
            }
        }
        true
    }

    // ────────────────────────────── 内部 ──────────────────────────────

    /// 裁决一次提交。**core 里唯一会拒绝写入的地方。**
    fn apply(&mut self, p: Patch) -> ApplyReport {
        let mut applied = Vec::new();
        let mut rejected = Vec::new();
        let v = self.version.bump();

        for op in &p.ops {
            let path = op.path().clone();
            match p.origin {
                Origin::User => {
                    match op {
                        Op::Set { value, source, confidence, .. } => self.ws.write(
                            path.clone(),
                            value.clone(),
                            Origin::User,
                            // 用户没自己标来源时记成「用户口述」；标了就尊重他标的。
                            source.clone().or(Some(crate::state::Source::User)),
                            *confidence,
                            v,
                        ),
                        Op::Remove { .. } => self.ws.erase(&path),
                    }
                    // 用户的写入必然比一个尚未提交的提议更新 ⇒ 同路径的 pending 作废。
                    // lost-update 防护的前半段。
                    if self.pending.remove_path(&path) {
                        self.metrics.pending_dropped_by_user_edit += 1;
                    }
                    self.touched.insert(path.clone(), v);
                    self.metrics.ops_applied_user += 1;
                    applied.push(path);
                }
                Origin::Model => {
                    // ★ core 里唯一的拒绝条件：模型取快照之后，用户动过同一路径。
                    //
                    //   这不是策略（不是「用户的字段不许模型改」），是 lost-update 防护
                    //   （「你读到的值已经过期了，别拿旧值算出来的结果覆盖新值」）。
                    //   逐 op 按 path 判，不是整份 CAS —— 用户改了推断图的一个节点，
                    //   不该让模型对另一个节点的更新整份失败。
                    if let Some(tv) = self.touched.get(&path) {
                        if *tv > p.base_version {
                            self.metrics.ops_rejected_stale += 1;
                            rejected.push((path, Reason::Stale));
                            continue;
                        }
                    }
                    // 模型的改动**只进 pending，不落 state**。turn 是事务边界。
                    self.pending.put(p.turn, op);
                    self.metrics.ops_pending_model += 1;
                    applied.push(path);
                }
            }
        }
        ApplyReport { applied, rejected, version: self.version }
    }

    /// 把 pending 合并进已提交状态。返回合并了几条。
    fn commit_pending(&mut self) -> usize {
        if self.pending.is_empty() {
            self.pending.clear();
            return 0;
        }
        let v = self.version.bump();
        let ops = std::mem::take(&mut self.pending.ops);
        let n = ops.len();
        for (path, op) in ops {
            match op {
                crate::state::PendingOp::Set { value, source, confidence } => {
                    self.ws.write(path, value, Origin::Model, source, confidence, v)
                }
                crate::state::PendingOp::Remove => self.ws.erase(&path),
            }
        }
        self.pending.clear();
        n
    }

    fn view(&self) -> StateView {
        StateView {
            ws: self.ws.clone(),
            pending: self.pending.clone(),
            version: self.version,
            history: self.history.clone(),
        }
    }

    fn emit_snapshot(&mut self) {
        let _ = self.writer.send(WriteJob::WriteSnapshot {
            version: self.version,
            ws: Box::new(self.ws.clone()),
        });
        self.metrics.writes_emitted += 1;
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

    /// 打断是**两段式反馈**：
    /// `Stopping` 立刻发（用户点了停就该马上看到反应，不等子任务收敛），
    /// `TurnClosed` 等 `TurnFinished` 回来再发。中间那段 UI 显示「正在停止」。
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
        let history = self.history.clone();
        let ws = self.ws.clone();
        let mem = self.memory.clone();
        let back = CoreHandle::new(self.self_tx.clone());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        tokio::spawn(async move {
            let mut msgs =
                vec![Message::system(DISTILL_PROMPT), Message::system(mem.prompt_block())];
            msgs.push(Message::system(format!(
                "本次会话的推断结果：\n{}",
                serde_json::to_string_pretty(&ws).unwrap_or_default()
            )));
            msgs.extend(history);
            match complete(client.as_ref(), msgs, CancellationToken::new()).await {
                Ok((text, usage)) => {
                    back.cost(TurnId::NONE, Role::Subagent, usage).await;
                    let p = draft_path(&dir, stamp);
                    let _ = tokio::fs::write(&p, text).await;
                    let _ = ui.send(UiEvent::Distilled { draft: p.display().to_string() });
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
        self.metrics.turns_started += 1;
        let _ = self.ui.send(UiEvent::TurnStarted { turn: id });

        let ctx = TurnCtx {
            id,
            token,
            core: CoreHandle::new(self.self_tx.clone()),
            ui: self.ui.clone(),
            models: self.models.clone(),
            registry: self.registry.clone(),
            memory: self.memory.clone(),
            tool_config: self.tool_config,
            context_limit: self.context_limit,
            mode: self.mode,
        };
        // 每轮开头把上下文占用报给状态栏（R5/R6 的展示面）。
        let _ = self.ui.send(UiEvent::ContextFootprint {
            total: toks(&self.memory.prompt_block())
                + toks(&self.memory.playbook.catalog())
                + self.history.iter().map(|m| toks(&m.content)).sum::<u32>(),
            cacheable: toks(&self.memory.prompt_block()) + toks(&self.memory.playbook.catalog()),
            turns: self.history.len(),
        });
        let back = CoreHandle::new(self.self_tx.clone());
        tokio::spawn(async move {
            let outcome = run_turn(ctx).await;
            back.finished(id, outcome).await;
        });
    }
}
