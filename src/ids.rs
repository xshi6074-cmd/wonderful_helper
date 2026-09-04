//! 标识符。
//!
//! 这些 newtype 存在的唯一理由是**防止串号**：全是 u64，混用不会报错但会导致
//! 「陈旧事件被当成当前事件处理」——那是这套设计里最常见的 bug。
//!
//! # 只剩一个序号了
//!
//! 上一版有 `Version`：既是 CAS 令牌、又是崩溃恢复游标、又是 UI 版本显示，
//! 三个用途耦合在一个计数上，而且有几处 bump 了却不落盘，盘上版本长期落后于内存。
//!
//! 现在只有 [`Seq`]：**主时间线上的第几条事件**。它不是「版本」，是「位置」。
//! 恢复从某个位置开始重放、UI 引用某个位置、回滚到某个位置，都是同一件事的不同说法。
//! 冲突裁决不再用它 —— 那个降级成了 turn 内的一个 path 集合，见 `core::Core::arbitrate`。

use serde::{Deserialize, Serialize};

/// 一个会话。**回滚就是从某一轮分叉出一个新会话**，所以它必须是一等公民。
///
/// # 分叉语义
///
/// 从 t7 回滚 ⇒ 新建一个 session，`parent` 指向当前会话、`forked_at` = t7 起点的前一位。
/// **原会话一条事件都不动**：那条路走过就走过了，它是「试过没成立的方法」的原始记录，
/// 而这恰恰是这个项目里最值钱的一类数据（见 `memory/project.md` 的「试过但没成立的」一栏）。
///
/// 读一个会话 = 沿 `parent` 链往上走，每段取 `seq <= 该段的 forked_at`，
/// 从根往叶拼起来。因为每段的 seq 区间首尾相接，拼出来天然按 seq 升序。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> SessionId {
        SessionId(uuid::Uuid::new_v4().simple().to_string()[..12].to_string())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        SessionId::new()
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_string())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 会话元信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// 从哪个会话分叉出来的。`None` = 根会话。
    pub parent: Option<SessionId>,
    /// 分叉点：父会话上 `seq <= forked_at` 的事件属于这条链。
    pub forked_at: Seq,
    /// 给用户看的名字，比如「t7 · 换成先做小规模复现」。
    pub title: String,
    pub created_ms: u64,
}

/// 图上一个节点的身份。**形如 `n41_0`：铸出它的那条事件的 seq + 批内序号。**
///
/// # 为什么由 Core 铸，而且形状里带 seq
///
/// 模型不许自己起 id —— 它会撞号，也会把删掉的东西复活。铸 id 和分配 [`Seq`]
/// 是同一类事：只有 Core 这个单线程信箱能做。形状里带 seq 白拿两件事：
///
/// 1. **id 自带溯源** —— 看见 `n41_0` 就知道它是第 41 号事件写下的，点它能跳过去。
/// 2. **删掉的 id 永不重铸** —— seq 单调，所以「模型换个 id 把用户删掉的步骤
///    原样加回来」这条路从形状上就断了。
///
/// # 别名
///
/// 模型要引用同一批 op 里刚建的节点时，用 `$name` 形式的别名（[`NodeId::is_alias`]）。
/// **别名只在一批内有效**，Core 在 `commit` 里把它替换成真 id，
/// 落进时间线的只有真 id —— 不变量 I11 验这一条。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);

impl NodeId {
    /// `$name` = 还没解析的批内别名。
    pub fn is_alias(&self) -> bool {
        self.0.starts_with('$')
    }
    /// 铸一个真 id。
    pub fn mint(seq: Seq, i: usize) -> NodeId {
        NodeId(format!("n{}_{}", seq.0, i))
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        NodeId(s.to_string())
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        NodeId(s)
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 图上一条边的身份。形如 `e41_1`，规则同 [`NodeId`]。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeId(pub String);

impl EdgeId {
    pub fn is_alias(&self) -> bool {
        self.0.starts_with('$')
    }
    pub fn mint(seq: Seq, i: usize) -> EdgeId {
        EdgeId(format!("e{}_{}", seq.0, i))
    }
}

impl From<&str> for EdgeId {
    fn from(s: &str) -> Self {
        EdgeId(s.to_string())
    }
}

impl From<String> for EdgeId {
    fn from(s: String) -> Self {
        EdgeId(s)
    }
}

impl std::fmt::Display for EdgeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 删除的目标：节点或边。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElemId {
    Node(NodeId),
    Edge(EdgeId),
}

/// 主时间线上的位置。**由 Core 单调分配，在一条会话链内单调。**
///
/// # 为什么是 Core 分配而不是数据库 AUTOINCREMENT
///
/// Core 是唯一写权、单线程 mailbox，事件的先后本来就由它决定。让数据库分配意味着
/// Core 在发出事件时还不知道自己发的是第几条 —— 那它就没法同步更新物化视图、
/// 没法同步推 UI，只能等一次磁盘往返。而 Core 不能 await 磁盘。
///
/// 代价是启动时要先把本链的事件读回来、从最后一条接着往下，仅此而已。
///
/// **在链内单调，跨链不比较。** 两个从同一点分叉出去的会话，各自的下一条都是
/// `forked_at + 1` —— 它们是同一段历史的两种续法，本来就不该有先后。
/// 所以事件的主键是 `(session, seq)`，不是 `seq`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    pub const ZERO: Seq = Seq(0);
    /// 返回**新**值。Core 里的用法是 `let s = self.next_seq();`。
    pub fn bump(&mut self) -> Seq {
        self.0 += 1;
        *self
    }
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 一轮对话。**turn 只有两个职责**：代际判断（丢弃陈旧消息）与回滚粒度。
///
/// 它不再是状态的事务边界 —— 模型的推断当场生效，不攒到轮末（见 `state` 头部）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub u64);

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "t{}", self.0)
    }
}

/// 单次工具/子任务调用。R6 把 token 分账到这一级。
///
/// 与模型给的 `call_id` 不是一回事：`call_id` 是模型的字符串、必须原样回填给 API；
/// `TaskId` 是本地序号，用来在账本和 UI 任务列表里指认同一次调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub u64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "k{}", self.0)
    }
}

/// 一次 `ask_user` 提问。
///
/// **就是那条提问事件自己的 seq**，不另开 id 空间：提问本来就是主时间线上的一条，
/// 用户的回答事件用 `corr` 指回来。多一套 id 就多一处要维护的对应关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuestionId(pub Seq);

impl std::fmt::Display for QuestionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Q{}", self.0.0)
    }
}

// 去重集合曾经是个定容 LRU（cap 512，启动时用最近 256 条预热）。删掉了，理由：
//
// 库上的 `UNIQUE(session, client_id)` 本来是当兜底用的，但它**兜不住** ——
// 唯一约束一旦被违反，`append` 整批返回 Err，writer 无限重试，
// 整条落盘流水线就此卡死。一个「用户重发了一条很旧的消息」把持久化打挂，
// 这个代价比多存几百个字符串大得多。
//
// 而一次会话里的用户输入条数天然有界（人手打字），所以现在直接用
// `HashSet<String>`，启动时用**整条会话链**上的全部 client_id 预热。
// 去重在 Core 里就判完了，库上的约束从「兜底」降级成「永远不该触发的断言」。
