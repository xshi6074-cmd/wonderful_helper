//! 三条通道上跑的所有消息。
//!
//! ```text
//!   UI ──CoreMsg──▶ Core ──spawn──▶ TurnTask ──CoreMsg──▶ Core
//!    ▲                                            │
//!    └────────── UiEvent (broadcast) ◀───────────┘
//!                       Core ──WriteJob──▶ Writer
//! ```
//!
//! # 三条通道的背压语义各不相同，这是刻意的
//!
//! - **CoreMsg：有界 mpsc。** 模型流式吐得比 Core 处理快时，生产者在 `send` 上
//!   自然等待。Core 的 handle 是微秒级的，所以这个等待永远很短。
//! - **UiEvent：broadcast，有损。** 慢 UI 绝不允许拖住 Core。订阅者收到
//!   `RecvError::Lagged` 时的正确反应是**重新取一份快照**，而不是试图补齐丢掉的事件。
//! - **WriteJob：无界 mpsc。** 写盘任务不能丢——丢了就是数据不一致。
//!   用无界是因为 Core 不能阻塞，而写盘任务的产生速率是人的操作速率。
//!
//! # 这一版删掉的消息
//!
//! `TryIntervene` / `InterveneGrant` / `DenyReason` 随打扰预算一起删除。
//! `CardsFired` / `CardVerdict` 随判断卡一起删除 —— 判断段的产物是场景判定，
//! 不是「抽中了哪张卡」。

use crate::ids::{TurnId, Version};
use crate::model::{Message, Role, Usage};
use crate::state::{Path, Phase, Workspace};
use uuid::Uuid;

/// 用户的一条输入。
#[derive(Debug, Clone)]
pub struct UserMsg {
    pub client_id: Uuid,
    pub text: String,
    /// 进入 inbox 时的 Core 版本，排错用。
    pub at: Version,
}

/// 用户中途说话时的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// **默认**。排队，等 turn 跑到检查点自取。
    ///
    /// 选它作默认的理由是成本：打断会让本轮已经花掉的 prompt 钱白花，
    /// 而排队最多让用户多等一个检查点。用户真想纠正时再显式打断。
    Queue,
    /// 打断当前 turn 并立刻把这句话作为新 turn 的输入。
    InterruptAndSend,
}

/// 发给 Core 的所有消息。
///
/// **每一条来自 TurnTask 的消息都带 `turn: TurnId`**，Core 对不上代际就丢弃。
/// 被取消的 subagent 仍然会跑完并把事件发回来，这是唯一的防线。
pub enum CoreMsg {
    // ── 来自 UI ──
    UserInput {
        client_id: Uuid,
        text: String,
        mode: SendMode,
    },
    /// 用户编辑侧栏。
    ///
    /// **不传入正在跑的 turn。** 立即写进 state 并落盘（UI 要即时反映、崩了不能丢），
    /// 但当前 turn 不会中途重取快照去感知它 —— 用户的修改行为不可预知，
    /// 让模型在一轮之内追着变化跑没有好处。下一轮组装上下文时随对话一起发出去。
    UserEdit {
        patch: crate::state::Patch,
    },
    /// 阶段闸门。**只从 UI 来**：模型可以建议收尾，不能自己推进。
    AdvancePhase {
        to: Phase,
    },
    /// 用户一键更换本轮场景。记录下来用于回头修正场景描述。
    OverrideScene {
        from: String,
        to: String,
    },
    /// 一键蒸馏：把这段会话沉淀进持久层。
    ///
    /// **是用户对一个对话的选择操作，不是自动行为。** 模型不能自己决定
    /// 「这段话值得写进你的长期记忆」—— 那是越权。产出写成草稿文件，
    /// 用户看过改过再合并，见 [`crate::memory::draft_path`]。
    Distill,
    Interrupt {
        turn: TurnId,
    },
    Shutdown,

    // ── 来自 TurnTask ──
    Snapshot {
        reply: tokio::sync::oneshot::Sender<std::sync::Arc<crate::state::StateView>>,
    },
    /// 取走 inbox 里攒着的用户插话。
    ///
    /// 只有**正在运行**的 turn 能取（不是 Closing 的）。一个正在收敛的 turn
    /// 把用户的新输入吞掉，那句话就永远没人回了——这是很隐蔽的一类丢消息。
    TakeInjections {
        turn: TurnId,
        reply: tokio::sync::oneshot::Sender<Vec<UserMsg>>,
    },
    /// 判断段判定的场景。Core 只是存下来给 UI 显示，**不据此改变任何流程**。
    SceneDecided {
        turn: TurnId,
        scene: String,
        rationale: String,
    },
    /// 待落定问题与搁置区的更新。
    ///
    /// 决策：它们**不走 pending/commit** 那套 diff 机制，直接写。理由是这两个列表是
    /// 便签性质的，覆盖掉的代价远低于为它们再复制一遍冲突裁决的代码。
    /// 真正需要事务语义的是推断图，那个走 `Propose`。
    Questions {
        turn: TurnId,
        open: Vec<String>,
        parked: Vec<String>,
    },
    Propose {
        patch: crate::state::Patch,
        reply: tokio::sync::oneshot::Sender<ApplyReport>,
    },
    /// 折叠早期对话。turn 侧算完摘要后提交，Core 用 `[摘要] + keep` 替换历史。
    ///
    /// 由 Core 而不是 turn 持有历史，所以替换必须走这一步 —— 否则两边会漂。
    CompactHistory {
        turn: TurnId,
        summary: Message,
        keep: Vec<Message>,
        folded: usize,
    },
    Cost {
        turn: TurnId,
        role: Role,
        usage: Usage,
    },
    TurnFinished {
        turn: TurnId,
        outcome: TurnOutcome,
    },
}

/// 裁决结果。
#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub applied: Vec<Path>,
    /// **必须回喂给模型。**
    ///
    /// 下一轮上下文里要带一句「你对 X 的修改没有生效，因为用户刚改过」，
    /// 否则模型会反复尝试改同一个字段，每轮白花一次钱。
    pub rejected: Vec<(Path, Reason)>,
    pub version: Version,
}

impl ApplyReport {
    /// 把 rejected 渲染成回喂给模型的一句话。空则返回 None。
    pub fn feedback(&self) -> Option<String> {
        if self.rejected.is_empty() {
            return None;
        }
        let items = self
            .rejected
            .iter()
            .map(|(p, r)| format!("- {p}：{}", r.explain()))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!("你上一步的部分改动没有生效：\n{items}\n用户的值是当前值，别再提交同样的改动。"))
    }
}

/// Core 拒绝一个模型 op 的理由。
///
/// **只有一种。** 早期还有 `Locked`（用户锁），已随 `locked: HashSet<Path>` 一起删除。
/// 剩下的这一种不是策略，是 lost-update 防护。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 模型取快照之后、提交之前，用户动过同一路径。
    Stale,
}

impl Reason {
    pub fn explain(&self) -> &'static str {
        match self {
            Reason::Stale => "在你读取之后用户改过这个字段，你的版本已过期",
        }
    }
}

/// 一轮的产物。
pub struct TurnOutcome {
    pub msgs: Vec<Message>,
    pub aborted: bool,
    pub stats: TurnStats,
}

/// 一轮的可观测指标。
///
/// 要印进 UI 和实验报告的：
/// - `judge_ms` / `answer_ms` + 分角色 token：判断段的开销占比，支撑「两段式不贵」；
/// - `asked_user`：**验收方案里的副指标「干预次数」**。防的是「每次问 20 个问题」
///   这种能在主指标上作弊的平凡策略。注意它现在统计的是**模型自己选择提问**的次数，
///   不是 harness 批准了几次打扰 —— 后者随打扰预算一起没了；
/// - `injections`：用户中途插话被吸收了几条，验收「插话必有响应」；
/// - `heartbeats`：长任务期间给用户发了几次「仍在进行」。
#[derive(Debug, Clone, Default)]
pub struct TurnStats {
    pub judge_ms: u64,
    pub answer_ms: u64,
    pub tool_ms: u64,
    pub loops: u32,
    pub injections: u32,
    /// 模型主动调用 `ask_user` 的次数。
    pub asked_user: u32,
    /// 本轮折叠了多少条早期消息（0 = 没触发压缩）。
    pub compacted: u32,
    pub scene: String,
    pub scene_unknown: bool,
    pub tools_run: u32,
    pub tools_timeout: u32,
    pub tools_interrupted: u32,
    pub tools_failed: u32,
    pub heartbeats: u32,
    pub proposed_ops: u32,
    pub rejected_ops: u32,
}

/// 发给 UI 的事件。有损通道，UI 收到 Lagged 时应重新取快照。
#[derive(Debug, Clone)]
pub enum UiEvent {
    TurnStarted { turn: TurnId },
    /// 用户在 turn 运行中发了话，已排队。
    Queued { pending: usize },
    /// 打断的**第一段反馈**：用户点了停就该马上看到反应，不等子任务收敛。
    Stopping { turn: TurnId },
    /// 打断的**第二段反馈**：子任务真的收敛完了。
    TurnClosed { turn: TurnId, aborted: bool },
    Delta(String),
    /// 本轮判成了哪个场景、为什么。UI 侧栏显示 + 提供一键更换。
    SceneChosen { turn: TurnId, scene: String, rationale: String },
    /// 模型调用了 `ask_user`：UI 把它渲染成选择题。
    ///
    /// 这不是 harness 的抢占 —— 是模型自己决定用这个形式提问。
    Choice { question: String, options: Vec<String> },
    /// 长任务心跳：语义是「仍在进行」，不是「快好了」。
    StillRunning { pending: Vec<String>, elapsed_ms: u64 },
    TaskDone { idx: usize, name: String },
    /// 已提交状态变了（用户编辑 / turn 提交）。
    StateChanged(ApplyReport),
    /// 未提交的模型改动变了 —— 侧栏先亮起来，不等 turn 结束。
    PendingChanged(ApplyReport),
    QuestionsChanged { open: usize, parked: usize },
    /// 早期对话被折叠了。**告知，不是征求同意** —— 征求同意就是一次不该有的打扰。
    Compacted { folded: usize, before_tokens: u32, after_tokens: u32 },
    /// 一键蒸馏完成，草稿写在这个路径。
    Distilled { draft: String },
    /// 上下文占用，供状态栏显示。
    ContextFootprint { total: u32, cacheable: u32, turns: usize },
    PhaseChanged { to: Phase },
    CostTick { role: Role, usage: Usage, session_total: u32 },
}

/// 落盘任务。**顺序由单一 writer task 保证。**
///
/// turn 结束时 Core 按 ① AppendHistory → ② 提交 pending → ③ WriteSnapshot 的顺序发送。
/// 先写日志再改状态：崩溃点的最坏情况是「历史里有但 state 没应用」，重放即可；
/// 反过来（先改状态后写日志）就会丢。
pub enum WriteJob {
    /// `commits` 是这一轮**即将被提交**的状态改动。
    ///
    /// 它必须和消息一起写进历史，否则「先写日志再改状态、崩了重放即可」这句话是假的：
    /// 日志里只有对话没有状态改动的话，崩在 ①② 之间就没得可重放。
    AppendHistory {
        turn: TurnId,
        version: Version,
        msgs: Vec<Message>,
        commits: Vec<crate::state::Op>,
    },
    WriteSnapshot {
        version: Version,
        ws: Box<Workspace>,
    },
    Shutdown,
}
