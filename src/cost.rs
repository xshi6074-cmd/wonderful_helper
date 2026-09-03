//! R6：token 计费账本。
//!
//! 分角色记账不是为了好看，是为了能讲出设计文档里那句话：
//! **「判断阶段只占总 token 的 X%」**——用来证明这套强制检查机制不贵。
//! 只报一个总数就讲不了这句话。
//!
//! `estimated_*` 单独计数是因为被打断的轮次拿不到服务端 usage，只能估算。
//! 把估算量和实测量混在一起会让账目看起来比实际精确，评审问一句就露馅。

use crate::model::{Role, Usage};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct CostLedger {
    by_role: BTreeMap<Role, Usage>,
    /// 有多少次记账是估算的（打断兜底）。
    pub estimated_entries: u32,
    pub entries: u32,
}

impl CostLedger {
    pub fn add(&mut self, role: Role, u: Usage) {
        let slot = self.by_role.entry(role).or_default();
        slot.prompt += u.prompt;
        slot.completion += u.completion;
        slot.estimated |= u.estimated;
        self.entries += 1;
        if u.estimated {
            self.estimated_entries += 1;
        }
    }

    pub fn of(&self, role: Role) -> Usage {
        self.by_role.get(&role).copied().unwrap_or_default()
    }

    pub fn total(&self) -> u32 {
        self.by_role.values().map(|u| u.total()).sum()
    }

    /// 判断阶段的 token 占比。总量为 0 时返回 0，避免除零。
    pub fn judge_share(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        self.of(Role::Judge).total() as f64 / total as f64
    }

    /// 打印成一行，给 UI 状态栏和实验日志用。
    pub fn line(&self) -> String {
        format!(
            "judge {} / answer {} / subagent {} = {} tok，判断段占比 {:.1}%（估算记账 {}/{} 次）",
            self.of(Role::Judge).total(),
            self.of(Role::Answer).total(),
            self.of(Role::Subagent).total(),
            self.total(),
            self.judge_share() * 100.0,
            self.estimated_entries,
            self.entries,
        )
    }
}
