//! 跟 Core 说话的入口。**按调用方拆成两组**，不再是一个大杂烩。
//!
//! - [`CoreHandle::session_*`] 那组给 UI：命令 + 快照 + 订阅。
//! - 其余给 turn task：输入、插话、追加、收尾。
//!
//! 拆开的理由不是洁癖：上一版 UI 侧和 turn 侧的方法混在同一个 impl 里，
//! 于是 UI 能调 `propose`、turn 能调 `advance_phase`。「阶段闸门只在用户手里」
//! 这条约束靠的是没人去调它，而不是靠类型。
//!
//! 历史的读取**不走这里**：UI 直接持一份 `Arc<dyn Store>` 按 seq 范围分页拉。
//! 让几百条消息穿过 Core 的信箱，只会把一个微秒级的 actor 变成搬运工。
//!
//! # 所有方法返回 `Option`，`None` 一律表示「Core 已经没了」
//!
//! Core 退出后 oneshot 的发送端被 drop，这里的 `recv` 会失败。
//! turn task 收到 `None` 的正确反应是**立刻收尾退出**，不要重试。

use crate::ids::{Seq, TurnId};
use crate::model::{Role, Usage};
use crate::ids::SessionId;
use crate::msg::{Ack, Applied, CoreMsg, Emit, Injected, SendMode, Snap, TurnOutcome, TurnView};
use crate::scene::SceneId;
use crate::state::{Op, Phase};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::Sender<CoreMsg>,
}

impl CoreHandle {
    pub fn new(tx: mpsc::Sender<CoreMsg>) -> Self {
        Self { tx }
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> CoreMsg) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).await.ok()?;
        rx.await.ok()
    }

    // ───────────────────────── turn task 侧 ─────────────────────────

    /// 取**当前**视图。**每次拼 prompt 前都要调一次。**
    ///
    /// 上一版这里叫 `input()`，轮初取一次就冻住 —— 于是判断段写进 workspace 的
    /// 推断，回答段看不见。Workspace 是即时工作台，消费者拿到的必须是最新的那份。
    pub async fn view(&self, turn: TurnId) -> Option<Box<TurnView>> {
        self.ask(|reply| CoreMsg::View { turn, reply }).await.flatten()
    }

    /// 取走 inbox 里攒着的用户插话，**只拿计数**。
    ///
    /// 内容不在这里给：那些事件在用户按下发送时就进时间线了，下一次取
    /// [`Self::view`] 自然看到。只返回计数是结构上的防呆 —— 上一版返回事件本身，
    /// turn 顺手就把它们又拼进了自己的局部副本。
    pub async fn take_injections(&self, turn: TurnId) -> Option<Injected> {
        self.ask(|reply| CoreMsg::TakeInjections { turn, reply }).await
    }

    /// 往主时间线追加一条。**turn 往外发东西只有这一个方法。**
    ///
    /// 返回的 [`Applied::dropped`] 非空时**必须回喂给模型**，否则它下一轮
    /// 会原样再提交一次，白花一次钱。
    pub async fn emit(&self, turn: TurnId, emit: Emit) -> Option<Applied> {
        self.ask(|reply| CoreMsg::Emit { turn, emit, reply }).await
    }

    /// 记一笔账。不关心回执，所以不等。
    pub async fn cost(&self, turn: TurnId, role: Role, usage: Usage) {
        let (tx, _rx) = oneshot::channel();
        let _ = self
            .tx
            .send(CoreMsg::Emit { turn, emit: Emit::Cost { role, task: None, usage }, reply: tx })
            .await;
    }

    /// 轮外的记账（蒸馏、恢复期间的调用）。挂在最近一轮上，账不能因为没轮次就漏。
    pub async fn cost_out(&self, role: Role, usage: Usage) {
        let (tx, _rx) = oneshot::channel();
        let _ = self
            .tx
            .send(CoreMsg::Emit {
                turn: TurnId(u64::MAX),
                emit: Emit::Cost { role, task: None, usage },
                reply: tx,
            })
            .await;
    }

    pub async fn finished(&self, turn: TurnId, outcome: TurnOutcome) {
        let _ = self.tx.send(CoreMsg::Finished { turn, outcome }).await;
    }

    // ───────────────────────────── UI 侧 ─────────────────────────────

    /// 用户说一句话。返回的 [`Ack`] 在**落盘事务提交后**才回：
    /// UI 在此之前把气泡显示成「发送中」，看到已送达就意味着真的在盘上了。
    ///
    /// 返回 `None` 表示这条被去重丢弃了（用户手抖点了两次）。
    pub async fn session_send(&self, text: impl Into<String>, mode: SendMode) -> Option<Ack> {
        self.ask(|reply| CoreMsg::Send {
            client_id: Uuid::new_v4(),
            text: text.into(),
            mode,
            answering: None,
            reply,
        })
        .await
        .flatten()
    }

    /// 用同一个 `client_id` 重发。UI 超时重试时必须这样做，Core 据此去重。
    pub async fn session_resend(
        &self,
        client_id: Uuid,
        text: impl Into<String>,
        mode: SendMode,
    ) -> Option<Ack> {
        self.ask(|reply| CoreMsg::Send {
            client_id,
            text: text.into(),
            mode,
            answering: None,
            reply,
        })
        .await
        .flatten()
    }

    /// 回答模型的提问。
    ///
    /// 与随口说一句**语义不同**：组装 prompt 时会写成「对提问 #N 的回答」，
    /// 模型才认得出这是自己那个问题的答案。上一版两者都退化成一条普通 user 消息。
    pub async fn session_answer(
        &self,
        question: Seq,
        choice: impl Into<String>,
        mode: SendMode,
    ) -> Option<Ack> {
        self.ask(|reply| CoreMsg::Send {
            client_id: Uuid::new_v4(),
            text: choice.into(),
            mode,
            answering: Some(question),
            reply,
        })
        .await
        .flatten()
    }

    /// 用户点了 apply。
    ///
    /// **apply 之前 UI 自己管**：在侧栏里改到一半、改了又撤销，都不产生事件。
    /// apply 之后才是真改动，落盘确认后回 ack。
    pub async fn session_edit(&self, ops: Vec<Op>) -> Option<Ack> {
        self.ask(|reply| CoreMsg::Edit { ops, reply }).await.flatten()
    }

    pub async fn session_snapshot(&self) -> Option<Snap> {
        self.ask(|reply| CoreMsg::Snapshot { reply }).await
    }

    /// 切换 mode。下一轮生效（当前轮已经拼好 prompt 了）。
    pub async fn session_set_mode(&self, to: crate::model::Mode) {
        let _ = self.tx.send(CoreMsg::SetMode { to }).await;
    }

    /// 等攒着的事件全部落盘。返回 false 表示这一批没写成功（此时 UI 上应有降级提示）。
    pub async fn session_flush(&self) -> bool {
        self.ask(|reply| CoreMsg::Flush { reply }).await.unwrap_or(false)
    }

    pub async fn session_interrupt(&self, turn: TurnId) -> Option<()> {
        self.tx.send(CoreMsg::Interrupt { turn }).await.ok()
    }

    /// 阶段闸门在用户手里。turn 侧没有对应的方法。
    pub async fn session_advance_phase(&self, to: Phase) -> Option<()> {
        self.tx.send(CoreMsg::AdvancePhase { to }).await.ok()
    }

    /// 一键更换场景。**下一轮生效** —— 新场景的 guidance 与案例会真的注入进去，
    /// 不只是换个标签。想立刻生效就先打断。
    pub async fn session_override_scene(&self, to: impl Into<SceneId>) -> Option<()> {
        self.tx.send(CoreMsg::OverrideScene { to: to.into() }).await.ok()
    }

    /// 一键蒸馏：把这段会话沉淀成持久层的更新草稿。**用户操作，不是自动行为。**
    pub async fn session_distill(&self) -> Option<()> {
        self.tx.send(CoreMsg::Distill).await.ok()
    }

    /// R5：从某一轮分叉出一个新会话。**原会话一条事件都不动。**
    ///
    /// 返回新会话的 id，调用方用它另起一个 Core 打开分支。
    pub async fn session_fork(
        &self,
        before_turn: TurnId,
        title: impl Into<String>,
    ) -> Option<Result<SessionId, String>> {
        self.ask(|reply| CoreMsg::Fork { before_turn, title: title.into(), reply }).await
    }

    /// 优雅退出：cancel、补齐未闭合调用、把攒着的事件冲干净，然后才返回。
    /// **这一步是「优雅退出」与「被 kill」的全部差别。**
    pub async fn session_shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(CoreMsg::Shutdown { reply: tx }).await.is_ok() {
            let _ = rx.await;
        }
    }
}
