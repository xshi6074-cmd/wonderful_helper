//! 上下文组装与压缩。
//!
//! # 分层
//!
//! ```text
//!   system prompt                ┐
//!   持久层（项目 / 偏好 / 知识）    ├ 跨轮不变 → prompt cache 命中到这里
//!   场景目录（id + when）          ┘
//!   推断图                         每轮变
//!   完整对话                       每轮增长
//! ```
//!
//! **判断段和回答段吃同一份对话。** 早先想让判断段不含 history 来省钱，
//! 但判断「这一轮该进哪个场景」本来就要看用户刚才说了什么、之前谈到哪了 ——
//! 砍掉历史等于让判断变瞎。两段式省下的是别的东西：判断段不带任何场景的 guidance
//! （八份 guidance 全塞进去它就跟回答段一样重），也不带参考案例。
//!
//! # 上下文策略：灌全量，接近上限才压缩
//!
//! 只送最近 K 轮是不现实的 —— 用户会引用三十轮之前定下的东西。所以送全量，
//! 估算到接近模型上限时再压缩一次。
//!
//! 压缩必然有损，所以顺序很重要：**压缩发生在判断段之前、紧跟在上一轮的推断提交之后**。
//! 那时推断图刚被更新过，会话里值得留下的结构化信息已经进图了，被折叠的是叙述过程。
//! 折叠时把当前推断图一并交给摘要模型，并明确告诉它：图里已有的不要重复，
//! 只保留图上没有、但后面还会被引用的事实（谁说过什么口径、放弃了哪条路线、为什么）。

use crate::memory::Memory;
use crate::model::{Message, MsgRole};
use crate::scene::Scene;
use crate::state::StateView;

/// 系统提示里那条替代了「用户编辑加锁」的约束。
///
/// 早期设计用 `locked: HashSet<Path>` 在 core 里硬拦模型的写入。删掉之后，
/// 这条约束的唯一落点就是这句话 + 推断图里的 `[用户设定]` 标记。
const USER_FIELD_RULE: &str = "推断图里标了 [用户设定] 的字段是用户自己填的。\
你可以提出不同意见，但**改动它之前先在正文里问一句**，不要直接覆盖。\
标了 [模型推断] 的可以直接更新。";

const ROLE_RULE: &str = "你不主导流程。是否进入实现由用户拍板 —— 你可以给出 brief 和建议，\
但不要替用户宣布开始。用户随时可以打断你或插话，插话的优先级高于你正在说的话。";

/// 组装好的分层上下文。
pub struct Context {
    /// ① 跨轮不变：系统提示。
    pub system: String,
    /// ② 跨轮不变：持久层。项目概述与进展、用户合作偏好、用户的知识与经验评估。
    pub memory: String,
    /// ③ 跨轮不变：场景目录（只有 id + when）。
    pub catalog: String,
    // ────────── prompt cache 命中到这里 ──────────
    /// ④ 每轮变：推断图。
    pub inference: String,
    /// ⑤ 每轮增长：完整对话。判断段和回答段吃的是同一份。
    pub history: Vec<Message>,
}

impl Context {
    pub fn build(
        view: &StateView,
        memory: &Memory,
        history: Vec<Message>,
        mode_note: &str,
    ) -> Context {
        Context {
            system: format!("{ROLE_RULE}\n\n{USER_FIELD_RULE}\n\n{mode_note}"),
            memory: memory.prompt_block(),
            catalog: format!("可选的协作场景：\n{}", memory.playbook.catalog()),
            inference: render_inference(view),
            history,
        }
    }

    fn shared(&self) -> Vec<Message> {
        vec![
            Message::system(&self.system),
            Message::system(&self.memory),
            Message::system(&self.catalog),
            Message::system(&self.inference),
        ]
    }

    /// 判断段：共享层 + 完整对话。**不含任何场景的 guidance，也不含案例。**
    pub fn for_judge(&self) -> Vec<Message> {
        let mut v = self.shared();
        v.extend(self.history.iter().cloned());
        v
    }

    /// 回答段：判断段那份 + 选中场景的 guidance 与案例 + 本轮检索到的材料。
    pub fn for_answer(&self, scene: &Scene, cases: &str, retrieved: &[Message]) -> Vec<Message> {
        let mut v = self.shared();
        // 场景的约束与建议 —— 注入，不是执行
        v.push(Message::system(format!(
            "本轮场景：{} · {}\n{}",
            scene.id, scene.label, scene.guidance
        )));
        if !cases.trim().is_empty() {
            v.push(Message::system(format!("参考案例：\n{cases}")));
        }
        v.extend(retrieved.iter().cloned());
        v.extend(self.history.iter().cloned());
        v
    }

    /// 各段占用，供 UI 的占用/成本展示与压缩决策用。
    pub fn footprint(&self) -> Footprint {
        Footprint {
            cacheable: toks(&self.system) + toks(&self.memory) + toks(&self.catalog),
            inference: toks(&self.inference),
            history: self.history.iter().map(|m| toks(&m.content)).sum(),
            turns: self.history.iter().filter(|m| matches!(m.role, MsgRole::User)).count(),
        }
    }
}

/// 粗估 token：3 字符 ≈ 1 token，中英混排的折中。宁可高估。
pub fn toks(s: &str) -> u32 {
    (s.chars().count() / 3) as u32
}

pub fn msgs_toks(ms: &[Message]) -> u32 {
    ms.iter().map(|m| toks(&m.content)).sum()
}

#[derive(Debug, Clone, Copy)]
pub struct Footprint {
    pub cacheable: u32,
    pub inference: u32,
    pub history: u32,
    pub turns: usize,
}

impl Footprint {
    pub fn total(&self) -> u32 {
        self.cacheable + self.inference + self.history
    }

    /// 可缓存占比。这个数字直接决定了两段式循环的实际成本。
    pub fn cache_share(&self) -> f64 {
        let t = self.total();
        if t == 0 { 0.0 } else { self.cacheable as f64 / t as f64 }
    }

    pub fn line(&self) -> String {
        format!(
            "上下文 ~{} tok（可缓存 {} / 推断图 {} / 对话 {}·{}轮），可缓存占比 {:.0}%",
            self.total(),
            self.cacheable,
            self.inference,
            self.history,
            self.turns,
            self.cache_share() * 100.0
        )
    }
}

// ────────────────────────── 压缩 ──────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct ContextLimit {
    /// 模型的上下文上限（token）。
    pub max_prompt_tokens: u32,
    /// 触发压缩的水位。0.8 表示用到八成就压。
    ///
    /// 留两成余量是因为压缩本身也要一次模型调用，而且这一轮的回答还没生成。
    /// 压到 100% 才动手，那一轮就直接超限失败了。
    pub trigger_at: f32,
    /// 压缩时保留最近多少轮原文不折叠。
    pub keep_recent_turns: usize,
}

impl Default for ContextLimit {
    fn default() -> Self {
        Self { max_prompt_tokens: 128_000, trigger_at: 0.8, keep_recent_turns: 6 }
    }
}

pub enum CompactPlan {
    /// 还没到水位，什么都不用做。
    Keep,
    /// 把 `fold` 折叠成一段摘要，`keep` 保留原文。
    Fold { fold: Vec<Message>, keep: Vec<Message> },
}

/// 决定要不要压缩、压哪一段。
///
/// `fixed` 是共享层（system + 持久层 + 场景目录 + 推断图）的占用 —— 它们不可折叠，
/// 所以真正能腾出来的只有对话那一段。
pub fn plan_compaction(history: &[Message], fixed: u32, limit: &ContextLimit) -> CompactPlan {
    let used = fixed + msgs_toks(history);
    let trigger = (limit.max_prompt_tokens as f32 * limit.trigger_at) as u32;
    if used < trigger {
        return CompactPlan::Keep;
    }
    let cut = split_at_recent(history, limit.keep_recent_turns);
    if cut == 0 {
        // 最近 K 轮本身就撑爆了，折叠更早的也救不了。交给模型侧的截断兜底，
        // 这里不做「连最近几轮也折叠」——那会让用户刚说的话被摘要，体验最差。
        return CompactPlan::Keep;
    }
    CompactPlan::Fold { fold: history[..cut].to_vec(), keep: history[cut..].to_vec() }
}

/// 返回「最近 k 轮」的起始下标。一轮以一条 user 消息开头。
pub fn split_at_recent(history: &[Message], k_turns: usize) -> usize {
    let mut seen = 0;
    for (i, m) in history.iter().enumerate().rev() {
        if matches!(m.role, MsgRole::User) {
            seen += 1;
            if seen > k_turns {
                return i + 1;
            }
        }
    }
    0
}

/// 交给摘要模型的指令。
///
/// 两条硬要求：**别重复推断图已有的**（那是浪费，而且会让两处说法漂移），
/// **保住图上没有但后面会被引用的东西**（口径、被放弃的路线及原因、用户的原话约束）。
pub fn fold_instruction(inference: &str) -> String {
    format!(
        "把下面这段较早的对话压成一段摘要，它将替代原文留在上下文里。\n\n\
         当前推断图已经记录了这些内容，**不要重复**：\n{inference}\n\n\
         摘要必须保住图上没有、但后面还可能被引用的东西：\n\
         - 用户对某个说法的口径与原话约束\n\
         - 试过并放弃的路线，以及放弃的理由\n\
         - 已经问过并得到回答的问题（免得后面重复问）\n\
         - 明确排除掉的可能性\n\n\
         不要评价，不要补全，只记录发生过什么。"
    )
}

/// 折叠结果在历史里的样子。
pub fn folded_message(summary: &str, folded: usize) -> Message {
    let mut m = Message::system(format!(
        "[已折叠的早期对话摘要 · 原 {folded} 条]\n{summary}"
    ));
    m.interrupted = false;
    m
}

// ────────────────────────── 渲染 ──────────────────────────

/// 把推断层渲染成 prompt 的一段。
fn render_inference(view: &StateView) -> String {
    let mut s = String::from("== 当前推断 ==\n");
    let lines = view.prompt_lines();
    if lines.is_empty() {
        s.push_str("（还没有任何推断）\n");
    } else {
        for l in lines {
            s.push_str(&l);
            s.push('\n');
        }
    }
    // 待落定 / 搁置区是可选的：没有就完全不出现，不占 prompt，也不提示模型去填。
    if !view.ws.open.is_empty() {
        s.push_str("\n== 待落定 ==\n");
        for q in &view.ws.open {
            s.push_str(&format!("- {q}\n"));
        }
    }
    if !view.ws.parked.is_empty() {
        s.push_str("\n== 搁置 ==\n");
        for q in &view.ws.parked {
            s.push_str(&format!("- {q}\n"));
        }
    }
    s.push_str(&format!("\n阶段：{}（推进由用户拍板）\n", view.ws.phase.label()));
    s
}
