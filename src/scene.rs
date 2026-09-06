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

    /// 这个场景下模型能看到的全部工具名：默认 + 场景专属 + **当前 mode 额外给的**。
    ///
    /// mode 也参与，是因为「探索期能开放搜索、行动期只按址抓」这类差别属于 mode
    /// 不属于场景。三份来源合并去重，顺序按「默认 → 场景 → mode」，
    /// 模型看到的工具列表因此是稳定的。
    pub fn exposed_tools(&self, scene: &Scene, mode_extra: &[String]) -> Vec<String> {
        let mut v = self.default_tools.clone();
        for t in scene.tools.iter().chain(mode_extra.iter()) {
            if !v.contains(t) {
                v.push(t.clone());
            }
        }
        v
    }

    /// 从 TOML 加载。`playbook.toml` 是 L2 可插拔的入口，也是用户直接编辑的文件。
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let raw: PlaybookFile = toml::from_str(s).map_err(|e| e.to_string())?;
        let mut scenes = BTreeMap::new();
        for sc in raw.scene {
            scenes.insert(sc.id.clone(), sc);
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

    /// 内置目录，对应设计文档第三节「2 · 协作动作」列的那几种。
    ///
    /// 这不是「默认值」而是 **bootstrap 案例**：验收方案里的「冷启动可用性」
    /// 要求没有个人积累时首次打开即可用。
    pub fn builtin() -> Self {
        let mut scenes = BTreeMap::new();
        let mut add = |id: &str, label: &str, when: &str, guidance: &str, tools: &[&str]| {
            scenes.insert(
                id.to_string(),
                Scene {
                    id: id.to_string(),
                    label: label.to_string(),
                    when: when.to_string(),
                    guidance: guidance.to_string(),
                    tools: tools.iter().map(|s| s.to_string()).collect(),
                },
            );
        };

        add(
            "none",
            "不做特殊干预",
            "用户的话是闲聊、确认、或已经很明确的具体请求",
            "正常回答。不要为了显得尽职而额外提问。",
            &[],
        );
        add(
            "clarify_goal",
            "澄清目标",
            "用户描述的 claim 模糊，或者要验证的东西和要改的东西对不上",
            "用户的目标还没落定。建议先把「要验证什么」问清楚再往下。\
             新手答不上开放式问题，如果要问，尽量带上候选项（用 ask_user 工具）。\
             不确定的部分不要替用户补全。",
            &["ask_user"],
        );
        add(
            "teach_background",
            "补充领域背景",
            "用户储备记录显示这个知识域他明确没有储备，或者他自陈不熟",
            "先把机制讲通，再把判断交回给用户。讲解要落到他这个仓库/这个实验上，\
             不要泛泛介绍概念。讲完可以问一句他是否要按这个理解继续。",
            &[],
        );
        add(
            "trace_code",
            "追踪代码链路",
            "用户要改某个模块，但对话里看不出他确认过这个模块的输出被谁消费",
            "建议先把相关的调用链路走一遍再谈改法 —— repo_tree 看结构、fs_grep 找引用、\
             fs_read 读具体位置。读到什么就用 record_graph 补到图上，source 填 \
             {\"repo\": \"路径:行号\"}。读不到的不要猜，要猜就标 source=\"guess\"（会画成虚线）。",
            &["fs_read", "fs_grep", "fs_find", "repo_tree"],
        );
        add(
            "check_assumption",
            "检查假设",
            "设计里有互相矛盾的字段，或者指标测不出 claim 说的那个东西",
            "指出你看到的矛盾，说清楚是哪两处对不上。措辞用「我看到这根线」而不是\
             「你这里有问题」—— 校对式，不是质问式。命中与否代价不对称。",
            &["ask_user"],
        );
        add(
            "diverge_design",
            "发散设计",
            "只有一个方案却要下结论，或者对照组明显不足以支撑 claim",
            "对照空间还没铺开。建议把可能的对照/消融列出来再收敛，\
             列的时候说明每一条能排除什么可能性。不用追求穷尽。\
             铺出来的对照臂用 record_graph 画成 ablation / baseline 节点，\
             用户要在图上直接删改的就是它们。",
            &[],
        );
        add(
            "cheap_first",
            "优先低成本试验",
            "算力/数据/时间的量级和方案对不上",
            "建议先做能最快证伪的那个最小实验。给出它要花多少、能排除什么。\
             如果预算根本不够，直接说清缺口，不要假装可行。",
            &[],
        );
        add(
            "stop_and_implement",
            "停止讨论进入实现",
            "该定的都定了，再讨论边际收益很低",
            "该收尾了。产出给下游编码 agent 的 brief：要验证的 claim / 要改的具体位置 / \
             必须保持不变的东西 / 对照清单 / 怎么算验收通过。\
             写 brief 之前先用 record_note 把验收口径落下来（open 里应该清空得差不多了）。\
             **是否真的进入实现由用户拍板，你只给 brief，不要替他宣布开始。**",
            &[],
        );

        Playbook {
            scenes,
            // 默认工具集：**这些名字必须在 Registry 里真的存在**。
            //
            // 两个动作工具永远在：模型能不能把推断写下来，不该是个可选项。
            // 读类文件工具也永远在：它们不依赖任何外部服务，跟场景无关。
            // 联网的两个不在这里 —— 它们按配置注册，由 mode 决定要不要给
            // （见 prompts.toml 的 [mode.*].tools）。
            default_tools: vec![
                "record_graph".into(),
                "record_note".into(),
                "fs_read".into(),
                "fs_grep".into(),
                "fs_find".into(),
                "repo_tree".into(),
            ],
        }
    }
}
