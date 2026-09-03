//! 标识符与一个极小的 LRU 集合。
//!
//! 这些 newtype 存在的唯一理由是**防止串号**：`TurnId` 和 `TaskId` 都是 u64，
//! 混用不会报错但会导致「陈旧事件被当成当前事件处理」——那是这套设计里最常见的 bug。

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Core 状态的单调版本号。**只有 Core 能递增它**。
///
/// 用途有二：① 模型提交 patch 时带上取快照时的 `base_version`，
/// Core 据此判断这期间用户有没有动过同一路径（见 `core::Core::apply`）；
/// ② 崩溃恢复时用来决定 history 从哪条开始重放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version(pub u64);

impl Version {
    pub const ZERO: Version = Version(0);
    pub fn bump(&mut self) -> Version {
        self.0 += 1;
        *self
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// 一轮对话的代际标识。
///
/// **每一个从 turn task 发回 Core 的消息都必须带上它**；对不上就丢弃。
/// 被取消的 subagent 仍然会跑完并把事件发回来，这是唯一的防线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnId(pub u64);

impl TurnId {
    /// 用户编辑不属于任何一轮。用这个哨兵值而不是 `Option<TurnId>`，
    /// 是为了让 `Patch` 保持一个平坦的结构；Core 在 User 分支里根本不看 turn。
    pub const NONE: TurnId = TurnId(u64::MAX);
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "t{}", self.0)
    }
}

/// 单个工具/子任务的标识，用于 R6 分账到具体调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u64);

/// 定容去重集合：`insert` 返回 true 表示这是新元素。
///
/// 用于 `client_id` 去重——用户手抖点两次发送，UI 会发两条 `UserInput`，
/// 两条的 `client_id` 相同。容量满时按插入顺序淘汰最旧的。
#[derive(Debug)]
pub struct LruSet<T: Hash + Eq + Clone> {
    set: HashSet<T>,
    order: VecDeque<T>,
    cap: usize,
}

impl<T: Hash + Eq + Clone> LruSet<T> {
    pub fn new(cap: usize) -> Self {
        Self { set: HashSet::new(), order: VecDeque::new(), cap: cap.max(1) }
    }

    /// 返回 true 表示这是**新**元素（即：应当处理）。false 表示重复，调用方应丢弃。
    pub fn insert(&mut self, v: T) -> bool {
        if self.set.contains(&v) {
            return false;
        }
        if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        self.order.push_back(v.clone());
        self.set.insert(v);
        true
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}
