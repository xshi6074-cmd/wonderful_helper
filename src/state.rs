//! 推断图与改动。
//!
//! # Workspace 是时间线的物化结果，不是第二份真相
//!
//! 它由 [`crate::event::Body::Edited`] 与 [`crate::event::Body::Inferred`] 依次应用而来
//! （[`Workspace::apply`]）。丢掉它、从头重放，得到的必须是同一份 —— 那是不变量 I1。
//! 所以这里**没有** version、没有 pending、没有 base_version：那些都是为了在两份真相
//! 之间维持一致而存在的机器，只有一份真相时它们没有意义。
//!
//! # 这里删掉过什么
//!
//! - **`locked: HashSet<Path>`**：持久化的硬锁，用户填错一次就永久挡住模型。
//!   约束已下沉到 prompt（`[用户设定]` 标记 + 一句话），见 [`Workspace::prompt_lines`]。
//! - **`Budget`（打扰预算）**：用限额换安静，等于在预算耗尽时用降质减少打扰。
//! - **`Pending` / `PendingOp`**：模型的改动曾经攒到轮末才提交。但「打断也提交」
//!   一确定，事务边界就名存实亡了，它剩下的唯一用途是让 UI 高亮「本轮新增」——
//!   而那件事用 `field.seq >= 本轮起点` 就能判断，不需要一份平行的数据结构。
//!   **模型的改动现在当场生效**，turn 只是回滚粒度。
//! - **`Field.version` / `Patch.base_version`**：冲突裁决降级成了 turn 内的
//!   一个 path 集合（见 `core::Core::arbitrate`），不再需要版本比较。
//!
//! # 剩下的 `Field.seq` 不是版本，是溯源
//!
//! 它记「这个字段最后一次被时间线上的哪条事件改的」。UI 点一个节点能跳到那条事件，
//! 用户能看见「这个结论是模型在第几轮、根据什么写下的」。这是推断图要给出的交代。

use crate::event::{Body, Event};
use crate::ids::Seq;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Value = serde_json::Value;

/// 字段路径，点分。例：`spec.claim`、`node.generator.output`、`edge.gen->replay`。
///
/// 冲突检查是**字段级**的：用户改了推断图的一个节点，不该让模型对另一个节点的
/// 更新整份失败。
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

/// 值的出处。**不是锁。**
///
/// 它不再是 `Op` 的字段：写它的**事件类型**就决定了出处（`Edited` ⇒ User，
/// `Inferred` ⇒ Model）。上一版把 origin 存在 op 里而恢复时没存，导致重放后
/// 用户填的字段全变成模型推断，`[用户设定]` 标记消失 —— 删锁之后唯一的保护失效。
/// 由事件类型决定就不可能丢。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    User,
    Model,
}

/// 这个推断是从哪来的。无实证来源的节点在图上渲染为虚线 —— 那正是该警惕的地方。
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

/// 推断图上的一个字段（节点/边属性也走这里）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub value: Value,
    /// 谁写的。由写它的事件类型决定，Core 从不据此拒绝任何东西。
    pub origin: Origin,
    /// 依据。**可选**：不填表示没标来源。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// 0.0–1.0。**可选**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// 最后一次写它的那条事件。溯源用，不是版本号。
    pub seq: Seq,
}

impl Field {
    /// 没有实证来源（没标、或标成模型推测）⇒ 图上画虚线。
    pub fn is_dashed(&self) -> bool {
        self.source.as_ref().map(|s| s.is_dashed()).unwrap_or(true)
    }
}

/// 阶段闸门。
///
/// **推进只能由用户确认**（[`crate::event::Body::PhaseSet`] 只从 UI 来）。
/// 模型可以在正文里建议收尾，但没有推进权 —— 也没有阻断权：它不能因为
/// 「我觉得还没准备好」拦住用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Phase {
    Designing,
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

impl Default for Phase {
    fn default() -> Self {
        Phase::Designing
    }
}

/// 推断层：**从线性对话里抽出来的「我们现在定下了什么」。**
///
/// # 它到底做什么（五件事，缺一件都不能删）
///
/// 1. **给模型的当前结论清单。** [`Workspace::prompt_lines`] 是每轮动态拼进 prompt 的
///    主要 metadata。没有它，模型只能从几十轮对话里自己重建「现在定了什么」——
///    而那正是它最容易搞错、且错了没人发现的事。
/// 2. **让折叠成立的前提。** 折叠早期对话时之所以敢丢掉叙述过程，是因为结构化结论
///    已经在这里了（见 [`crate::context::plan_compaction`] 的位置选择）。
///    没有这张图，压缩就是纯粹的信息丢失。
/// 3. **仲裁的对象。** `turn_edits` 比的是 [`Path`]，而 path 的语义由这个结构定义。
/// 4. **UI 侧栏的数据源。** 推断图要画出来、每个字段要能点开溯源（[`Field::seq`]
///    指向写它的那条事件）、要能就地编辑。
/// 5. **蒸馏的输入。** 一次会话的结论，写进持久层的原料。
///
/// # 它不是什么
///
/// **不是第二份真相。** 它是主时间线的纯函数投影：`fold(apply, Workspace::new(), events)`。
/// 丢掉它从头重放必须得到逐字段相同的一份（不变量 I1，每个场景收尾都验）。
/// 所以它没有 version、没有 pending、没有 base_version —— 那些机器是为了在
/// 两份真相之间维持一致而存在的，只有一份真相时它们没有意义。
///
/// **不存流水。** 这里没有 `scene` 也没有 `scene_overrides`：上一版把它们塞在这里，
/// 于是重启后 UI 会显示上次会话最后一轮的场景。场景判定与用户更换都是时间线上的事件。
///
/// # 已知的不足
///
/// `fields` 还是平铺的 `path → Field`，不是真正的图（节点 / 边 / 边上的关系）。
/// 设计文档要的是「推断架构图」，现在只做到了「带来源的键值表」。
/// 改成图结构会动 [`Op`] 的形状，越早改越便宜。
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
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一条事件物化进来。**恢复与运行时走的是同一个函数** ——
    /// 两条路径分开写是上一版 origin 丢失的直接原因。
    pub fn apply(&mut self, e: &Event) {
        match &e.body {
            Body::Edited { ops } => self.write_all(ops, Origin::User, e.seq),
            Body::Inferred { ops, .. } => self.write_all(ops, Origin::Model, e.seq),
            Body::PhaseSet { to } => self.phase = *to,
            _ => {}
        }
    }

    fn write_all(&mut self, ops: &[Op], origin: Origin, seq: Seq) {
        for op in ops {
            match op {
                Op::Set { path, value, source, confidence, open, parked } => {
                    if let Some(v) = open {
                        self.open = v.clone();
                        continue;
                    }
                    if let Some(v) = parked {
                        self.parked = v.clone();
                        continue;
                    }
                    self.fields.insert(
                        path.clone(),
                        Field {
                            value: value.clone(),
                            origin,
                            // 用户没自己标来源时记成「用户口述」；标了就尊重他标的
                            source: match (source.clone(), origin) {
                                (Some(s), _) => Some(s),
                                (None, Origin::User) => Some(Source::User),
                                (None, Origin::Model) => None,
                            },
                            confidence: *confidence,
                            seq,
                        },
                    );
                }
                Op::Remove { path } => {
                    self.fields.remove(path);
                }
            }
        }
    }

    /// 组装进 prompt 的推断清单。
    ///
    /// **删掉硬锁之后，「模型改用户填过的字段之前要先问」的落点。**
    /// `[用户设定]` 标记 + system 段里的一句约束，替代了那个会拒绝写入的锁。
    /// 同时把来源与置信度一并给模型 —— 它自己就能看出哪些是它上一轮瞎猜的。
    pub fn prompt_lines(&self, turn_start: Seq) -> Vec<String> {
        let mut out = Vec::new();
        for (path, f) in &self.fields {
            let mark = match f.origin {
                Origin::User => "[用户设定]",
                Origin::Model => "[模型推断]",
            };
            // 本轮刚写的标出来，模型才知道哪些是它这一轮自己加的
            let fresh = if f.seq > turn_start { " (本轮)" } else { "" };
            out.push(format!(
                "{path} {mark}{fresh} = {}{}",
                f.value,
                annotate(f.source.as_ref(), f.confidence)
            ));
        }
        out
    }
}

/// 来源与置信度的标注。两者都可选，没标就什么都不加 —— 不要给模型制造
/// 「这里本该有个我没填的字段」的错觉。
fn annotate(source: Option<&Source>, conf: Option<f32>) -> String {
    match (source, conf) {
        (None, None) => String::new(),
        (Some(s), None) => format!("  ⟨{}⟩", s.label()),
        (None, Some(c)) => format!("  ⟨conf {c:.1}⟩"),
        (Some(s), Some(c)) => format!("  ⟨{} · conf {c:.1}⟩", s.label()),
    }
}

/// 单个改动。
///
/// 刻意只有 Set / Remove：列表追加（`Append`）会让「同一路径的冲突」失去良定义。
/// 待落定与搁置区走 `Set` 的 `open` / `parked` 变体 —— 让它们享受与字段
/// **同一套**仲裁，而不是像上一版那样整份直接覆盖、静默吃掉用户加的条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Set {
        path: Path,
        #[serde(default)]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
        /// 整份替换待落定列表。与 `value` 互斥，`path` 固定为 `"open"`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<Vec<String>>,
        /// 整份替换搁置区。`path` 固定为 `"parked"`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parked: Option<Vec<String>>,
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
        Op::Set {
            path: path.into(),
            value: value.into(),
            source: None,
            confidence: None,
            open: None,
            parked: None,
        }
    }

    /// 有实证来源的写入。
    pub fn grounded(path: impl Into<Path>, value: impl Into<Value>, source: Source, c: f32) -> Op {
        Op::Set {
            path: path.into(),
            value: value.into(),
            source: Some(source),
            confidence: Some(c),
            open: None,
            parked: None,
        }
    }

    pub fn remove(path: impl Into<Path>) -> Op {
        Op::Remove { path: path.into() }
    }

    pub fn open(items: Vec<String>) -> Op {
        Op::Set {
            path: Path::new("open"),
            value: Value::Null,
            source: None,
            confidence: None,
            open: Some(items),
            parked: None,
        }
    }

    pub fn parked(items: Vec<String>) -> Op {
        Op::Set {
            path: Path::new("parked"),
            value: Value::Null,
            source: None,
            confidence: None,
            open: None,
            parked: Some(items),
        }
    }
}
