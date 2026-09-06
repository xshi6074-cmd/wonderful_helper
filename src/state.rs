//! 推断层：**实验流程图**（有拓扑的）+ **图外推断**（没拓扑的）。
//!
//! # Workspace 是时间线的物化结果，不是第二份真相
//!
//! 它由 [`crate::event::Body::Edited`] 与 [`crate::event::Body::Inferred`] 依次应用而来
//! （[`Workspace::apply`]）。丢掉它、从头重放，得到的必须是同一份 —— 那是不变量 I1。
//! 所以这里**没有** version、没有 pending、没有 base_version：那些都是为了在两份真相
//! 之间维持一致而存在的机器，只有一份真相时它们没有意义。
//!
//! # 两种画法并存
//!
//! - [`FlowView::Built`]：**程序画**。结构化的 nodes/edges，由 [`crate::render`] 纯函数
//!   渲染成 mermaid。相对死板，但每个节点有稳定 id ⇒ 能点选、能就地编辑、能逐元素仲裁。
//! - [`FlowView::Sketch`]：**模型画**。模型直接写 mermaid / HTML 源码，我们原样渲染。
//!   自由，但**放弃结构化编辑** —— 用户只能整段改源码，仲裁退化到「整份 sketch」一个键。
//!
//! 两种都实现，跑起来再看哪种在真实对话里更有用。UI 侧两种都是「源码 + 预览」双栏。
//!
//! # 图只服务正向决策
//!
//! 放弃的路线、试过没成立的方法**不进图**。它们在时间线上（`Drop` 事件带 `why`），
//! 蒸馏时去捞。理由是用户的阅读与决策成本：图上每多一个「这条走不通」的灰节点，
//! 就多一份要略过的东西。将来若要给它一个位置，那是 explore mode 下的独立栏，不是这张图。
//!
//! # 这里删掉过什么
//!
//! - **`locked: HashSet<Path>`**：持久化的硬锁，用户填错一次就永久挡住模型。
//!   约束已下沉到 prompt（`[用户设定]` 标记 + 一句话），见 [`Workspace::prompt_lines`]。
//! - **`Budget`（打扰预算）**：用限额换安静，等于在预算耗尽时用降质减少打扰。
//! - **`Pending` / `PendingOp`**：模型的改动曾经攒到轮末才提交。但「打断也提交」
//!   一确定，事务边界就名存实亡了。**模型的改动现在当场生效**，turn 只是回滚粒度。
//! - **`Field.version` / `Patch.base_version`**：冲突裁决降级成了 turn 内的一个键集合。
//!
//! # 剩下的 `Prov.seq` 不是版本，是溯源
//!
//! 它记「这个东西最后一次被时间线上的哪条事件改的」。UI 点一个节点能跳到那条事件，
//! 用户能看见「这个结论是模型在第几轮、根据什么写下的」。这是推断图要给出的交代。

use crate::event::{Body, Event};
use crate::ids::{EdgeId, ElemId, NodeId, Seq};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Value = serde_json::Value;

/// 图外推断的路径，点分。例：`spec.claim`、`budget.gpu_hours`。
///
/// 也当**仲裁键**用：图内元素的键是 `flow/<id>` 这种合成路径，见 [`Op::keys`]。
/// 冲突检查是元素级的：用户改了图里一个节点，不该让模型对另一个节点的更新整份失败。
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
/// 它不是 `Op` 的字段：写它的**事件类型**就决定了出处（`Edited` ⇒ User，
/// `Inferred` ⇒ Model）。上一版把 origin 存在 op 里而恢复时没存，导致重放后
/// 用户填的字段全变成模型推断，`[用户设定]` 标记消失。由事件类型决定就不可能丢。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    User,
    Model,
}

/// 这个推断是从哪来的。无实证来源的元素在图上渲染为虚线 —— 那正是该警惕的地方。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// 仓库位置，如 `src/model/generator.py:42`
    #[serde(alias = "Repo")]
    Repo(String),
    /// 论文章节
    #[serde(alias = "Paper")]
    Paper(String),
    /// 用户口述
    #[serde(alias = "User")]
    User,
    /// 模型推测 —— 渲染成虚线的就是它
    #[serde(alias = "Guess")]
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

/// 溯源三件套 + 位置。**节点、边、图外推断共用同一份**，
/// 所以 UI 的「点开看出处」对图内图外是同一段代码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prov {
    /// 谁写的。由写它的事件类型决定，Core 从不据此拒绝任何东西。
    pub origin: Origin,
    /// 依据。**可选**：不填表示没标来源，与「模型推测」同等对待（都画虚线）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// 0.0–1.0。**可选**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// 最后一次写它的那条事件。溯源用，不是版本号。
    pub seq: Seq,
}

impl Prov {
    fn new(origin: Origin, source: Option<Source>, confidence: Option<f32>, seq: Seq) -> Prov {
        Prov {
            // 用户没自己标来源时记成「用户口述」；标了就尊重他标的
            source: match (source, origin) {
                (Some(s), _) => Some(s),
                (None, Origin::User) => Some(Source::User),
                (None, Origin::Model) => None,
            },
            origin,
            confidence,
            seq,
        }
    }

    /// 没有实证来源（没标、或标成模型推测）⇒ 图上画虚线。
    pub fn is_dashed(&self) -> bool {
        self.source.as_ref().map(|s| s.is_dashed()).unwrap_or(true)
    }

    /// prompt 里跟在元素后面的那一小段标注。
    pub fn annotate(&self) -> String {
        let mark = match self.origin {
            Origin::User => "用户设定",
            Origin::Model => "模型推断",
        };
        match (self.source.as_ref(), self.confidence) {
            (None, None) => format!("[{mark}]"),
            (Some(s), None) => format!("[{mark} · {}]", s.label()),
            (None, Some(c)) => format!("[{mark} · conf {c:.1}]"),
            (Some(s), Some(c)) => format!("[{mark} · {} · conf {c:.1}]", s.label()),
        }
    }
}

/// **图外推断**：没有拓扑的判断。领域、目标、资源约束、口径、风险、用户偏好。
///
/// 判据是一句话：*它和别的东西之间有没有一条边*。有就进图，没有就进这里。
/// 强行把所有判断塞进图，图会变成一张便签墙。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub value: Value,
    /// 挂到图上某个节点（「这一步的学习率依据不足」）。UI 在该节点旁显示角标，
    /// prompt 里跟在该节点的明细后面。**不必二选一** —— 图内图外可以互相指。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<NodeId>,
    pub prov: Prov,
}

/// 图上的一个节点。
///
/// # 为什么 `kind` 是 String 不是 enum
///
/// 主场景是 ML：最有信息量的是**架构图**（数据 → 模块 → 损失 → 指标 → 消融臂），
/// 而这类图的词表根本枚举不完。写成 enum 的话，模型一想画「先并行三个配比、
/// 再挑一个往下走」就撞墙。形状与配色的映射在 `memory/prompts.toml` 里，
/// **用户随时可改，未知 kind 落到默认形状** —— 这才是「不用 mermaid 模板」的落点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    /// 开放词表。ML 默认那套：`data` / `module` / `op` / `loss` / `metric` / `ablation`。
    pub kind: String,
    /// 图上显示的那行字。
    pub label: String,
    /// 细节：这一步具体怎么做。图上折起来，点开才看。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// 参数：维度 / 学习率 / 批大小 / 数据规模。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
    /// 分组、子图、并行臂、对照组 —— 全靠它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    pub prov: Prov,
}

/// 图上的一条边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    /// 开放词表：`flow`（张量/数据流）/ `feeds` / `supervises` / `compares` / `depends`。
    pub kind: String,
    /// 边上的条件或说明。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
    pub prov: Prov,
}

/// 源码的语言。**Sketch 两种都支持；Built 目前只出 mermaid**（见 [`crate::render`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    Mermaid,
    Html,
}

impl Lang {
    pub fn tag(&self) -> &'static str {
        match self {
            Lang::Mermaid => "mermaid",
            Lang::Html => "html",
        }
    }
}

/// **模型画的那份。**模型直接写源码，我们原样渲染。
///
/// 代价说死：这块**没有稳定 id**，所以点不了、就地编辑不了、逐元素仲裁不了。
/// 用户只能整段改源码，整份 sketch 在仲裁里只占一个键 `flow/sketch`。
/// 要自由就得付这个代价，不能只要自由不付代价。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sketch {
    pub lang: Lang,
    pub src: String,
    pub prov: Prov,
}

/// 当前以哪一份为准。用户可切，模型也可切（切换本身走仲裁）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowView {
    /// 程序画：结构化 + 纯函数渲染。默认。
    #[default]
    Built,
    /// 模型画：模型自己写源码。
    Sketch,
}

/// 实验流程图。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Graph {
    /// BTree 而非 Hash：**渲染必须逐字节确定**（不变量 I10）。
    #[serde(default)]
    pub nodes: BTreeMap<NodeId, Node>,
    #[serde(default)]
    pub edges: BTreeMap<EdgeId, Edge>,
    /// 图级渲染意向：`dialect` / `dir` / `classdef.<name>`。
    /// **渲染器不认识的键直接忽略，不报错** —— 这是留给模型的自由度，不是协议。
    #[serde(default)]
    pub render: BTreeMap<String, String>,
    /// 模型画的那份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sketch: Option<Sketch>,
    #[serde(default)]
    pub view: FlowView,
}

impl Graph {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.sketch.is_none()
    }

    /// 一个节点的子节点，按 id 排序（BTreeMap 保证）。
    pub fn children(&self, id: &NodeId) -> Vec<&Node> {
        self.nodes.values().filter(|n| n.parent.as_ref() == Some(id)).collect()
    }

    /// 边的有序端点对，当仲裁键用。**堵「换个 id 把同样的连接加回来」。**
    pub fn pair_key(from: &NodeId, to: &NodeId) -> Path {
        Path(format!("flow/{}->{}", from.0, to.0))
    }

    /// 节点标签的归一化键。**堵「换个 id 新建同名节点复活删掉的步骤」。**
    ///
    /// 启发式：模型换个措辞就绕过去了。但代价方向是安全的 —— 宁可多丢一条 op
    /// （模型下一轮从 `dropped` 里看得到、可以重提），也不要静默盖掉用户的删除。
    pub fn label_key(label: &str) -> Path {
        let norm: String =
            label.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect();
        Path(format!("flow/label:{norm}"))
    }

    /// 收尾：去悬边、断 parent 环、砍过深嵌套。**不变量 I9 的落点。**
    ///
    /// 放在 `apply` 末尾而不是各个分支里：删节点、改 parent、加边都可能破坏完整性，
    /// 每处各写一遍迟早漏。这里是纯函数式的收敛，重放也走同一段。
    fn sanitize(&mut self) {
        // 悬边：端点没了的边直接不存在。删节点时的连带删除也靠这一步兜底。
        let ids: Vec<EdgeId> = self
            .edges
            .iter()
            .filter(|(_, e)| !self.nodes.contains_key(&e.from) || !self.nodes.contains_key(&e.to))
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            self.edges.remove(&id);
        }
        // parent 指向不存在的节点 ⇒ 提升到顶层。
        let orphans: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.parent.as_ref().is_some_and(|p| !self.nodes.contains_key(p)))
            .map(|(k, _)| k.clone())
            .collect();
        for id in orphans {
            if let Some(n) = self.nodes.get_mut(&id) {
                n.parent = None;
            }
        }
        // parent 成环或过深 ⇒ 把闯祸的那个提到顶层。
        let bad: Vec<NodeId> =
            self.nodes.keys().filter(|id| self.depth_of(id).is_none()).cloned().collect();
        for id in bad {
            if let Some(n) = self.nodes.get_mut(&id) {
                n.parent = None;
            }
        }
    }

    /// 嵌套深度；成环或超过 [`MAX_DEPTH`] 返回 `None`。
    pub fn depth_of(&self, id: &NodeId) -> Option<usize> {
        let mut cur = id.clone();
        for d in 0..MAX_DEPTH {
            match self.nodes.get(&cur).and_then(|n| n.parent.clone()) {
                None => return Some(d),
                Some(p) if p == *id => return None, // 环
                Some(p) => cur = p,
            }
        }
        None
    }
}

/// 嵌套上限。超过就是模型在造迷宫，不是在画图。
pub const MAX_DEPTH: usize = 8;

/// 阶段闸门。
///
/// **推进只能由用户确认**（[`crate::event::Body::PhaseSet`] 只从 UI 来）。
/// 模型可以在正文里建议收尾，但没有推进权 —— 也没有阻断权。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Phase {
    #[default]
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

/// 推断层：**从线性对话里抽出来的「我们现在定下了什么」。**
///
/// # 它到底做什么（五件事，缺一件都不能删）
///
/// 1. **给模型的当前结论清单。** 每轮动态拼进 prompt 的主要 metadata。
///    没有它，模型只能从几十轮对话里自己重建「现在定了什么」——
///    而那正是它最容易搞错、且错了没人发现的事。
/// 2. **让折叠成立的前提。** 折叠早期对话时之所以敢丢掉叙述过程，是因为结构化结论
///    已经在这里了。没有这张图，压缩就是纯粹的信息丢失。
/// 3. **仲裁的对象。** `turn_edits` 比的是 [`Path`]，而键的语义由 [`Op::keys`] 定义。
/// 4. **UI 侧栏的数据源。** 图要画出来、每个元素要能点开溯源、要能就地编辑。
/// 5. **蒸馏的输入。** 一次会话的结论，写进持久层的原料。
///
/// # 它不是什么
///
/// **不是第二份真相。** 它是主时间线的纯函数投影：`fold(apply, Workspace::new(), events)`。
/// 丢掉它从头重放必须得到相同的一份（不变量 I1，每个场景收尾都验）。
///
/// **不存流水。** 这里没有 `scene` 也没有 `scene_overrides`：场景判定与用户更换
/// 都是时间线上的事件，塞在这里会让重启后 UI 显示上次会话最后一轮的场景。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub phase: Phase,
    /// **图内**：实验流程 / 模型架构。有拓扑的东西。
    #[serde(default)]
    pub flow: Graph,
    /// **图外**：没有拓扑的判断。
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
                Op::Set { path, value, anchor, source, confidence, open, parked } => {
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
                            anchor: anchor.clone(),
                            prov: Prov::new(origin, source.clone(), *confidence, seq),
                        },
                    );
                }
                Op::Remove { path } => {
                    self.fields.remove(path);
                }

                Op::Node { id, kind, label, body, parent, attrs, source, confidence } => {
                    // 别名没被解析就落到这里 ⇒ 丢弃。不变量 I11 在测里验它。
                    if id.is_alias() {
                        continue;
                    }
                    let prov = Prov::new(origin, source.clone(), *confidence, seq);
                    let n = self.flow.nodes.entry(id.clone()).or_insert_with(|| Node {
                        id: id.clone(),
                        kind: DEFAULT_NODE_KIND.into(),
                        label: String::new(),
                        body: String::new(),
                        attrs: BTreeMap::new(),
                        parent: None,
                        prov: prov.clone(),
                    });
                    // patch：None = 这一项不改。这是「只改 label 不动 body」的前提。
                    if let Some(v) = kind {
                        n.kind = v.clone();
                    }
                    if let Some(v) = label {
                        n.label = v.clone();
                    }
                    if let Some(v) = body {
                        n.body = v.clone();
                    }
                    if let Some(v) = parent {
                        n.parent = v.clone();
                    }
                    for (k, v) in attrs {
                        match v {
                            Some(v) => {
                                n.attrs.insert(k.clone(), v.clone());
                            }
                            None => {
                                n.attrs.remove(k);
                            }
                        }
                    }
                    n.prov = prov;
                }

                Op::Edge { id, from, to, kind, label, attrs, source, confidence } => {
                    if id.is_alias() {
                        continue;
                    }
                    let prov = Prov::new(origin, source.clone(), *confidence, seq);
                    match self.flow.edges.get_mut(id) {
                        Some(e) => {
                            if let Some(v) = from {
                                e.from = v.clone();
                            }
                            if let Some(v) = to {
                                e.to = v.clone();
                            }
                            if let Some(v) = kind {
                                e.kind = v.clone();
                            }
                            if let Some(v) = label {
                                e.label = v.clone();
                            }
                            for (k, v) in attrs {
                                match v {
                                    Some(v) => {
                                        e.attrs.insert(k.clone(), v.clone());
                                    }
                                    None => {
                                        e.attrs.remove(k);
                                    }
                                }
                            }
                            e.prov = prov;
                        }
                        // 建边必须给两个端点。缺一个就不是一条边，静默跳过。
                        None => {
                            let (Some(f), Some(t)) = (from, to) else { continue };
                            self.flow.edges.insert(
                                id.clone(),
                                Edge {
                                    id: id.clone(),
                                    from: f.clone(),
                                    to: t.clone(),
                                    kind: kind.clone().unwrap_or_else(|| DEFAULT_EDGE_KIND.into()),
                                    label: label.clone().unwrap_or_default(),
                                    attrs: attrs
                                        .iter()
                                        .filter_map(|(k, v)| {
                                            v.clone().map(|v| (k.clone(), v))
                                        })
                                        .collect(),
                                    prov,
                                },
                            );
                        }
                    }
                }

                Op::Drop { id, .. } => match id {
                    ElemId::Node(n) => {
                        self.flow.nodes.remove(n);
                        // 子节点**提升到被删节点的 parent，不级联删除**：
                        // 删一个「对照组」不该把组里三个步骤一起带走 —— 那是用户
                        // 最不想要的意外。以它为端点的边由 sanitize 收掉。
                        let up = None;
                        for child in self.flow.nodes.values_mut() {
                            if child.parent.as_ref() == Some(n) {
                                child.parent = up.clone();
                            }
                        }
                        // 挂在这个节点上的图外推断解除锚定，但**不删** —— 那是独立的一条判断。
                        for f in self.flow_anchored_mut(n) {
                            *f = None;
                        }
                    }
                    ElemId::Edge(e) => {
                        self.flow.edges.remove(e);
                    }
                },

                Op::Render { key, value } => match value {
                    Some(v) => {
                        self.flow.render.insert(key.clone(), v.clone());
                    }
                    None => {
                        self.flow.render.remove(key);
                    }
                },

                Op::Sketch { lang, src } => {
                    self.flow.sketch = Some(Sketch {
                        lang: *lang,
                        src: src.clone(),
                        prov: Prov::new(origin, source_of_sketch(origin), None, seq),
                    });
                }

                Op::View { to } => self.flow.view = *to,
            }
        }
        self.flow.sanitize();
    }

    /// 解除锚定用的可变借用。单独一个函数只是为了避开借用检查。
    fn flow_anchored_mut(&mut self, n: &NodeId) -> Vec<&mut Option<NodeId>> {
        self.fields
            .values_mut()
            .filter(|f| f.anchor.as_ref() == Some(n))
            .map(|f| &mut f.anchor)
            .collect()
    }

    /// 组装进 prompt 的**图外**推断清单。图内的部分见 [`crate::render::flow_block`]。
    ///
    /// **删掉硬锁之后，「模型改用户填过的字段之前要先问」的落点。**
    /// `[用户设定]` 标记 + system 段里的一句约束，替代了那个会拒绝写入的锁。
    /// 同时把来源与置信度一并给模型 —— 它自己就能看出哪些是它上一轮瞎猜的。
    pub fn prompt_lines(&self, turn_start: Seq) -> Vec<String> {
        let mut out = Vec::new();
        for (path, f) in &self.fields {
            // 挂在节点上的跟着节点走，不在这里重复一遍
            if f.anchor.is_some() {
                continue;
            }
            let fresh = if f.prov.seq > turn_start { " (本轮)" } else { "" };
            out.push(format!("{path} {}{fresh} = {}", f.prov.annotate(), f.value));
        }
        out
    }
}

/// 模型没给 sketch 标来源时的默认。用户手写的算「用户口述」，模型画的算「推测」。
fn source_of_sketch(origin: Origin) -> Option<Source> {
    match origin {
        Origin::User => Some(Source::User),
        Origin::Model => Some(Source::Guess),
    }
}

/// 建节点时模型没给 kind 的兜底。主场景是 ML 架构图，所以是 `module` 不是 `step`。
pub const DEFAULT_NODE_KIND: &str = "module";
/// 建边时的兜底：张量 / 数据流。
pub const DEFAULT_EDGE_KIND: &str = "flow";

/// `Option<Option<T>>` 的反序列化：区分「字段没给」和「字段给了 null」。
fn double_option<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

/// 单个改动。
///
/// 图内的 op **全部是 patch**（`None` = 这一项不改）：这是「只改 label 不动 body」
/// 能成立的前提，也是把重写压到最小的地方 —— 模型每轮只发它真想改的那几项。
///
/// # 为什么是内部 tag
///
/// 默认的外部 tag 会序列化成 `{"Node": {...}}`，模型写起来别扭、事件 JSON 读起来
/// 也别扭。内部 tag 是 `{"op": "node", "id": ..., "label": ...}` —— 平的，
/// **模型侧的 schema 和落盘格式是同一个**，不需要在中间再翻译一层。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    // ───────────── 图外 ─────────────
    Set {
        path: Path,
        #[serde(default)]
        value: Value,
        /// 挂到图上某个节点。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<NodeId>,
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

    // ───────────── 图内 ─────────────
    Node {
        id: NodeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// `Some(None)` = 从分组里挪出来；`None` = 不动。
        ///
        /// 要 `double_option`：serde 默认会把 JSON 的 `null` 收成外层的 `None`，
        /// 也就是「不动」—— 于是「提到顶层」这个意思**从 JSON 根本表达不出来**。
        /// 模型侧的 schema 里写了 `parent: null` 能提到顶层，这里就得真的做到。
        #[serde(
            default,
            deserialize_with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        parent: Option<Option<NodeId>>,
        /// 值为 `None` = 删这个 attr。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attrs: Vec<(String, Option<String>)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
    },
    Edge {
        id: EdgeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<NodeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<NodeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attrs: Vec<(String, Option<String>)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
    },
    Drop {
        id: ElemId,
        /// 「为什么去掉？」—— UI 上是删除后原地的一个**可跳过**单行输入，不弹窗、
        /// 不阻断。用户愿意写就白拿一行最值钱的数据；不写也不追问。
        /// 图上不留痕（图只服务正向决策），这行字留在时间线上给蒸馏。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    Render {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Sketch {
        lang: Lang,
        src: String,
    },
    View {
        to: FlowView,
    },
}

impl Op {
    /// **仲裁键。**一条 op 可能占多个键 —— 那正是堵漏洞的地方。
    ///
    /// 用户的 op 把这些键**占下**（写进 `turn_edits`），模型的 op **查**这些键，
    /// 撞上任意一个就整条丢弃。两边用同一个函数，所以不存在「占的键和查的键不一样」。
    ///
    /// 需要 `&Graph` 是因为删除类 op 的键要从**当前图**里查（端点、标签）。
    /// 调用点必须在 `apply` **之前** —— 图变了就查不到了。
    pub fn keys(&self, g: &Graph) -> Vec<Path> {
        match self {
            Op::Set { path, .. } | Op::Remove { path } => vec![path.clone()],

            Op::Node { id, label, .. } => {
                let mut ks = vec![Path(format!("flow/{}", id.0))];
                // 标签键。用户删掉一个节点时占的就是它 —— 模型换个 id 新建一个
                // 同名节点想把它复活，撞的是这一条（id 键根本撞不上，新 id 从没被占过）。
                if let Some(l) = label {
                    ks.push(Graph::label_key(l));
                }
                // 改名前的旧标签也占：否则「用户删了 A，模型把 B 改名成 A」照样绕过去。
                if let Some(n) = g.nodes.get(id) {
                    ks.push(Graph::label_key(&n.label));
                }
                ks
            }

            Op::Edge { id, from, to, .. } => {
                let mut ks = vec![Path(format!("flow/{}", id.0))];
                // 端点：op 带了就用 op 的，没带就从图里查现存的那条边
                let cur = g.edges.get(id);
                let f = from.clone().or_else(|| cur.map(|e| e.from.clone()));
                let t = to.clone().or_else(|| cur.map(|e| e.to.clone()));
                if let (Some(f), Some(t)) = (f, t) {
                    ks.push(Graph::pair_key(&f, &t));
                }
                // 改接：旧端点对也要占，否则模型能把它接回原处
                if let Some(e) = cur {
                    ks.push(Graph::pair_key(&e.from, &e.to));
                }
                ks
            }

            Op::Drop { id, .. } => match id {
                ElemId::Node(n) => {
                    let mut ks = vec![Path(format!("flow/{}", n.0))];
                    if let Some(node) = g.nodes.get(n) {
                        ks.push(Graph::label_key(&node.label));
                    }
                    ks
                }
                ElemId::Edge(e) => {
                    let mut ks = vec![Path(format!("flow/{}", e.0))];
                    if let Some(edge) = g.edges.get(e) {
                        ks.push(Graph::pair_key(&edge.from, &edge.to));
                    }
                    ks
                }
            },

            Op::Render { key, .. } => vec![Path(format!("flow/render/{key}"))],
            // 整份 sketch 一个键 —— 模型画的那份没有内部 id 可以分。
            Op::Sketch { .. } => vec![Path::new("flow/sketch")],
            Op::View { .. } => vec![Path::new("flow/view")],
        }
    }

    /// 报进 `dropped` 时用的那一个键。取第一个 —— 它总是元素自己的 id。
    pub fn key(&self, g: &Graph) -> Path {
        self.keys(g).into_iter().next().unwrap_or_else(|| Path::new("?"))
    }

    /// 这条 op 引用到的所有节点 id（含别名）。别名解析要用。
    pub fn node_refs_mut(&mut self) -> Vec<&mut NodeId> {
        match self {
            // anchor 不走这里：它解析失败只清自己、不丢整条 op，见 `resolve`。
            Op::Set { .. } => vec![],
            Op::Node { id, parent, .. } => {
                let mut v = vec![id];
                if let Some(Some(p)) = parent {
                    v.push(p);
                }
                v
            }
            Op::Edge { from, to, .. } => from.iter_mut().chain(to.iter_mut()).collect(),
            Op::Drop { id: ElemId::Node(n), .. } => vec![n],
            _ => vec![],
        }
    }

    /// 同上，边 id。
    pub fn edge_refs_mut(&mut self) -> Vec<&mut EdgeId> {
        match self {
            Op::Edge { id, .. } => vec![id],
            Op::Drop { id: ElemId::Edge(e), .. } => vec![e],
            _ => vec![],
        }
    }

    // ───────────── 构造助手 ─────────────

    /// 不标来源的图外写入。
    pub fn set(path: impl Into<Path>, value: impl Into<Value>) -> Op {
        Op::Set {
            path: path.into(),
            value: value.into(),
            anchor: None,
            source: None,
            confidence: None,
            open: None,
            parked: None,
        }
    }

    /// 有实证来源的图外写入。
    pub fn grounded(path: impl Into<Path>, value: impl Into<Value>, source: Source, c: f32) -> Op {
        Op::Set {
            path: path.into(),
            value: value.into(),
            anchor: None,
            source: Some(source),
            confidence: Some(c),
            open: None,
            parked: None,
        }
    }

    /// 挂在某个节点上的图外推断。
    pub fn anchored(path: impl Into<Path>, value: impl Into<Value>, on: impl Into<NodeId>) -> Op {
        Op::Set {
            path: path.into(),
            value: value.into(),
            anchor: Some(on.into()),
            source: None,
            confidence: None,
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
            anchor: None,
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
            anchor: None,
            source: None,
            confidence: None,
            open: None,
            parked: Some(items),
        }
    }

    /// 建 / 改一个节点。`id` 可以是别名（`$name`），Core 提交时铸成真 id。
    pub fn node(id: impl Into<NodeId>, kind: &str, label: &str) -> Op {
        Op::Node {
            id: id.into(),
            kind: Some(kind.into()),
            label: Some(label.into()),
            body: None,
            parent: None,
            attrs: vec![],
            source: None,
            confidence: None,
        }
    }

    /// 只改标题。
    pub fn relabel(id: impl Into<NodeId>, label: &str) -> Op {
        Op::Node {
            id: id.into(),
            kind: None,
            label: Some(label.into()),
            body: None,
            parent: None,
            attrs: vec![],
            source: None,
            confidence: None,
        }
    }

    /// 建 / 改一条边。
    pub fn edge(id: impl Into<EdgeId>, from: impl Into<NodeId>, to: impl Into<NodeId>) -> Op {
        Op::Edge {
            id: id.into(),
            from: Some(from.into()),
            to: Some(to.into()),
            kind: None,
            label: None,
            attrs: vec![],
            source: None,
            confidence: None,
        }
    }

    pub fn drop_node(id: impl Into<NodeId>) -> Op {
        Op::Drop { id: ElemId::Node(id.into()), why: None }
    }

    pub fn drop_edge(id: impl Into<EdgeId>) -> Op {
        Op::Drop { id: ElemId::Edge(id.into()), why: None }
    }

    pub fn sketch(lang: Lang, src: impl Into<String>) -> Op {
        Op::Sketch { lang, src: src.into() }
    }

    pub fn view(to: FlowView) -> Op {
        Op::View { to }
    }
}

/// 别名解析 + 幽灵 id 拦截。**Core 在 `commit` 之前调它，落进时间线的只有真 id。**
///
/// `seq` 是这批 op 将要落到的那条事件的位置。id 从它铸出来（`n<seq>_<i>`），
/// 所以 id 自带溯源，而且因为 seq 单调，**删掉的 id 永不重铸**。
///
/// 返回 `(留下的 op, 被丢掉的 op 的键)`。丢弃的四种情况：
///
/// 1. **引用了批里没定义过的别名** —— 模型自己写岔了。
/// 2. **`Op::Node` / `Op::Edge` 用了一个图里不存在的真 id** —— 新元素必须走别名。
///    不拦的话，模型记错一个 id 就会凭空造出一个没人要的孤儿节点，而且它会长期
///    留在图上没人发现。
/// 3. **边的端点不存在**（既不在图里，也不是这批新建的）。
/// 4. **`Drop` 的目标不存在** —— 本来就是空操作，但报出来模型才知道自己记错了。
///
/// `anchor` 指向不存在的节点是个例外：**只清掉 anchor，不丢整条 op**。
/// 那条图外推断本身是有效的判断，不该因为挂错了地方就整条作废。
pub fn resolve(ops: Vec<Op>, g: &Graph, seq: Seq) -> (Vec<Op>, Vec<Path>) {
    use std::collections::HashMap;

    // 第一遍：按出现顺序铸 id。节点与边共用一个序号，所以 id 在批内唯一。
    let mut nmap: HashMap<String, NodeId> = HashMap::new();
    let mut emap: HashMap<String, EdgeId> = HashMap::new();
    let mut i = 0usize;
    for op in &ops {
        match op {
            Op::Node { id, .. } if id.is_alias() && !nmap.contains_key(&id.0) => {
                nmap.insert(id.0.clone(), NodeId::mint(seq, i));
                i += 1;
            }
            Op::Edge { id, .. } if id.is_alias() && !emap.contains_key(&id.0) => {
                emap.insert(id.0.clone(), EdgeId::mint(seq, i));
                i += 1;
            }
            _ => {}
        }
    }
    let minted: std::collections::HashSet<NodeId> = nmap.values().cloned().collect();

    // 第二遍：改写引用，顺手做存在性检查。
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for mut op in ops {
        let key = op.key(g);
        let mut bad = false;
        // anchor 是唯一的例外：解析不了、或指不到真节点，**只清掉 anchor**。
        // 那条图外推断本身是个有效判断，不该因为挂错了地方就整条作废。
        if let Op::Set { anchor: anchor @ Some(_), .. } = &mut op {
            let a = anchor.clone().unwrap();
            let real = if a.is_alias() { nmap.get(&a.0).cloned() } else { Some(a) };
            *anchor = real.filter(|n| g.nodes.contains_key(n) || minted.contains(n));
        }

        for r in op.node_refs_mut() {
            if r.is_alias() {
                match nmap.get(&r.0) {
                    Some(real) => *r = real.clone(),
                    None => bad = true,
                }
            }
        }
        for r in op.edge_refs_mut() {
            if r.is_alias() {
                match emap.get(&r.0) {
                    Some(real) => *r = real.clone(),
                    None => bad = true,
                }
            }
        }
        if !bad {
            bad = !exists(&op, g, &minted, &emap);
        }
        if bad {
            dropped.push(key);
        } else {
            kept.push(op);
        }
    }
    (kept, dropped)
}

/// 存在性检查：真 id 必须指向真东西。见 [`resolve`] 的第 2–4 条。
fn exists(
    op: &Op,
    g: &Graph,
    minted: &std::collections::HashSet<NodeId>,
    emap: &std::collections::HashMap<String, EdgeId>,
) -> bool {
    let known_node = |n: &NodeId| g.nodes.contains_key(n) || minted.contains(n);
    match op {
        Op::Node { id, parent, .. } => {
            if !known_node(id) {
                return false;
            }
            // parent 指向不存在的节点不算致命：sanitize 会把它提到顶层。
            // 为此丢掉整条节点更新，比留一个层级不对的节点更糟。
            let _ = parent;
            true
        }
        Op::Edge { id, from, to, .. } => {
            let fresh = emap.values().any(|e| e == id);
            if !fresh && !g.edges.contains_key(id) {
                return false;
            }
            // 端点：新建时必须两个都给；改边时缺的那个沿用原值
            let f = from.clone().or_else(|| g.edges.get(id).map(|e| e.from.clone()));
            let t = to.clone().or_else(|| g.edges.get(id).map(|e| e.to.clone()));
            match (f, t) {
                (Some(f), Some(t)) => known_node(&f) && known_node(&t),
                _ => false,
            }
        }
        Op::Drop { id, .. } => match id {
            ElemId::Node(n) => g.nodes.contains_key(n),
            ElemId::Edge(e) => g.edges.contains_key(e),
        },
        _ => true,
    }
}
