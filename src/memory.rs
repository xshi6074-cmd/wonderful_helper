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
use std::hash::{Hash, Hasher};
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
/// scenes = ["need_ablation"]
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

/// 一个 mode 的全套提示词。
///
/// # 为什么 mode 不只是一句话
///
/// 上一版 mode 就是往 system 段塞一行字（外加行动 mode 关掉讲解场景）。
/// 那不够：探索期和行动期该暴露的**工具**不一样，同一个工具该怎么用也不一样 ——
/// 探索期的 `record_graph` 是「先把结构大致摆出来，拿不准就标 guess」，
/// 行动期是「图要能直接交给编码 agent，写清楚具体的模块名和指标名」。
///
/// 全部放在 `prompts.toml` 里，所以换 mode 的行为**改文件就能调**，不用重编译。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModePrompt {
    /// 拼进 system 段的那一句（或那几句）。
    #[serde(default)]
    pub note: String,
    /// 这个 mode **额外**暴露的工具，叠在场景的 default_tools 之上。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 工具名 → 追加到它 description 后面的一句。同一个工具在两个 mode 下说明不同。
    #[serde(default)]
    pub tool_notes: BTreeMap<String, String>,
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
    /// 场景判定角色的独立任务说明。判断段不再复用主助手的交付提示词。
    #[serde(default)]
    pub judge: String,
    /// 折叠早期对话时给摘要模型的指令。
    pub fold: String,
    /// 一键蒸馏角色的独立任务说明。
    #[serde(default)]
    pub distill: String,
    /// 两个 mode 各自的提示词与工具。键是 `explore` / `go`。
    #[serde(default)]
    pub mode: BTreeMap<String, ModePrompt>,
    /// 旧字段：mode 只有一句话的那一版。**只在 `[mode.*].note` 为空时兜底**，
    /// 这样老的 prompts.toml 不会因为多一个小节就整份解析失败、静默回退到内置内容。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode_explore: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
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
        builtin_prompts()
    }
}

/// 仓库里的 `memory/prompts.toml` 同时是项目默认配置和冷启动模板。
/// 只保留这一份正文，避免修改项目提示词后忘记同步一套 Rust 字符串。
fn builtin_prompts() -> Prompts {
    toml::from_str(include_str!("../memory/prompts.toml"))
        .expect("内置 memory/prompts.toml 必须是合法 Prompts TOML")
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
            Ok(s) => toml::from_str::<Prompts>(&s).map(fill_prompt_defaults).unwrap_or_else(|e| {
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

    /// 命中**这一组**场景里任意一个的案例正文。
    ///
    /// 并集去重：一条案例同时服务两个命中的场景时，只注入一次。
    pub fn cases_for(&self, scenes: &[String]) -> String {
        self.cases
            .iter()
            .filter(|c| c.scenes.iter().any(|s| scenes.iter().any(|w| w == s)))
            .map(|c| format!("### {}\n{}", c.title, c.body.trim()))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// 蒸馏模型不能假设当前工具工作区就是本仓库，所以把允许更新的持久层原文
    /// 直接作为输入提供。优先读取磁盘原文以保留用户格式；读取失败才用已加载内容兜底。
    pub async fn distill_sources(&self, dir: &FsPath) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (name, fallback) in [
            (PROJECT, self.project.as_str()),
            (PREFERENCES, self.preferences.as_str()),
            (KNOWLEDGE, self.knowledge.as_str()),
        ] {
            let text = tokio::fs::read_to_string(dir.join(name)).await
                .unwrap_or_else(|_| fallback.to_string());
            out.insert(name.to_string(), text);
        }
        let playbook = tokio::fs::read_to_string(dir.join(PLAYBOOK)).await
            .unwrap_or_else(|_| self.playbook.to_toml_string());
        out.insert(PLAYBOOK.to_string(), playbook);

        if let Ok(mut entries) = tokio::fs::read_dir(dir.join(CASES)).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                if let Ok(text) = tokio::fs::read_to_string(&path).await {
                    out.insert(format!("cases/{name}"), text);
                }
            }
        }
        out
    }

    /// 这个 mode 的全套提示词，缺项按「文件 → 旧字段 → 内置」逐级兜底。
    ///
    /// 逐项兜底而不是整份兜底：用户只想改一句 note，不该因此把工具清单清空。
    pub fn mode(&self, mode: crate::model::Mode) -> ModePrompt {
        let key = mode.key();
        let mut m = self.prompts.mode.get(key).cloned().unwrap_or_default();
        let builtin = builtin_modes();
        if m.note.trim().is_empty() {
            let legacy = match mode {
                crate::model::Mode::Explore => &self.prompts.mode_explore,
                crate::model::Mode::Go => &self.prompts.mode_go,
            };
            m.note = if legacy.trim().is_empty() {
                builtin.get(key).map(|d| d.note.clone()).unwrap_or_default()
            } else {
                legacy.clone()
            };
        }
        if m.tools.is_empty() {
            m.tools = builtin.get(key).map(|d| d.tools.clone()).unwrap_or_default();
        }
        if let Some(default) = builtin.get(key) {
            for (tool, note) in &default.tool_notes {
                m.tool_notes.entry(tool.clone()).or_insert_with(|| note.clone());
            }
        }
        m
    }
}

fn fill_prompt_defaults(mut prompts: Prompts) -> Prompts {
    let default = builtin_prompts();
    if prompts.judge.trim().is_empty() {
        prompts.judge = default.judge;
    }
    if prompts.distill.trim().is_empty() {
        prompts.distill = default.distill;
    }
    prompts
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


/// 两个 mode 的内置内容。bootstrap 时落成 `prompts.toml` 的 `[mode.*]` 小节。
///
/// 措辞是**想法级**的，等着被重写 —— 提示词本来就该在实测里改，
/// 而改它只要动这个文件，不用重编译。
pub fn builtin_modes() -> BTreeMap<String, ModePrompt> {
    builtin_prompts().mode
}

// ───────────────────────── 蒸馏草稿 ─────────────────────────
//
// # 为什么草稿要分节
//
// 上一版蒸馏产出是一整篇 Markdown，落成 `draft-<stamp>.md` 就完了 ——
// 要让它生效，用户得自己打开草稿、自己判断哪一段属于哪个文件、自己复制粘贴。
// 那不是「一键」，那是把最没意思的一步留给了人。
//
// 现在模型按目标文件分节输出，程序解析成 [`Section`]，UI 逐节预览 / 修改 /
// 勾选，一次写回。**写回前要校验**：TOML 追加坏了会让整个 playbook 失效，
// 而那种失效要到下一轮才以「场景全没了」的形式冒出来。

/// 一节草稿写到哪个文件、怎么写。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub file: String,
    pub mode: WriteMode,
    pub text: String,
    /// 模型生成草稿时看到的源文件版本。写回前比较，避免覆盖审阅期间的并发编辑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// 整份替换。三个叙述性文件走这条：模型手里有现有内容，写的是更新后的整份。
    Replace,
    /// 按场景 id 合并。草稿只展示新增/更新的完整场景块，未输出的场景不动。
    Merge,
}

/// 这个文件名是不是合法的蒸馏目标，以及该怎么写。
pub fn write_mode_for(file: &str) -> Option<WriteMode> {
    match file {
        PROJECT | PREFERENCES | KNOWLEDGE => Some(WriteMode::Replace),
        PLAYBOOK => Some(WriteMode::Merge),
        // 一个 cases 文件就是一条案例，同名即更新它
        f if f.starts_with("cases/") && f.ends_with(".md") && f.matches('/').count() == 1 => {
            Some(WriteMode::Replace)
        }
        _ => None,
    }
}

/// 从草稿正文里切出各节。节标题形如 `## project.md`。
///
/// 认不出的标题连同它下面的正文一起归到**上一节**，而不是丢掉 ——
/// 模型多写一个 `### 概述` 是常事，那不该让整节消失。
/// 第一个合法标题之前的内容（模型的开场白之类）直接扔掉。
pub fn parse_draft(text: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for line in text.lines() {
        let target = line
            .strip_prefix("## ")
            .map(str::trim)
            .and_then(|t| write_mode_for(t).map(|m| (t.to_string(), m)));
        match target {
            Some((file, mode)) => out.push(Section {
                file,
                mode,
                text: String::new(),
                base_revision: None,
            }),
            None => {
                if let Some(s) = out.last_mut() {
                    s.text.push_str(line);
                    s.text.push('\n');
                }
            }
        }
    }
    out.retain(|s| !s.text.trim().is_empty());
    for s in &mut out {
        s.text = s.text.trim().to_string();
    }
    out
}

/// 写回之前校验。**结构化文件坏了要当场拦住**，不能等到下一轮
/// 以「场景全没了」「案例不见了」的形式冒出来。
pub fn validate(file: &str, merged: &str) -> Result<(), String> {
    if file == PLAYBOOK {
        Playbook::from_toml_str(merged).map(|_| ()).map_err(|e| format!("合并后不是合法的 playbook：{e}"))
    } else if file.starts_with("cases/") {
        match parse_case(merged) {
            Some(_) => Ok(()),
            None => Err("案例要以 TOML frontmatter 开头：--- 换行 id/title/scenes 换行 --- 换行 正文".into()),
        }
    } else {
        Ok(())
    }
}

/// 案例引用的是合并后的场景库，而不是只检查 frontmatter 能否解析。
/// 供蒸馏写回的预检使用，避免案例先落盘、下一轮才发现引用了不存在的场景。
pub fn validate_case_dependencies(
    file: &str,
    content: &str,
    playbook: &Playbook,
) -> Result<(), String> {
    if !file.starts_with("cases/") {
        return Ok(());
    }
    let case = parse_case(content).ok_or_else(|| {
        "案例要以 TOML frontmatter 开头：--- 换行 id/title/scenes 换行 --- 换行 正文"
            .to_string()
    })?;
    let missing = case
        .scenes
        .iter()
        .filter(|id| playbook.get(id).is_none())
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("案例引用了合并后仍不存在的场景：{}", missing.join("、")))
    }
}

/// 审阅草稿期间的并发修改检测。只在本进程内比较，不把它当内容身份或安全哈希。
pub fn content_revision(content: Option<&str>) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match content {
        Some(text) => {
            "present".hash(&mut h);
            text.hash(&mut h);
        }
        None => "missing".hash(&mut h),
    }
    format!("{:016x}", h.finish())
}

// ────────────────────────── bootstrap ──────────────────────────
//
// 这些是首次运行写进文件的初始内容，之后归用户所有。
// 措辞刻意写成「等着被填」的样子，让用户一打开就知道这里该写什么。

fn bootstrap_project() -> String {
    "# 项目记忆\n\n尚无已确认的项目记录。新增项目时按项目分节，保留概述与依据、阶段目标、当前有效设计、实际进展、尝试及结果、待决事项和下一步。\n\n设计已选、代码已改、检查通过、实验支持分别记录。结果未知或原因未证实时明确标注；保留被替代路线中仍有价值的理由。\n".into()
}

fn bootstrap_preferences() -> String {
    "# 合作偏好\n\n尚无已确认的长期偏好。根据明确要求或实际反馈记录讲解、提问、结构化材料、分工、拒绝事项、交流方式和固定流程，并注明适用范围与依据。\n\n单次拒绝不扩大为永久禁令；当前明确要求优先于旧偏好；没有反馈不代表接受。\n".into()
}

fn bootstrap_knowledge() -> String {
    "# 知识版图\n\n尚无足够证据判断具体知识点的掌握情况。记录领域与知识点、内容/讲解范围、来源、用户反馈、可支持的掌握证据和可接续的问题。\n\n讲过、表示理解、能解释、能应用和存在局部误解分别记录。未知保持未知，不根据身份、沉默或一次错误判断整体能力。\n".into()
}
