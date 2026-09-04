//! 三条通道上跑的消息，以及三种读视图。
//!
//! ```text
//!   UI ──CoreMsg──▶ Core ──spawn──▶ TurnTask ──CoreMsg──▶ Core
//!    ▲                                            │
//!    └────────── UiEvent (broadcast) ◀───────────┘
//!                       Core ──WriteJob──▶ Writer ──▶ Store
//! ```
//!
//! # 三条通道的背压语义各不相同，这是刻意的
//!
//! - **CoreMsg：有界 mpsc。** 模型流式吐得比 Core 处理快时，生产者在 `send` 上
//!   自然等待。Core 的 handle 是微秒级的，所以这个等待永远很短。
//! - **UiEvent：broadcast，有损。** 慢 UI 绝不允许拖住 Core。每条事件都带 `seq`，
//!   所以订阅者收到 `Lagged` 时的正确反应是**按 seq 范围向 store 补拉**，
//!   而不是像上一版那样「重取快照」—— 正文 chunk 不在快照里，重取补不回来。
//! - **WriteJob：无界 mpsc。** 写盘任务不能丢。用无界是因为 Core 不能阻塞，
//!   而事件的产生速率上限是模型的吐字速率。
//!
//! # turn 往 Core 发的东西收敛成了一个 [`Emit`]
//!
//! 上一版有 `scene_decided` / `questions` / `compact_history` / `cost` 四个签名各异的
//! 方法，加上 `propose` 走另一套 patch 机制。它们本来是同一件事：**往主时间线追加一条**。
//! 现在是一个方法一个枚举，加新事件只动 [`Emit`] 与 [`crate::event::Body`] 两处。

use crate::event::{Body, Event};
use crate::ids::{Seq, SessionId, TaskId, TurnId};
use crate::model::{Call, Message, Mode, Role, Usage};
use crate::scene::SceneId;
use crate::state::{Op, Path, Phase, Workspace};
use std::sync::Arc;
use uuid::Uuid;

/// 用户中途说话时的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// **默认**。排队，等 turn 跑到检查点自取。
    ///
    /// 选它作默认的理由是成本：打断会让本轮已经花掉的 prompt 钱白花，
    /// 而排队最多让用户多等一个检查点。
    Queue,
    /// 打断当前 turn 并立刻把这句话作为新 turn 的输入。
    InterruptAndSend,
}

/// 落盘确认。写入事务提交后才回。
#[derive(Debug, Clone)]
pub struct Ack {
    pub seq: Seq,
    /// false = 已进内存与 UI，但落盘还没确认。UI 应保持「发送中」。
    pub durable: bool,
}

// ───────────────────────────── 读视图 ─────────────────────────────

/// 给 UI 的当前状态。**小**：不含历史，所以每次状态变化都可以整份推。
///
/// 上一版的 `StateView` 把完整会话历史也塞在里面，于是 turn 每轮、用户每编辑一次
/// 都要深拷贝全部消息。现在历史归 store，UI 按 seq 范围分页拉。
#[derive(Debug, Clone)]
pub struct Snap {
    pub session: SessionId,
    pub seq: Seq,
    pub ws: Workspace,
    /// 当前场景。**派生自时间线最近一条 `Judged` / `SceneOverridden`**，不存在 Workspace 里。
    pub scene: Option<SceneId>,
    /// 还没被回答的提问。恢复后 UI 要把它们重新画出来。
    pub open_questions: Vec<OpenQuestion>,
    pub turn: Option<TurnId>,
    pub queued: usize,
}

#[derive(Debug, Clone)]
pub struct OpenQuestion {
    pub id: Seq,
    pub question: String,
    pub options: Vec<String>,
}

/// turn 每次拼 prompt 前取的**当前视图**。
///
/// # 上一版这里错得很典型
///
/// 它当时叫 `TurnInput`，轮初取一次、整轮不再重取。于是：判断段推断出的东西经
/// `Emit::Infer` 进了 Core 的 workspace，**回答段却还在用轮初冻结的那份**——
/// 判断段刚写下的结论，回答段看不见。这正是上一版声称要消灭的
/// 「同一份数据在同一时刻有两个值」，只不过换了个位置又犯了一遍。
///
/// 现在的规则很简单：**Workspace 是即时工作台，消费者拿到的永远是最新的那份。**
/// 每次要拼 prompt 就调 [`crate::handle::CoreHandle::view`] 取一次，
/// 不缓存、不冻结、不自己在 turn 里攒一份平行的副本。
///
/// 取一次的开销：`events` 是 `Arc` clone（O(1)），`ws` 是一张几十项的表的 clone。
/// 相对一次几十秒的模型调用可以忽略。
///
/// # 这不等于「用户编辑会重新驱动 turn」
///
/// 两回事。用户中途改侧栏**不会**打断、不会让 turn 回退重跑 —— 流程上它只是
/// 进了 `turn_edits` 参与仲裁。但下一次拼 prompt 时，图上就是他改过的值。
/// 拼 prompt 用的是拼那一刻的真实状态，这跟「要不要为它重启流程」是两个问题。
pub struct TurnView {
    pub turn: TurnId,
    /// 取这份视图时的位置。
    pub seq: Seq,
    /// 本轮起点。`field.seq > turn_start` 就是本轮新写的。
    pub turn_start: Seq,
    /// **当前**推断图，不是轮初的。
    pub ws: Workspace,
    /// **当前**完整时间线，含本轮已经追加的一切（含刚吸收的插话）。
    ///
    /// turn 不再自己维护 `local: Vec<Event>` 把插话拼进去 —— 那些事件在用户按下
    /// 发送时就已经进时间线了，再抄一份到 turn 里只会造出第二个可能不同步的副本。
    pub events: Arc<Vec<Event>>,
    pub mode: Mode,
    /// 用户点过「换成这个场景」⇒ 本轮以它为准。Core 只在本轮第一次取视图时给出。
    ///
    /// 换场景不只是改一个标签：新场景的 guidance 与案例要真的注入进去。
    pub scene_override: Option<SceneId>,
}

/// 检查点上取回的插话情况。
///
/// **刻意不返回事件本身。** 上一版返回 `Vec<Event>`，turn 顺手就把它们拼进了
/// 自己的局部时间线 —— 而那些事件早就在主时间线上了。返回一个计数，
/// 结构上就不可能再犯那个错：turn 只能拿它做流程判断（要不要重走判断段），
/// 内容一律从 [`TurnView::events`] 来。
#[derive(Debug, Clone, Copy, Default)]
pub struct Injected {
    /// 用户随口说的。
    pub said: u32,
    /// 对模型提问的回答。语义不同，分开计，也分开进 stats。
    pub answered: u32,
}

impl Injected {
    pub fn total(&self) -> u32 {
        self.said + self.answered
    }
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

// ───────────────────────────── turn → Core ─────────────────────────────

/// turn 要往主时间线上追加的一条。
///
/// 全部经由 `CoreHandle::emit` 一个方法。Core 负责分配 seq、建立 `corr` 关联、
/// 物化进 workspace、推 UI、丢给 writer —— turn 不碰这些。
#[derive(Debug, Clone)]
pub enum Emit {
    /// 判断段的结论。它不进 prompt：它**决定了拼什么**。
    Judged { scene: SceneId, rationale: String },
    /// 助手正文。
    Wrote { text: String, interrupted: bool },
    /// 助手发起工具调用。Core 记下 `call_id → seq` 以便后面的返回能连上。
    Called { text: String, calls: Vec<Call> },
    /// 模型通过 `ask_user` 提问。Core 自动把 `corr` 指向刚才那条 `Called`。
    Asked { call_id: String, question: String, options: Vec<String> },
    /// 工具返回。Core 按 `call_id` 找到发起它的那条 `Called` 建立 `corr`。
    Returned { call_id: String, name: String, content: String, outcome: String, task: TaskId },
    /// 未返回就被打断的调用。补上它，下一次请求才不会被 API 拒掉。
    Aborted { call_id: String, why: String },
    /// harness 插进对话的一句 system 话。
    Noted { text: String },
    /// 模型推断出的改动。**Core 会先仲裁**：撞上本轮用户改过的路径就丢弃，
    /// 时间线上只记生效的部分，被丢的通过 [`Applied::dropped`] 回给 turn。
    Infer { ops: Vec<Op> },
    /// 折叠早期对话。
    Folded { from: Seq, to: Seq, summary: String, folded: u32 },
    Cost { role: Role, task: Option<TaskId>, usage: Usage },
}

/// `emit` 的回执。
///
/// `events` 是 Core 真正追加进时间线的那几条，**原样回给 turn**。
/// turn 靠它维护本轮的局部时间线来组装下一次 prompt —— 而不是自己再写一遍
/// 「Emit 变成什么 Body」的映射。那种两处映射迟早会漂。
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub seq: Seq,
    /// 被 turn 内仲裁丢掉的路径。**必须回喂给模型**，否则它下一轮会原样再提交一次。
    pub dropped: Vec<Path>,
    pub events: Vec<Event>,
}

impl Applied {
    /// 渲染成回喂给模型的一句话。空则返回 None。
    pub fn feedback(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        let items =
            self.dropped.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
        Some(format!(
            "你对下面这些字段的改动没有生效，因为用户在这一轮里刚改过它们：\n{items}\n\
             推断图里的现值就是用户的值，别再提交同样的改动。"
        ))
    }
}

/// 一轮的产物。
pub struct TurnOutcome {
    pub aborted: bool,
    pub stats: TurnStats,
}

/// 一轮的可观测指标。**落进时间线的 `TurnClosed`**，不再是收下即丢弃。
///
/// 要印进 UI 和实验报告的：
/// - `judge_ms` / `answer_ms`：判断段的开销占比，支撑「两段式不贵」；
/// - `asked_user`：**验收方案的副指标「干预次数」**。防的是「每次问 20 个问题」
///   这种能在主指标上作弊的平凡策略；
/// - `injections`：用户中途插话被吸收了几条，验收「插话必有响应」；
/// - `dropped_ops`：模型撞上用户改动被丢掉几条，反映用户在轮中改得有多勤。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TurnStats {
    pub judge_ms: u64,
    pub answer_ms: u64,
    pub tool_ms: u64,
    pub loops: u32,
    pub injections: u32,
    pub answers: u32,
    pub asked_user: u32,
    pub compacted: u32,
    pub scene: String,
    pub scene_unknown: bool,
    pub scene_overridden: bool,
    pub tools_run: u32,
    pub tools_timeout: u32,
    pub tools_interrupted: u32,
    pub tools_failed: u32,
    pub heartbeats: u32,
    pub inferred_ops: u32,
    pub dropped_ops: u32,
}

// ───────────────────────────── Core 的信箱 ─────────────────────────────

pub enum CoreMsg {
    // ── 来自 UI ──
    Send {
        client_id: Uuid,
        text: String,
        mode: SendMode,
        /// 非 None ⇒ 这是对某条提问的回答，语义上与随口说一句不同。
        answering: Option<Seq>,
        reply: tokio::sync::oneshot::Sender<Option<Ack>>,
    },
    /// 用户点了 apply。
    ///
    /// **apply 之前 UI 自己管，Core 一无所知。** 用户在侧栏里改到一半、
    /// 改了又撤销，都不该产生事件 —— 上一版每次改动立即写盘，把 UI 的中间态
    /// 灌进了主时间线。
    Edit {
        ops: Vec<Op>,
        reply: tokio::sync::oneshot::Sender<Option<Ack>>,
    },
    /// 阶段闸门。**只从 UI 来。**
    AdvancePhase { to: Phase },
    /// 用户一键更换本轮场景。下一轮以它为准并注入对应的 guidance 与案例。
    OverrideScene { to: SceneId },
    /// 一键蒸馏：把这段会话沉淀进持久层。**用户操作，不是自动行为。**
    Distill,
    /// R5：从某一轮分叉出一个新会话。**回滚不是截断。**
    ///
    /// 原会话一条事件都不动 —— 那条路走过就走过了，它是「试过没成立的方法」的
    /// 原始记录，而这恰恰是这个项目里最值钱的一类数据。分叉出来的新会话由调用方
    /// 用返回的 id 另起一个 Core 打开。
    Fork {
        before_turn: TurnId,
        title: String,
        reply: tokio::sync::oneshot::Sender<Result<SessionId, String>>,
    },
    Interrupt { turn: TurnId },
    Snapshot { reply: tokio::sync::oneshot::Sender<Snap> },
    /// 落盘屏障：等 writer 把攒着的 best-effort 批次冲干净。
    ///
    /// 生产上用于「用户要看一眼已保存状态」和退出前的确认；
    /// 测试里用于在断言盘上内容之前消除攒批带来的时序不确定。
    Flush { reply: tokio::sync::oneshot::Sender<bool> },
    Shutdown { reply: tokio::sync::oneshot::Sender<()> },
    /// 退出宽限期到了，不再等 turn 收尾。由 Core 自己的超时 task 发。
    ShutdownNow,

    // ── 来自 TurnTask ──
    /// 取**当前**视图。turn 每次拼 prompt 前调一次。
    View {
        turn: TurnId,
        reply: tokio::sync::oneshot::Sender<Option<Box<TurnView>>>,
    },
    /// 取走 inbox 里攒着的用户插话，**只返回计数**。
    ///
    /// 内容不在这里给：那些事件在用户按下发送时就进时间线了，
    /// turn 下一次取 [`TurnView`] 自然就看到。
    ///
    /// 只有**正在运行**的 turn 能取。一个正在收敛的 turn 把用户的新输入吞掉，
    /// 那句话就永远没人回了 —— 这是很隐蔽的一类丢消息。
    TakeInjections {
        turn: TurnId,
        reply: tokio::sync::oneshot::Sender<Injected>,
    },
    Emit {
        turn: TurnId,
        emit: Emit,
        reply: tokio::sync::oneshot::Sender<Applied>,
    },
    Finished { turn: TurnId, outcome: TurnOutcome },
}

// ───────────────────────────── 出向 ─────────────────────────────

/// 发给 UI 的事件。有损通道；每条带 `seq`，跳号就按范围向 store 补拉。
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 主时间线追加了一条。UI 的增量渲染入口。
    Appended { seq: Seq, turn: Option<TurnId>, body: Box<Body> },
    /// 流式正文片段。**不进主时间线**（整段完成后由 `Wrote` 落一条），
    /// 但带上它所属的 turn，UI 才知道该往哪个气泡里追加。
    Delta { turn: TurnId, text: String },
    TurnStarted { turn: TurnId },
    /// 用户在 turn 运行中发了话，已排队。
    Queued { pending: usize },
    /// 打断的**第一段反馈**：用户点了停就该马上看到反应，不等子任务收敛。
    Stopping { turn: TurnId },
    /// 打断的**第二段反馈**：子任务真的收敛完了。
    TurnClosed { turn: TurnId, aborted: bool },
    /// 已提交状态变了。带上生效的 ops，UI 可以直接应用，不必回头拉快照。
    StateChanged { seq: Seq, ops: Vec<Op>, dropped: Vec<Path> },
    /// 长任务心跳：语义是「仍在进行」，不是「快好了」。
    StillRunning { pending: Vec<String>, elapsed_ms: u64 },
    TaskDone { idx: usize, name: String },
    /// 早期对话被折叠了。**告知，不是征求同意。**
    Compacted { folded: u32, before_tokens: u32, after_tokens: u32 },
    /// 一键蒸馏完成，草稿写在这个路径。
    Distilled { draft: String },
    /// 上下文占用，供状态栏显示。
    ContextFootprint { total: u32, cacheable: u32, events: usize },
    PhaseChanged { to: Phase },
    CostTick { role: Role, usage: Usage, session_total: u32 },
    /// 落盘连续失败。**不回滚内存状态** —— 让用户刚打的字消失比暂时没落盘更糟。
    /// 状态栏应常亮直到 `PersistOk`。
    PersistDegraded { why: String, pending: usize },
    PersistOk,
    /// 持久层文件解析失败，已回退到内置内容。
    ///
    /// 「文件就是界面」的代价：用户改坏了 `playbook.toml`，必须像界面报错一样报出来，
    /// 不能只 eprintln 然后静悄悄用内置目录继续跑。
    MemoryDegraded { file: String, err: String },
    /// 上次异常退出留下的痕迹，已修补。
    Recovered { events: usize, crashed_turns: usize, reopened_questions: usize },
    /// 从某一轮分叉出了一个新会话。原会话不受影响。
    Forked { session: SessionId, from_turn: TurnId, at: Seq },
}

/// 落盘任务。
pub enum WriteJob {
    /// 追加一批事件。`ack` 非 None ⇒ 这是 must-flush，写入事务提交后才回执。
    Append {
        evs: Vec<Event>,
        ack: Option<tokio::sync::oneshot::Sender<bool>>,
    },
    /// 打一个 checkpoint。**纯功能性** —— 失败只记告警，不影响任何正确性。
    Checkpoint { session: SessionId, seq: Seq, ws: Box<Workspace> },
    /// 分叉出来的新会话的元信息。
    NewSession { session: crate::ids::Session },
    Shutdown { ack: tokio::sync::oneshot::Sender<()> },
}

/// 组装 prompt 需要的消息序列。放在这里是因为它跨了 event 与 model 两个模块。
pub fn messages_of(events: &[Event]) -> Vec<Message> {
    crate::event::assemble(events)
}
