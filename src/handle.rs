//! `CoreHandle`：所有人跟 Core 说话的唯一入口。
//!
//! turn task 和 UI 都只拿得到这个 handle，拿不到 Core 本身——
//! 「Core 是唯一写权」这条原则靠的就是没有第二条路径能碰到 `Core` 的字段。
//!
//! # 所有方法返回 `Option`，`None` 一律表示「Core 已经没了」
//!
//! Core 退出后 oneshot 的发送端被 drop，这里的 `recv` 会失败。
//! turn task 收到 `None` 的正确反应是**立刻收尾退出**，不要重试——
//! Core 没了就没人能接收它的 `TurnFinished` 了，重试只会挂住。

use crate::ids::{TurnId, Version};
use crate::model::{Message, Role, Usage};
use crate::msg::{ApplyReport, CoreMsg, SendMode, TurnOutcome, UserMsg};
use crate::state::{Op, Patch, Phase, StateView};
use std::sync::Arc;
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

    // ── turn task 侧 ──

    /// 取一份**不可变**快照。
    ///
    /// 拿到的是快照不是引用：接下来 await 的几十秒里用户可能已经改过侧栏了，
    /// 所以任何「读—改—写」都必须走 [`Self::propose`] 让 Core 裁决，
    /// 不能自己算完整份写回去。
    pub async fn snapshot(&self) -> Option<Arc<StateView>> {
        self.ask(|reply| CoreMsg::Snapshot { reply }).await
    }

    /// 取走 inbox 里攒着的用户插话。返回空 vec 表示没人说话（不是错误）。
    pub async fn take_injections(&self, turn: TurnId) -> Option<Vec<UserMsg>> {
        self.ask(|reply| CoreMsg::TakeInjections { turn, reply }).await
    }

    /// 报告本轮判定的场景。Core 只存下来给 UI 显示，**不据此改变流程**。
    ///
    /// 这里曾经是 `try_intervene`：turn 要向 Core 申请「打扰用户一次」的许可。
    /// 那个许可制随打扰预算一起删了 —— 模型选一个合适的做法本来就是它的工作。
    pub async fn scene_decided(
        &self,
        turn: TurnId,
        scene: impl Into<String>,
        rationale: impl Into<String>,
    ) {
        let _ = self
            .tx
            .send(CoreMsg::SceneDecided {
                turn,
                scene: scene.into(),
                rationale: rationale.into(),
            })
            .await;
    }

    /// 折叠早期对话。历史归 Core 持有，替换必须走这一步，否则两边会漂。
    pub async fn compact_history(
        &self,
        turn: TurnId,
        summary: Message,
        keep: Vec<Message>,
        folded: usize,
    ) {
        let _ = self
            .tx
            .send(CoreMsg::CompactHistory { turn, summary, keep, folded })
            .await;
    }

    /// 更新待落定问题与搁置区。
    pub async fn questions(&self, turn: TurnId, open: Vec<String>, parked: Vec<String>) {
        let _ = self.tx.send(CoreMsg::Questions { turn, open, parked }).await;
    }

    /// 提交模型的改动。返回的 [`ApplyReport`] 里的 `rejected` **必须回喂给模型**。
    pub async fn propose(&self, patch: Patch) -> Option<ApplyReport> {
        self.ask(|reply| CoreMsg::Propose { patch, reply }).await
    }

    pub async fn cost(&self, turn: TurnId, role: Role, usage: Usage) {
        let _ = self.tx.send(CoreMsg::Cost { turn, role, usage }).await;
    }

    pub async fn finished(&self, turn: TurnId, outcome: TurnOutcome) {
        let _ = self.tx.send(CoreMsg::TurnFinished { turn, outcome }).await;
    }

    // ── UI 侧 ──

    /// 返回生成的 `client_id`。UI 重发同一条时必须复用它，Core 据此去重。
    pub async fn user_input(&self, text: impl Into<String>, mode: SendMode) -> Option<Uuid> {
        let client_id = Uuid::new_v4();
        self.tx
            .send(CoreMsg::UserInput { client_id, text: text.into(), mode })
            .await
            .ok()?;
        Some(client_id)
    }

    /// 用同一个 `client_id` 重发，用于测试去重路径。
    pub async fn user_input_with_id(
        &self,
        client_id: Uuid,
        text: impl Into<String>,
        mode: SendMode,
    ) -> Option<()> {
        self.tx
            .send(CoreMsg::UserInput { client_id, text: text.into(), mode })
            .await
            .ok()
    }

    /// 用户编辑：**立即 apply + 立即落盘，但不推进正在跑的 turn**。
    ///
    /// 那一轮已经取过快照了，用户的修改随下一轮上下文一起发出去。
    /// 用户的修改行为不可预知，让模型在一轮之内追着变化跑没有好处。
    pub async fn user_edit(&self, base_version: Version, ops: Vec<Op>) -> Option<()> {
        let patch = Patch::user(TurnId::NONE, base_version, ops);
        self.tx.send(CoreMsg::UserEdit { patch }).await.ok()
    }

    pub async fn interrupt(&self, turn: TurnId) -> Option<()> {
        self.tx.send(CoreMsg::Interrupt { turn }).await.ok()
    }

    /// phase 闸门在用户手里。
    pub async fn advance_phase(&self, to: Phase) -> Option<()> {
        self.tx.send(CoreMsg::AdvancePhase { to }).await.ok()
    }

    /// 用户一键更换本轮场景。
    pub async fn override_scene(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Option<()> {
        self.tx
            .send(CoreMsg::OverrideScene { from: from.into(), to: to.into() })
            .await
            .ok()
    }

    /// 一键蒸馏：把这段会话沉淀成持久层的更新草稿。**用户操作，不是自动行为。**
    pub async fn distill(&self) -> Option<()> {
        self.tx.send(CoreMsg::Distill).await.ok()
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(CoreMsg::Shutdown).await;
    }
}
