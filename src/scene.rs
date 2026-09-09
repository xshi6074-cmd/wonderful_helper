//! 协作场景（playbook）：**判断段的产物，不是 harness 的指令。**
//!
//! # 这个文件是上一版最大的设计错误被改掉的地方
//!
//! 上一版有一个 `Action` 枚举，判断段吐一个变体，harness 照着执行：
//! `AskUser` 就掐断回答段把问题交回用户、`Block` 就拒绝推进、`Enumerate` 就强制列 N 条。
//! 那是拿状态机替模型做流程决策 —— 模型只负责填了个枚举值。
//!
//! 正确的类比是编码 agent：它不会在 harness 里写死「实现代码前必须读三个仓库」，
//! 它把「读仓库」**作为工具暴露**给模型，并在 prompt 里说明什么时候建议读。
//! 读几个、读哪个、要不要读，是模型的工作。
//!
//! 所以现在判断段的产物是一个 [`SceneId`]：一次**场景判定**。harness 拿它做三件事，
//! 全都是「准备材料」，没有一件是「代替模型决定流程」：
//!
//! 1. 把 [`Scene::guidance`]（约束与建议）注入回答段的 prompt；
//! 2. 把 [`Scene::tools`] 列出的工具**暴露**给模型 —— 用不用、用几次、什么顺序由模型定；
//! 3. 把命中这个场景的参考案例检索出来一并注入（案例在自己的文件里声明服务于哪些场景，
//!    见 [`crate::memory::Case`]）。
//!
//! harness 之后不再干预回答段。模型想提问就自己调 `ask_user`，想读仓库就自己调 `fs_read`，
//! 想把读到的落成推断就自己调 `record_graph`，也可以什么都不调直接回答。
//!
//! # 这里写的工具名必须真的存在
//!
//! 上一版这份内置目录里写的是 `read_repo` / `repo_qa` / `search_cases` ——
//! **注册表里一个都没有**，而 `Registry::specs` 当时是 `filter_map` 静默丢弃。
//! 结果整套 `fs_*` / `repo_tree` / `web_*` 从来没被暴露给模型过，
//! 指标上表现为 `tools_run: 0`，看起来像模型不爱调工具。
//! 现在认不出的名字会报到界面上，见 [`crate::tools::Exposed::missing`]。
//!
//! # 状态机负责什么
//!
//! 只负责两件事：**状态的稳定维持**（写权仲裁、事务边界、崩溃恢复）和
//! **模型与 workspace 之间的稳定接口**（快照 / 提交 / 工具调用）。
//! 不负责「这一步该做什么」—— 那是模型的工作。
//!
//! # 可插拔
//!
//! 场景是纯文本配置。L1 = 往 playbook 里加一条；L2 = 换整份 playbook。
//! 两层都不用改 Rust。改 Rust 的只有加一种新工具（L3），文档里要明说。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type SceneId = String;

/// 一个协作场景。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub id: SceneId,
    pub label: String,
    /// 什么时候判成这个场景。**只有这一行进判断段的目录**，
    /// 判断段看的是目录级信息，不需要读完整的 guidance。
    pub when: String,
    /// 注入回答段的约束与建议。
    ///
    /// 措辞上必须是**给模型的建议和约束**，不是给 harness 的指令。
    /// 「建议先把这条链路讲通再往下」是对的；「必须先调用 read_repo 三次」是错的
    /// —— 后者要么该写成工具的 description，要么根本不该存在。
    pub guidance: String,
    /// 这个场景下**暴露**给模型的工具名。模型自主决定是否调用。
    ///
    /// 空表示只给默认工具集。注意这是「暴露」不是「安排」：
    /// 列了 `ask_user` 只意味着这个场景下模型可以用选择题的形式提问，
    /// 不意味着它这一轮一定会问。
    #[serde(default)]
    pub tools: Vec<String>,
}

/// `playbook.toml` 的磁盘形态：场景是一个数组，读进来才转成按 id 索引的表。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaybookFile {
    #[serde(default)]
    default_tools: Vec<String>,
    #[serde(default)]
    scene: Vec<Scene>,
}

/// 蒸馏草稿里的场景级 diff。它故意没有 default_tools，防止模型借更新场景
/// 顺手改掉整库的能力边界。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaybookPatch {
    #[serde(default)]
    scene: Vec<Scene>,
}

/// 场景目录。可插拔的纯文本配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playbook {
    pub scenes: BTreeMap<SceneId, Scene>,
    /// 所有场景都能用的工具。
    pub default_tools: Vec<String>,
}

impl Playbook {
    pub fn get(&self, id: &str) -> Option<&Scene> {
        self.scenes.get(id)
    }

    /// 判断段看到的目录：只有 id + when，不含 guidance。
    ///
    /// 判断段要选的是「进哪个场景」，看 when 就够了。八份 guidance 全塞进去，
    /// 判断段的 prompt 就跟回答段一样重，两段式白分。guidance 只在场景选定之后
    /// 注入回答段，见 [`crate::context::Context::for_answer`]。
    pub fn catalog(&self) -> String {
        self.scenes
            .values()
            .map(|s| format!("- {} · {}：{}", s.id, s.label, s.when))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 本轮模型能看到的全部工具名：默认 + **这一组**场景各自要的 + 当前 mode 额外给的。
    ///
    /// mode 也参与，是因为不同阶段可能开放不同的辅助工具。三份来源合并去重，
    /// 顺序按「默认 → 场景（按传入序）→ mode」，
    /// 模型看到的工具列表因此是稳定的。
    pub fn exposed_tools(&self, scenes: &[Scene], mode_extra: &[String]) -> Vec<String> {
        let mut v = self.default_tools.clone();
        let from_scenes = scenes.iter().flat_map(|s| s.tools.iter());
        for t in from_scenes.chain(mode_extra.iter()) {
            if !v.contains(t) {
                v.push(t.clone());
            }
        }
        v
    }

    /// 把一组 id 解析成场景，认不出的丢掉并回报。
    /// 顺序保留 judge 给出的影响优先级；不能让 BTreeMap 的字母序冒充重要性。
    pub fn resolve(&self, ids: &[SceneId]) -> (Vec<Scene>, Vec<SceneId>) {
        let mut unknown = Vec::new();
        let mut out = Vec::new();
        for id in ids {
            if let Some(scene) = self.scenes.get(id) {
                if !out.iter().any(|seen: &Scene| seen.id == scene.id) {
                    out.push(scene.clone());
                }
            } else if !unknown.contains(id) {
                unknown.push(id.clone());
            }
        }
        (out, unknown)
    }

    /// 从 TOML 加载。`playbook.toml` 是 L2 可插拔的入口，也是用户直接编辑的文件。
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let raw: PlaybookFile = toml::from_str(s).map_err(|e| e.to_string())?;
        let mut scenes = BTreeMap::new();
        for mut sc in raw.scene {
            let id = sc.id.trim().to_string();
            if id.is_empty() {
                return Err("场景 id 不能为空".into());
            }
            if sc.label.trim().is_empty() || sc.when.trim().is_empty() {
                return Err(format!("场景 {id} 的 label/when 不能为空"));
            }
            sc.id = id.clone();
            if scenes.insert(id.clone(), sc).is_some() {
                return Err(format!("场景 id = {id:?} 重复"));
            }
        }
        if !scenes.contains_key("none") {
            return Err("playbook 必须包含 id = \"none\" 的场景（无特殊处理时的回退）".into());
        }
        Ok(Playbook { scenes, default_tools: raw.default_tools })
    }

    /// 写回 TOML。bootstrap 时用它把内置目录落成用户可编辑的文件。
    pub fn to_toml_string(&self) -> String {
        let f = PlaybookFile {
            default_tools: self.default_tools.clone(),
            scene: self.scenes.values().cloned().collect(),
        };
        let body = toml::to_string_pretty(&f).unwrap_or_default();
        let header = concat!(
            "# 易犯错场景与对应指令。这个文件属于你，随时可以改。
",
            "# guidance 写给模型看，措辞应是建议与约束，不是给程序的指令。
",
            "# tools 只决定这个场景下模型能看见哪些工具，不决定它会不会用。
",
            "# 必须保留一个 id = \"none\" 的场景作为回退。

",
        );
        format!("{header}{body}")
    }

    /// 把只含新增/更新 `[[scene]]` 块的草稿按 id 合并到当前场景库。
    /// 未出现在 diff 里的场景和 default_tools 都保持原样。
    pub fn merge_toml(current: &str, delta: &str) -> Result<String, String> {
        let mut base = Self::from_toml_str(current)?;
        let patch: PlaybookPatch = toml::from_str(delta)
            .map_err(|e| format!("场景 diff 不是合法 TOML：{e}"))?;
        if patch.scene.is_empty() {
            return Err("场景 diff 里没有 [[scene]] 块".into());
        }
        let mut seen = Vec::new();
        for mut scene in patch.scene {
            let id = scene.id.trim().to_string();
            if id.is_empty() {
                return Err("场景 id 不能为空".into());
            }
            if seen.iter().any(|known: &String| known == &id) {
                return Err(format!("场景 diff 里 id = {id:?} 重复"));
            }
            if scene.label.trim().is_empty() || scene.when.trim().is_empty() {
                return Err(format!("场景 {id} 的 label/when 不能为空"));
            }
            scene.id = id.clone();
            seen.push(id.clone());
            base.scenes.insert(id, scene);
        }
        Ok(base.to_toml_string())
    }

    /// 仓库里的 `memory/playbook.toml` 同时是项目默认场景库和冷启动模板。
    /// 单一来源可防止删了磁盘场景却仍从 Rust 内置版本回落出来。
    pub fn builtin() -> Self {
        Self::from_toml_str(include_str!("../memory/playbook.toml"))
            .expect("内置 memory/playbook.toml 必须是合法场景库")
    }
}
