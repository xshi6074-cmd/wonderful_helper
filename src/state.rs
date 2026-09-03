//! Workspace 状态、路径、补丁。
//!
//! # 这里是「锁」被删掉的地方
//!
//! 早期设计里有一个 `locked: HashSet<Path>`：用户碰过的字段就上锁、持久化、
//! 模型的改动撞上锁就被 Core 拒绝，另有 `Unlock` 消息和 UI 上的解锁操作。
//! 这个设计是错的，理由有两条：
//!
//! 1. **用户也会搞错。** 一个持久化的硬锁意味着用户填错一次就永久挡住模型，
//!    只能靠再学一个「解锁」操作救回来。为了一点鲁棒性引入了一个新概念。
//! 2. **它把策略摊进了 core 的好几个地方**：`apply` 里判、`Unlock` 消息里改、
//!    快照里存、恢复时读、UI 里画。改一次语义要动五处。
//!
//! 现在的做法：Core **不做锁、不做策略性拒绝**。「模型改用户填过的字段之前要先问」
//! 这条约束下沉到 prompt（见 [`crate::context`] 里的 `USER_FIELD_RULE`，
//! 配合 [`StateView::prompt_lines`] 给字段打的 `[用户设定]` 标记）。
//! 模型不听话的代价是一轮返工，不是数据被永久污染。
//!
//! [`Field::origin`] 是**出处**（provenance），不是锁 —— Core 从不因为它拒绝任何东西。
//! core 里唯一剩下的拒绝条件是 lost-update 防护（[`crate::msg::Reason::Stale`]），只有一处。
//!
//! # 可选项
//!
//! [`Field::source`] / [`Field::confidence`]（来源与置信度）、以及
//! [`Workspace::open`] / [`Workspace::parked`]（待落定与搁置）都是**可选的**：
//! 不填就完全不出现在 prompt 和 UI 里，也不提示模型去填。
//! 它们要不要成为强制项，等界面真正拉出来之后再定。
//!
//! # 这里也是「打扰预算」被删掉的地方
//!
//! `Budget` 结构整个没了。它试图用限额约束模型打扰用户的次数，但限额一到就让模型
//! 「直接给结论」—— 那是在用降低质量换安静。正确的做法是把不该有的打扰权限逐个掐掉，
//! 掐干净之后就不需要限额。清单见 [`crate::turn`] 头部。

use crate::ids::{TurnId, Version};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Value = serde_json::Value;

/// 字段路径，点分。例：`spec.claim`、`node.generator.output`、`edge.gen->replay`。
///
/// 冲突检查是**字段级**的：用户改了推断图的一个节点，不该让模型对另一个节点的
/// 更新整份失败。所以裁决逐 op 按 path 判，不是整份 CAS。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Path(pub String);

impl Path {
    pub fn new(s: impl Into<String>) -> Self {
        Path(s.into())
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Path {
    fn from(s: &str) -> Self {
        Path(s.to_string())
    }
}

impl From<String> for Path {
    fn from(s: String) -> Self {
        Path(s)
    }
}

/// 值的出处。**不是锁**，见本模块头部说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    User,
    Model,
}

/// 这个推断是从哪来的。设计文档「1 · 推断架构图」要求每个节点与每条边都带来源，
/// **无来源的节点在图上渲染为虚线** —— 那正是用户该警惕的地方。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// 仓库位置，如 `src/model/generator.py:42`
    Repo(String),
    /// 论文章节
    Paper(String),
    /// 用户口述
    User,
    /// 模型推测 —— 渲染成虚线的就是它
    Guess,
}

impl Source {
    /// 是否该渲染成虚线（无实证来源）。没标来源的字段同样算虚线，
    /// 判断在 [`Field::is_dashed`]。
    pub fn is_dashed(&self) -> bool {
        matches!(self, Source::Guess)
    }

    pub fn label(&self) -> String {
        match self {
            Source::Repo(p) => format!("repo:{p}"),
            Source::Paper(p) => format!("paper:{p}"),
            Source::User => "用户口述".into(),
            Source::Guess => "模型推测".into(),
        }
    }
}

/// workspace 里的一个字段（推断图的节点/边属性也走这里）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub value: Value,
    /// 谁最后写的。只用于 prompt 标记与 UI 渲染，Core 从不据此拒绝写入。
    pub origin: Origin,
    /// 依据。**可选**：不填表示没标来源。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// 0.0–1.0。**可选**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// 最后一次被写入时的 Core 版本。
    pub version: Version,
}

impl Field {
    /// 没有实证来源（没标、或标成模型推测）⇒ 图上画虚线。
    pub fn is_dashed(&self) -> bool {
        self.source.as_ref().map(|s| s.is_dashed()).unwrap_or(true)
    }
}

/// 阶段。对应设计文档「进入实现由用户拍板」。
///
/// **推进只能由用户确认**（[`crate::msg::CoreMsg::AdvancePhase`] 只从 UI 来）。
/// 模型可以在正文里建议收尾，但没有推进的权限 —— 这是「agent 不主导流程」的落点。
/// 注意它也**不是**模型的阻断权：模型不能因为「我觉得还没准备好」就拦住用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Phase {
    /// 讨论设计中
    Designing,
    /// 用户已拍板，产出交接 brief
    Handoff,
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Designing => "设计讨论",
            Phase::Handoff => "交接",
        }
    }
}

/// 已提交的 workspace。**只有 Core 持有可变副本。**
///
/// 对应设计文档 state 三层里的 **turn 层**：推断图 · 待落定 · 搁置区 · 本轮场景。
/// （会话层与持久层尚未落地，见 `lib.rs` 的缺口清单。）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub phase: Phase,
    /// 推断图。现在还是平铺的 path→field；节点/边的图结构见缺口清单。
    pub fields: BTreeMap<Path, Field>,
    /// 待落定的问题。**可选**，空则不出现在 prompt 里。
    #[serde(default)]
    pub open: Vec<String>,
    /// 搁置区。**可选**，空则不出现在 prompt 里。
    #[serde(default)]
    pub parked: Vec<String>,
    /// 本轮判定的场景 id，UI 侧栏显示 + 支持一键更换。
    pub scene: Option<String>,
    /// 用户更换场景的记录，用于回头修正场景描述（设计文档「更换记录用于修正动作描述」）。
    pub scene_overrides: Vec<(String, String)>,
}

impl Default for Phase {
    fn default() -> Self {
        Phase::Designing
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn write(
        &mut self,
        path: Path,
        value: Value,
        origin: Origin,
        source: Option<Source>,
        confidence: Option<f32>,
        version: Version,
    ) {
        self.fields.insert(path, Field { value, origin, source, confidence, version });
    }

    pub(crate) fn erase(&mut self, path: &Path) {
        self.fields.remove(path);
    }
}

/// 单个改动。
///
/// 刻意只有 Set / Remove 两种：列表追加（`Append`）会让「同一路径的冲突」失去良定义
/// （两个 Append 互不冲突，但和一个 Set 冲突）。需要改列表时整体 Set 一个数组值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Set {
        path: Path,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
    },
    Remove {
        path: Path,
    },
}

impl Op {
    pub fn path(&self) -> &Path {
        match self {
            Op::Set { path, .. } => path,
            Op::Remove { path } => path,
        }
    }

    /// 不标来源的写入。
    pub fn set(path: impl Into<Path>, value: impl Into<Value>) -> Op {
        Op::Set { path: path.into(), value: value.into(), source: None, confidence: None }
    }

    /// 有实证来源的写入。
    pub fn grounded(path: impl Into<Path>, value: impl Into<Value>, source: Source, c: f32) -> Op {
        Op::Set {
            path: path.into(),
            value: value.into(),
            source: Some(source),
            confidence: Some(c),
        }
    }

    pub fn remove(path: impl Into<Path>) -> Op {
        Op::Remove { path: path.into() }
    }
}

/// 一次提交。
///
/// `base_version` 是提交方**取快照时**的版本。Core 用它判断
/// 「我 await 的这几十秒里，用户动过同一路径吗」。
#[derive(Debug, Clone)]
pub struct Patch {
    pub turn: TurnId,
    pub base_version: Version,
    pub origin: Origin,
    pub ops: Vec<Op>,
}

impl Patch {
    pub fn model(turn: TurnId, base_version: Version, ops: Vec<Op>) -> Self {
        Self { turn, base_version, origin: Origin::Model, ops }
    }

    pub fn user(turn: TurnId, base_version: Version, ops: Vec<Op>) -> Self {
        Self { turn, base_version, origin: Origin::User, ops }
    }
}

/// 本轮模型改动，未提交。按 path 索引，天然 last-writer-wins。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pending {
    pub turn: Option<TurnId>,
    pub ops: BTreeMap<Path, PendingOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PendingOp {
    Set { value: Value, source: Option<Source>, confidence: Option<f32> },
    Remove,
}

impl Pending {
    pub fn put(&mut self, turn: TurnId, op: &Op) {
        self.turn = Some(turn);
        match op {
            Op::Set { path, value, source, confidence } => {
                self.ops.insert(
                    path.clone(),
                    PendingOp::Set {
                        value: value.clone(),
                        source: source.clone(),
                        confidence: *confidence,
                    },
                );
            }
            Op::Remove { path } => {
                self.ops.insert(path.clone(), PendingOp::Remove);
            }
        }
    }

    /// 用户改了同一路径 ⇒ 模型对该路径的未提交改动作废。
    ///
    /// lost-update 防护的**前半段**：用户的写入必然比一个尚未提交的提议更新，
    /// 所以直接丢弃，而不是让 commit 时去覆盖用户刚填的值。
    /// 后半段在 `core::Core::apply` 的 Stale 判断里（防止 propose 晚于 UserEdit 到达）。
    pub fn remove_path(&mut self, path: &Path) -> bool {
        self.ops.remove(path).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// 摊平成一串 `Op`，供写进历史日志用（崩溃后按它重放）。
    pub fn as_ops(&self) -> Vec<Op> {
        self.ops
            .iter()
            .map(|(path, op)| match op {
                PendingOp::Set { value, source, confidence } => Op::Set {
                    path: path.clone(),
                    value: value.clone(),
                    source: source.clone(),
                    confidence: *confidence,
                },
                PendingOp::Remove => Op::Remove { path: path.clone() },
            })
            .collect()
    }

    pub fn clear(&mut self) {
        self.turn = None;
        self.ops.clear();
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// 来源与置信度的标注。两者都是可选的，没标就什么都不加 —— 不要给模型
/// 制造「这里本该有个我没填的字段」的错觉。
fn annotate(source: Option<&Source>, conf: Option<f32>) -> String {
    match (source, conf) {
        (None, None) => String::new(),
        (Some(s), None) => format!("  ⟨{}⟩", s.label()),
        (None, Some(c)) => format!("  ⟨conf {c:.1}⟩"),
        (Some(s), Some(c)) => format!("  ⟨{} · conf {c:.1}⟩", s.label()),
    }
}

/// 发给 turn task 的**不可变**快照：已提交状态 ⊕ 未提交改动。
///
/// 两半刻意分开存而不是合并成一份：UI 侧栏要把「模型本轮新推断出来的东西」高亮出来，
/// 合并了就分不出。要读「生效值」用 [`StateView::effective`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateView {
    pub ws: Workspace,
    pub pending: Pending,
    pub version: Version,
    /// **完整**会话历史（上下文分层的第五段）。
    ///
    /// 不是最近 K 轮 —— 用户会引用三十轮之前定下的东西。接近模型上限时压缩，
    /// 见 [`crate::context::plan_compaction`]。
    ///
    /// 放在快照里而不是让 turn 自己攒：整轮只取一次快照，所以这份历史的边界
    /// 和推断层的边界是同一时刻，不会出现「状态是新的、历史是旧的」。
    pub history: Vec<crate::model::Message>,
}

impl StateView {
    /// 生效值 = pending 优先，其次已提交。
    pub fn effective(&self, path: &Path) -> Option<&Value> {
        match self.pending.ops.get(path) {
            Some(PendingOp::Set { value, .. }) => Some(value),
            Some(PendingOp::Remove) => None,
            None => self.ws.fields.get(path).map(|f| &f.value),
        }
    }

    /// 组装进 prompt 的推断清单。
    ///
    /// **删掉 `locked` 之后，「模型改用户填过的字段之前要先问」的落点之一。**
    /// `[用户设定]` 标记 + system 段里的一句约束，替代了原来那个会拒绝写入的硬锁。
    /// 同时把来源与置信度一并给模型 —— 它自己就能看出哪些是它上一轮瞎猜的。
    pub fn prompt_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (path, f) in &self.ws.fields {
            let mark = match f.origin {
                Origin::User => "[用户设定]",
                Origin::Model => "[模型推断]",
            };
            let pend = if self.pending.ops.contains_key(path) { " (本轮有未提交改动)" } else { "" };
            out.push(format!(
                "{path} {mark}{pend} = {}{}",
                f.value,
                annotate(f.source.as_ref(), f.confidence)
            ));
        }
        for (path, op) in &self.pending.ops {
            if !self.ws.fields.contains_key(path) {
                match op {
                    PendingOp::Set { value, source, confidence } => out.push(format!(
                        "{path} [模型推断·未提交] = {value}{}",
                        annotate(source.as_ref(), *confidence)
                    )),
                    PendingOp::Remove => out.push(format!("{path} [模型推断·未提交] = <删除>")),
                }
            }
        }
        out
    }
}
