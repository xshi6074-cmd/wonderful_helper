//! 持久层：跨会话的蒸馏成果，**外加 harness 自己的提示词**。
//!
//! # 只有两层
//!
//! - **持久层**（这个文件）：跨会话。项目概述与进展、用户合作偏好、知识与经验评估、
//!   场景库、参考案例、提示词模板。目标是**让一个新对话能快速入手这个项目**。
//! - **推断层**（[`crate::state::Workspace`]）：一个会话内的推断图，由主时间线物化。
//!
//! 中间那个「会话层 / 工作 memory」没有：完整对话本来就要塞给模型，
//! 推断图已经承担了信息整合，再夹一层筛过的会话事实是重复劳动。
//!
//! # 文件就是界面
//!
//! 持久层**用户随时可以直接改**，包括内置的 bootstrap 内容。所以它落在人写得动的
//! 格式上：叙述性的内容用 Markdown，结构化的场景库与提示词用 TOML。
//! **刻意不进 SQLite** —— 它的 owner 是用户，塞进不透明的库等于把「用户可修改」
//! 这条设计约束押给一个还没写的 UI。
//!
//! ```text
//! <workspace>/memory/
//!   project.md        项目概述 · 阶段目标 · 进展（含已跑过的实验方法与成败）
//!   preferences.md    用户合作偏好
//!   knowledge.md      实验相关知识情况与经验评估
//!   playbook.toml     易犯错场景与对应指令
//!   prompts.toml      harness 注入的提示词模板
//!   cases/*.md        参考案例（TOML frontmatter + 正文）
//! ```
//!
//! # 每轮重读
//!
//! turn 在开头重新加载一次（见 [`crate::turn::run_turn`]）。所以用户改了任何一个
//! 文件，**下一轮立刻生效**，不必重启。读几个小文件是微秒级，不值得为它引一套
//! 文件监听。轮中不重读：同一轮的两段看到不同的持久层会很难排查。
//!
//! # 解析失败要报出来
//!
//! 用户把 `playbook.toml` 改坏了，回退到内置目录继续跑是对的（不能因为一个文件
//! 起不来），但**必须像界面报错一样报出来** —— 上一版只 eprintln，用户以为自己
//! 写的场景生效了，其实没有。见 [`Memory::warnings`]。

use crate::scene::Playbook;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};

const PROJECT: &str = "project.md";
const PREFERENCES: &str = "preferences.md";
const KNOWLEDGE: &str = "knowledge.md";
const PLAYBOOK: &str = "playbook.toml";
const PROMPTS: &str = "prompts.toml";
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

/// harness 注入的提示词模板。
///
/// 放进持久层而不是写死在 Rust 里，是为了兑现「改动立即生效」：
/// 这些是 metadata，每轮动态拼进 prompt，用户改完下一轮就变。
/// 写死在源码里的话，调一句话的措辞要重新编译。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompts {
    /// 角色与流程边界。
    pub role: String,
    /// 「改用户填过的字段之前先问」—— 删掉硬锁之后这条约束的落点。
    pub user_field: String,
    /// 折叠早期对话时给摘要模型的指令。
    pub fold: String,
    /// 探索 mode 的一句话。
    pub mode_explore: String,
    /// 行动 mode 的一句话。
    pub mode_go: String,
    /// 流程图的形状与配色词表。
    #[serde(default)]
    pub graph: GraphStyle,
}

/// 图的词表：kind → 形状 / 配色 / 连接符。
///
/// # 这是「不能用 mermaid 模板」的落点
///
/// 词表放在持久层而不是 Rust 的 enum 里，所以**用户随时能加一种节点类型、
/// 改一个形状，不用重编译**；模型也能现造 kind，未知的落到默认形状，不报错。
/// 灵活性放在词表开放，不放在让模型自由写文本 —— 后者会让节点失去稳定 id。
///
/// # 默认这套是给 ML 架构图的
///
/// 主场景是 ML，最有信息量的是架构图：数据 → 模块 → 算子 → 损失 → 指标 → 消融臂。
/// 不是实验步骤流水，也不是决策树 —— 决策树本来就难画好，`gate` 留了个形状但不主推。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStyle {
    /// kind → mermaid 形状模板，`%s` 是标签的位置。
    #[serde(default)]
    pub shape: BTreeMap<String, String>,
    /// kind → `classDef` 正文。没有条目就不生成 classDef。
    #[serde(default)]
    pub class: BTreeMap<String, String>,
    /// 边 kind → **实线**连接符。虚线不查这里 —— 那由 `prov` 决定，见 `crate::render`。
    #[serde(default)]
    pub edge: BTreeMap<String, String>,
}

/// 内置形状。用户的 toml 只覆盖它列到的那些 kind，剩下的仍走这里 ——
/// 否则用户加一个 `[graph.shape]` 小节就会把其余全部清空。
const BUILTIN_SHAPE: &[(&str, &str)] = &[
    ("data", "[(%s)]"),      // 数据集 / 张量存储
    ("module", "[%s]"),      // 模块
    ("op", "([%s])"),        // 算子 / 变换
    ("loss", "{{%s}}"),      // 损失
    ("metric", "[/%s/]"),    // 指标
    ("ablation", "[[%s]]"),  // 消融臂
    ("baseline", "[[%s]]"),  // 对照
    ("gate", "{%s}"),        // 判定（不主推）
    ("note", "(%s)"),
];

const BUILTIN_EDGE: &[(&str, &str)] = &[
    ("flow", "-->"),        // 张量 / 数据流
    ("feeds", "-->"),
    ("supervises", "==>"),  // 监督信号，加粗
    ("compares", "---"),    // 对比关系，无向
    ("depends", "-->"),
];

impl Default for GraphStyle {
    fn default() -> Self {
        GraphStyle {
            shape: BUILTIN_SHAPE.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            class: BTreeMap::new(),
            edge: BUILTIN_EDGE.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }
}

impl GraphStyle {
    /// 查形状：用户词表 → 内置 → 方框。**任何 kind 都渲染得出来，不会失败。**
    pub fn shape_for(&self, kind: &str) -> &str {
        if let Some(s) = self.shape.get(kind) {
            return s;
        }
        BUILTIN_SHAPE.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v).unwrap_or("[%s]")
    }

    /// 查连接符：同上，兜底是普通箭头。
    pub fn edge_for(&self, kind: &str) -> &str {
        if let Some(s) = self.edge.get(kind) {
            return s;
        }
        BUILTIN_EDGE.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v).unwrap_or("-->")
    }
}

impl Default for Prompts {
    fn default() -> Self {
        Prompts {
            role: "你是实验设计的研究助理。你不主导流程：是否进入实现由用户拍板，\
                   你可以在正文里建议收尾，但没有推进权，也不能因为「我觉得还没准备好」\
                   拦住用户。你的工作是把一个模糊的想法收敛成能交给编码 agent 的实验设计。"
                .into(),
            user_field: "推断图里标了 [用户设定] 的字段是用户自己填的。\
                         你可以提出不同意见，但**改动它之前先在正文里问一句**，不要直接覆盖。\
                         标了 [模型推断] 的可以直接更新。"
                .into(),
            fold: "把下面这段早期对话压成摘要，供后续对话继续使用。\n\
                   务必保住：口径与用户的原话约束、已经放弃的路线与放弃的理由、\
                   已经问过的问题、明确排除的可能性。\n\
                   这些正是推断图里没有、但后面会被引用的东西。叙述过程可以大幅压缩。"
                .into(),
            mode_explore: "当前是探索 mode：可以展开讲、可以开放式追问。\
                           抽取资料时关注动机、领域背景、术语定义。"
                .into(),
            mode_go: "当前是行动 mode：用户想往前推进，回答直接一些，不要倒回去讲基础。\
                      抽取资料时关注方法、超参、实现细节。"
                .into(),
            graph: GraphStyle::default(),
        }
    }
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
    pub prompts: Prompts,
    pub cases: Vec<Case>,
    /// 解析失败的文件。**必须报给用户**，否则他以为自己改的东西生效了。
    pub warnings: Vec<(String, String)>,
}

impl Memory {
    /// 读持久层；目录不存在或缺文件就写入 bootstrap 内容再读。
    ///
    /// 缺哪个补哪个，不是「整个目录不存在才 bootstrap」—— 用户可能删掉其中一个文件，
    /// 那次启动应该把它补回来，而不是让 agent 少一块记忆还不吭声。
    pub async fn load_or_bootstrap(dir: &FsPath) -> Memory {
        let _ = tokio::fs::create_dir_all(dir).await;
        let _ = tokio::fs::create_dir_all(dir.join(CASES)).await;
        let mut warnings = Vec::new();

        let project = read_or_write(dir, PROJECT, bootstrap_project()).await;
        let preferences = read_or_write(dir, PREFERENCES, bootstrap_preferences()).await;
        let knowledge = read_or_write(dir, KNOWLEDGE, bootstrap_knowledge()).await;

        let playbook = match tokio::fs::read_to_string(dir.join(PLAYBOOK)).await {
            Ok(s) => Playbook::from_toml_str(&s).unwrap_or_else(|e| {
                warnings.push((PLAYBOOK.into(), e));
                Playbook::builtin()
            }),
            Err(_) => {
                let pb = Playbook::builtin();
                let _ = tokio::fs::write(dir.join(PLAYBOOK), pb.to_toml_string()).await;
                pb
            }
        };

        let prompts = match tokio::fs::read_to_string(dir.join(PROMPTS)).await {
            Ok(s) => toml::from_str::<Prompts>(&s).unwrap_or_else(|e| {
                warnings.push((PROMPTS.into(), e.to_string()));
                Prompts::default()
            }),
            Err(_) => {
                let p = Prompts::default();
                if let Ok(s) = toml::to_string_pretty(&p) {
                    let _ = tokio::fs::write(dir.join(PROMPTS), s).await;
                }
                p
            }
        };

        let cases = load_cases(&dir.join(CASES)).await;
        Memory { project, preferences, knowledge, playbook, prompts, cases, warnings }
    }

    /// 空的持久层，用于测试与不落盘的场景。
    pub fn empty() -> Memory {
        Memory {
            project: String::new(),
            preferences: String::new(),
            knowledge: String::new(),
            playbook: Playbook::builtin(),
            prompts: Prompts::default(),
            cases: vec![],
            warnings: vec![],
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

    /// mode 的一句话。文本在持久层里，用户可改。
    pub fn mode_note(&self, mode: crate::model::Mode) -> &str {
        match mode {
            crate::model::Mode::Explore => &self.prompts.mode_explore,
            crate::model::Mode::Go => &self.prompts.mode_go,
        }
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
