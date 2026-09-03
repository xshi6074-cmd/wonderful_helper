//! 持久层：跨会话的蒸馏成果。
//!
//! # 只有两层
//!
//! - **持久层**（这个文件）：跨会话。场景库、案例、用户合作偏好、用户的实验知识与经验评估、
//!   项目概述与阶段目标与进展。目标是**让一个新对话能快速入手这个项目**。
//! - **推断层**（[`crate::state::Workspace`]）：一个会话内的推断图，turn 是它的事务边界。
//!
//! 中间那个「会话层 / 工作 memory」没有了。理由是完整对话本来就要塞给模型
//! （见 [`crate::context`] 的全量上下文策略），而推断图已经承担了信息整合，
//! 再夹一层筛选过的会话事实是重复劳动。
//!
//! # 文件就是界面
//!
//! 持久层**用户随时可以直接改**，包括内置的 bootstrap 内容。所以它落在人写得动的格式上：
//! 叙述性的内容用 Markdown，结构化的场景库用 TOML。不要用不透明的 json blob ——
//! 那等于把「用户可修改」这条设计约束交给一个还没写的 UI 去兑现。
//!
//! ```text
//! <workspace>/memory/
//!   project.md        项目概述 · 阶段目标 · 进展（含已跑过的实验方法与成败）
//!   preferences.md    用户合作偏好
//!   knowledge.md      实验相关知识情况与经验评估
//!   playbook.toml     易犯错场景与对应指令
//!   cases/*.md        参考案例（TOML frontmatter + 正文）
//! ```
//!
//! 首次运行时把 bootstrap 内容写进去；之后读到的就是用户改过的版本。

use crate::scene::Playbook;
use serde::{Deserialize, Serialize};
use std::path::{Path as FsPath, PathBuf};

const PROJECT: &str = "project.md";
const PREFERENCES: &str = "preferences.md";
const KNOWLEDGE: &str = "knowledge.md";
const PLAYBOOK: &str = "playbook.toml";
const CASES: &str = "cases";

/// 一条参考案例。文件形如：
///
/// ```text
/// ---
/// id = "reproduce-before-stacking"
/// title = "旧结论未复现就叠加"
/// scenes = ["check_assumption"]
/// ---
/// 正文……
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    pub title: String,
    /// 这条案例服务于哪些场景。判断段判出场景之后按它检索。
    #[serde(default)]
    pub scenes: Vec<String>,
    #[serde(default, skip)]
    pub body: String,
}

#[derive(Debug, Deserialize)]
struct CaseFront {
    id: String,
    title: String,
    #[serde(default)]
    scenes: Vec<String>,
}

/// 持久层的全部内容。
#[derive(Debug, Clone)]
pub struct Memory {
    /// 项目总体概述、阶段目标、进展（成功/失败的实验方法概述）。
    /// **新对话靠它快速入手。**
    pub project: String,
    /// 用户的合作偏好。
    pub preferences: String,
    /// 用户在各知识域的掌握程度与经验评估。决定 agent 是提问还是讲解。
    pub knowledge: String,
    /// 易犯错场景与对应指令。
    pub playbook: Playbook,
    pub cases: Vec<Case>,
}

impl Memory {
    /// 读持久层；目录不存在或缺文件就写入 bootstrap 内容再读。
    ///
    /// 缺哪个补哪个，不是「整个目录不存在才 bootstrap」—— 用户可能删掉其中一个文件，
    /// 那次启动应该把它补回来，而不是让 agent 少一块记忆还不吭声。
    pub async fn load_or_bootstrap(dir: &FsPath) -> Memory {
        let _ = tokio::fs::create_dir_all(dir).await;
        let _ = tokio::fs::create_dir_all(dir.join(CASES)).await;

        let project = read_or_write(dir, PROJECT, bootstrap_project()).await;
        let preferences = read_or_write(dir, PREFERENCES, bootstrap_preferences()).await;
        let knowledge = read_or_write(dir, KNOWLEDGE, bootstrap_knowledge()).await;

        let playbook = match tokio::fs::read_to_string(dir.join(PLAYBOOK)).await {
            Ok(s) => Playbook::from_toml_str(&s).unwrap_or_else(|e| {
                eprintln!("[memory] playbook.toml 解析失败（{e}），改用内置目录");
                Playbook::builtin()
            }),
            Err(_) => {
                let pb = Playbook::builtin();
                let _ = tokio::fs::write(dir.join(PLAYBOOK), pb.to_toml_string()).await;
                pb
            }
        };

        let cases = load_cases(&dir.join(CASES)).await;
        Memory { project, preferences, knowledge, playbook, cases }
    }

    /// 空的持久层，用于测试与不落盘的场景。
    pub fn empty() -> Memory {
        Memory {
            project: String::new(),
            preferences: String::new(),
            knowledge: String::new(),
            playbook: Playbook::builtin(),
            cases: vec![],
        }
    }

    /// 组装进上下文的那一段。跨轮不变，可命中 prompt cache。
    pub fn prompt_block(&self) -> String {
        let mut s = String::from("== 关于这个项目 ==\n");
        s.push_str(trim_or(&self.project, "（尚无项目记录）"));
        s.push_str("\n\n== 关于这位用户 ==\n");
        s.push_str(trim_or(&self.preferences, "（尚无合作偏好记录）"));
        s.push_str("\n\n== 用户的知识与经验评估 ==\n");
        s.push_str(trim_or(&self.knowledge, "（尚无评估）"));
        s.push('\n');
        s
    }

    /// 命中某个场景的案例正文。
    pub fn cases_for(&self, scene: &str) -> String {
        self.cases
            .iter()
            .filter(|c| c.scenes.iter().any(|s| s == scene))
            .map(|c| format!("### {}\n{}", c.title, c.body.trim()))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

fn trim_or<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.trim().is_empty() { fallback } else { s.trim() }
}

async fn read_or_write(dir: &FsPath, name: &str, bootstrap: String) -> String {
    let p = dir.join(name);
    match tokio::fs::read_to_string(&p).await {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::fs::write(&p, &bootstrap).await;
            bootstrap
        }
    }
}

async fn load_cases(dir: &FsPath) -> Vec<Case> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return out };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = tokio::fs::read_to_string(&path).await else { continue };
        if let Some(c) = parse_case(&text) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn parse_case(text: &str) -> Option<Case> {
    let rest = text.strip_prefix("---")?;
    let (front, body) = rest.split_once("\n---")?;
    let f: CaseFront = toml::from_str(front.trim()).ok()?;
    Some(Case { id: f.id, title: f.title, scenes: f.scenes, body: body.trim().to_string() })
}

/// 一键蒸馏产出的草稿落盘位置。
///
/// **写成草稿而不是直接覆盖持久层**：蒸馏是模型对整段会话的概括，
/// 直接盖掉用户手写的记忆是一次不该有的越权。用户看过、改过再合并。
pub fn draft_path(dir: &FsPath, stamp: u64) -> PathBuf {
    dir.join(format!("draft-{stamp}.md"))
}

// ────────────────────────── bootstrap ──────────────────────────
//
// 这些是首次运行写进文件的初始内容，之后归用户所有。
// 措辞刻意写成「等着被填」的样子，让用户一打开就知道这里该写什么。

fn bootstrap_project() -> String {
    "# 项目\n\n\
     ## 概述\n（这个项目在做什么、验证什么大方向。写给一个刚接手的新对话看。）\n\n\
     ## 当前阶段目标\n（这一阶段要拿到的结论是什么。）\n\n\
     ## 进展\n\
     ### 已经跑通并成立的\n（方法概述 + 结论。）\n\n\
     ### 试过但没成立的\n（方法概述 + 为什么没成立。这一栏比上一栏值钱。）\n"
        .into()
}

fn bootstrap_preferences() -> String {
    "# 合作偏好\n\n\
     - 讲解详略：（更想要结论，还是更想要推导过程）\n\
     - 提问方式：（能接受开放式问题，还是需要带候选项）\n\
     - 什么时候希望被打断：（比如「方案明显跑偏时立刻说」）\n\
     - 不希望 agent 做的事：（比如「别替我改实验参数」）\n"
        .into()
}

fn bootstrap_knowledge() -> String {
    "# 知识与经验评估\n\n\
     每条形如：`领域 / 具体点 — 掌握程度 — 依据`。掌握程度用三档：\n\
     **有储备**（可以直接问他）、**没储备**（先讲通机制再把判断交回）、**不清楚**（先问清楚这一点本身）。\n\n\
     - 例：持续学习 / BWT 与 forgetting measure — 不清楚 — 尚未在对话中出现\n"
        .into()
}
