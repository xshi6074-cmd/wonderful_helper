//! 上下文组装。**每轮从当前状态现拼，不存拼好的结果。**
//!
//! # 为什么 metadata 不进时间线
//!
//! 时间线上只有「谁说了什么、改了什么」这类原始事实。system 段、持久层内容、
//! 推断图、场景 guidance、参考案例，全部是**组装时现拼**的。
//!
//! 好处是改动立刻生效：用户改了 `memory/preferences.md`、换了场景、
//! 编辑了推断图，下一次组装就是新的，不需要迁移任何历史数据，也不会出现
//! 「历史里存着一份三十轮前的旧 system prompt」。
//!
//! 代价是拼装要够快。实测一轮几十条事件，拼一次是微秒级。
//!
//! # 五段分层，前三段跨轮不变
//!
//! ```text
//! ① system 规则        ┐
//! ② 持久层             ├ 跨轮不变 ⇒ 命中 prompt cache
//! ③ 场景目录           ┘
//! ④ 流程图 + 图外推断 + 待落定  每轮变
//! ⑤ 完整对话           每轮追加
//! ```
//!
//! 判断段与回答段吃**同一份对话**：判断「这一轮该进哪个场景」本来就要看用户
//! 刚说了什么。两段式省下的是场景 guidance 与案例（八份 guidance 全塞进判断段，
//! 它就跟回答段一样重了）。

use crate::event::{Event, assemble};
use crate::ids::Seq;
use crate::memory::Memory;
use crate::model::Message;
use crate::scene::Scene;
use crate::state::Workspace;

/// 推断段的小标题。有图时说「图外」，没图时说「当前」。
const HEAD_ALL: &str = "== 当前推断 ==\n";
const HEAD_OFF: &str = "== 图外推断 ==\n";

/// 3 字符 ≈ 1 token，中英混排的折中。宁可高估。
pub fn toks(s: &str) -> u32 {
    (s.chars().count() / 3) as u32
}

pub struct Context {
    /// ① 规则。来自持久层的 `prompts.toml`，用户可改。
    pub system: String,
    /// ② 持久层：项目 / 偏好 / 知识评估。
    pub memory: String,
    /// ③ 场景目录（只有 id 与触发条件，不含 guidance）。
    pub catalog: String,
    /// ④ 推断图 + 待落定 + 搁置。
    pub inference: String,
    /// ⑤ 完整对话。
    pub history: Vec<Message>,
}

impl Context {
    pub fn build(
        ws: &Workspace,
        turn_start: Seq,
        memory: &Memory,
        history: Vec<Message>,
        mode_note: &str,
    ) -> Context {
        let p = &memory.prompts;
        let mut system = String::new();
        system.push_str(p.role.trim());
        system.push_str("\n\n");
        system.push_str(p.user_field.trim());
        system.push_str("\n\n");
        system.push_str(mode_note.trim());
        system.push_str(&format!("\n\n当前阶段：{}。", ws.phase.label()));

        // 图在最前：拓扑是这段 metadata 里信息密度最高的东西，而且后面的图外推断
        // 要靠它才知道自己挂在哪儿。两种画法（程序画 / 模型画）在这里都收敛成源码。
        let flow = crate::render::flow_block(ws, &memory.prompts.graph, turn_start);
        let mut inference = String::new();
        if !flow.is_empty() {
            inference.push_str(&flow);
            inference.push('\n');
        }
        // 有图时说清楚这一段是「图外」的，免得模型以为图里少了东西。
        inference.push_str(if flow.is_empty() { HEAD_ALL } else { HEAD_OFF });
        let lines = ws.prompt_lines(turn_start);
        if lines.is_empty() {
            inference.push_str("（还没有任何推断）\n");
        } else {
            for l in &lines {
                inference.push_str(&format!("- {l}\n"));
            }
        }
        // 空就完全不出现 —— 不要给模型制造「这里本该有个我没填的东西」的错觉。
        if !ws.open.is_empty() {
            inference.push_str("\n== 待落定 ==\n");
            for q in &ws.open {
                inference.push_str(&format!("- {q}\n"));
            }
        }
        if !ws.parked.is_empty() {
            inference.push_str("\n== 已搁置 ==\n");
            for q in &ws.parked {
                inference.push_str(&format!("- {q}\n"));
            }
        }

        Context {
            system,
            memory: memory.prompt_block(),
            catalog: memory.playbook.catalog(),
            inference,
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

    /// 判断段：共享层 + 完整对话，**不含任何场景的 guidance 与案例**。
    pub fn for_judge(&self) -> Vec<Message> {
        let mut v = self.shared();
        v.extend(self.history.iter().cloned());
        v
    }

    /// 回答段：共享层 + 本轮场景的 guidance 与案例 + 检索材料 + 完整对话。
    ///
    /// **场景选定之后要真的灌进来** —— 只把场景 id 记在某个字段里、prompt 却没变，
    /// 等于用户换了半天场景模型什么都没感觉到。
    pub fn for_answer(&self, scene: &Scene, cases: &str, retrieved: &[Message]) -> Vec<Message> {
        let mut v = self.shared();
        if !scene.guidance.trim().is_empty() {
            v.push(Message::system(format!(
                "== 本轮场景：{} ==\n触发条件：{}\n\n{}",
                scene.label,
                scene.when.trim(),
                scene.guidance.trim()
            )));
        }
        if !cases.trim().is_empty() {
            v.push(Message::system(format!("== 相关案例 ==\n{}", cases.trim())));
        }
        v.extend(retrieved.iter().cloned());
        v.extend(self.history.iter().cloned());
        v
    }

    pub fn footprint(&self) -> Footprint {
        let cacheable = toks(&self.system) + toks(&self.memory) + toks(&self.catalog);
        let inference = toks(&self.inference);
        let history = self.history.iter().map(|m| toks(&m.content)).sum();
        Footprint { cacheable, inference, history }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Footprint {
    pub cacheable: u32,
    pub inference: u32,
    pub history: u32,
}

impl Footprint {
    pub fn total(&self) -> u32 {
        self.cacheable + self.inference + self.history
    }
}

/// 上下文上限与压缩水位。
#[derive(Debug, Clone, Copy)]
pub struct ContextLimit {
    pub max_prompt_tokens: u32,
    /// 到几成就折叠。留余量是因为折叠本身要一次模型调用，而这一轮的回答还没生成 ——
    /// 压到 100% 才动手，这一轮就直接超限失败。
    pub trigger_at: f32,
    /// 最近几轮绝不折叠。用户刚说的话被折进摘要是最糟的一种压缩。
    pub keep_recent_turns: usize,
}

impl Default for ContextLimit {
    fn default() -> Self {
        Self { max_prompt_tokens: 128_000, trigger_at: 0.8, keep_recent_turns: 6 }
    }
}

/// 折叠方案。`from..=to` 是要折的事件区间，落成一条 `Folded` 事件。
pub enum CompactPlan {
    Keep,
    Fold { from: Seq, to: Seq, msgs: Vec<Message>, count: u32 },
}

/// 算这一轮要不要折叠、折哪一段。
///
/// # 位置很重要
///
/// 折叠发生在**判断段之前**，那时上一轮的推断已经提交进图了 ——
/// 值得留下的结构化信息在图里，被折叠的是叙述过程。
///
/// 折的是**事件区间**而不是消息区间：区间要落成一条 `Folded{from,to}` 事件，
/// 原始事件一条不删，UI 能展开、蒸馏能读全程。
pub fn plan_compaction(events: &[Event], fixed: u32, limit: &ContextLimit) -> CompactPlan {
    let msgs = assemble(events);
    let used = fixed + msgs.iter().map(|m| toks(&m.content)).sum::<u32>();
    if (used as f32) < limit.max_prompt_tokens as f32 * limit.trigger_at {
        return CompactPlan::Keep;
    }
    let split = split_at_recent(events, limit.keep_recent_turns);
    if split == 0 {
        return CompactPlan::Keep;
    }
    let head = &events[..split];
    let msgs = assemble(head);
    if msgs.is_empty() {
        return CompactPlan::Keep;
    }
    CompactPlan::Fold {
        from: head[0].seq,
        to: head[head.len() - 1].seq,
        msgs,
        count: head.len() as u32,
    }
}

/// 找「保留最近 k 轮」的分界下标。
///
/// 轮次由事件自己带（`Event::turn`），不需要靠扫消息角色去猜边界。
///
/// # 分界要往前退到那一轮的**触发输入**
///
/// 用户那句话是在 `TurnOpened` **之前**落盘的（按下发送就写，然后才开轮），
/// 而且它的 `turn` 是 `None`。只按 `TurnOpened` 切，会把提问折进摘要、
/// 把回答留在外面 —— 模型看到一个没有问题的答案。所以分界点要继续往前退，
/// 越过紧挨着的那几条轮外事件（用户输入、回答、侧栏编辑）。
pub fn split_at_recent(events: &[Event], k_turns: usize) -> usize {
    let mut turns: Vec<crate::ids::TurnId> = Vec::new();
    for e in events {
        if let Some(t) = e.turn
            && turns.last() != Some(&t) {
                turns.push(t);
            }
    }
    if turns.len() <= k_turns {
        return 0;
    }
    let boundary = turns[turns.len() - k_turns];
    let mut i = match events.iter().position(|e| e.turn == Some(boundary)) {
        Some(i) => i,
        None => return 0,
    };
    // 往前退过属于这一轮但还没被打上轮号的用户动作
    while i > 0 && events[i - 1].turn.is_none() {
        i -= 1;
    }
    i
}

/// 摘要指令。正文来自持久层的 `prompts.toml`，用户可改；这里只负责把推断图拼上去。
///
/// 把图一起给摘要模型是有意的：告诉它「图里已经有的不要重复」，
/// 它才会去保住图上没有、但后面会被引用的那些东西 ——
/// 口径与原话约束、放弃的路线与理由、已问过的问题、明确排除的可能性。
pub fn fold_instruction(tmpl: &str, inference: &str) -> String {
    format!("{}\n\n当前推断图（这里已经有的不要在摘要里重复）：\n{}", tmpl.trim(), inference)
}
