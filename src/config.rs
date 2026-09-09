//! 模型配置：**默认 → JSON 文件 → 环境变量**，三层覆盖，来源可查。
//!
//! # 为什么要记「这个值是从哪来的」
//!
//! 配置出错时最难查的不是「值不对」，是「我明明改了文件为什么没生效」。
//! 所以每个字段都带一条 [`From`] 记录，[`Settings::describe`] 一次全打出来 ——
//! 这是配置层唯一真正重要的可观测性。
//!
//! # 密钥不进 `config.json`
//!
//! 两个文件，分工明确：
//!
//! - `config.json`：模型花名册 + 工具策略。用户会读它、diff 它、贴给别人看，
//!   所以它**必须永远不含密钥** —— 不靠脱敏，靠这里根本没有那个字段。
//! - `secrets.json`：只有密钥。0600、写进 `.gitignore`、[`Settings::describe`]
//!   里只显示「有 / 没有」。
//!
//! 「填入并存入」走 [`Secrets::put`] + [`Secrets::save`]，UI 那边就是一个输入框。
//! **环境变量优先级最高**，这样 CI 和临时调试不用碰文件。
//!
//! # 环境变量不直接读
//!
//! [`Settings::load`] 收一个 `env` 闭包而不是自己调 `std::env::var`。
//! 一半是为了可测（edition 2024 里 `set_var` 是 unsafe，测试根本不该去改进程环境），
//! 一半是为了将来能从别处喂配置（比如 UI 的临时覆盖）而不用改这个文件。

use crate::model::Role;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.json";
pub const SECRETS_FILE: &str = "secrets.json";

/// 一个值是从哪一层来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Src {
    /// 代码里的内置默认
    Default,
    /// `config.json`
    File,
    /// 环境变量
    Env,
}

impl Src {
    pub fn label(&self) -> &'static str {
        match self {
            Src::Default => "默认",
            Src::File => "config.json",
            Src::Env => "环境变量",
        }
    }
}

/// provider 的协议族。决定请求怎么拼 —— 与具体模型无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    Anthropic,
    /// OpenAI 兼容的 `/chat/completions`。vLLM、together、deepseek、本地
    /// llama.cpp 都是这一族，所以它不叫 `OpenAi`。
    OpenAiCompat,
}


/// 一个内置的 provider 预设。
///
/// # 为什么放在 Rust 侧而不是前端
///
/// base_url 和密钥变量名是**同一份事实**：`Settings::default()` 要用，UI 的
/// 「添加 provider」也要用。抄两份迟早对不上，而症状是「照着界面填完连不上」。
///
/// `models` 只是**拉不到时的兜底**。真正的候选来自 `GET {base}/models`
/// （见 server 的 `models_probe`）—— 写死的列表一定会过期：这份里原来写的
/// `moonshot-v1-8k` 在 2026-08-31 下线、`deepseek-chat` 在 2026-07-24 废弃、
/// `glm-4-plus` 更早，而界面还在把它们当候选推给用户。
/// 列表里的东西是查证时点的快照，不是事实。
pub struct Preset {
    pub name: &'static str,
    pub label: &'static str,
    pub api: Api,
    pub base_url: &'static str,
    pub key_env: &'static str,
    /// 建这个 provider 时写进 config.json 的调用方式**种子**。
    ///
    /// # 它只是个初值，不是运行时读的表
    ///
    /// 一旦写进 `config.json`，之后所有请求读的都是那份配置 ——
    /// 用户随时能手改，协商撞墙时也会就地改它。代码里这份只在
    /// 「第一次把这家加进花名册」时用一次。
    ///
    /// 值来自各家文档（2026-09 查）。查不到的按 OpenAI 兼容的通例给，
    /// 错了协商会当场纠正 —— 所以这里宁可给最严的一档。
    pub caps: crate::caps::Caps,
    pub models: &'static [&'static str],
}

/// 内置的几家。**是函数不是 const**：种子里的 `Caps` 带 String 字段，
/// const 上下文构造不出来。反正它只在「加进花名册」时调一次。
pub fn presets() -> Vec<Preset> {
    vec![
    Preset {
        name: "anthropic", label: "Anthropic", api: Api::Anthropic,
        base_url: "https://api.anthropic.com", key_env: "ANTHROPIC_API_KEY",
        models: &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5-20251001"],
            // Anthropic：tool_choice = auto/any/tool/none；temperature 0–1。
            // **开着 thinking 时不能指名某个工具**，所以判断段这边先关 thinking、
            // 用 any（不点名）—— 两条限制同时避开。
            caps: crate::caps::Caps {
                thinking: crate::caps::Thinking::Disabled,
                forced: crate::caps::Forced::Any,
                temperature: true,
                temp_max: Some(1.0),
                max_tokens_field: "max_tokens".into(),
                stream_usage: true,
            },
    },
    Preset {
        name: "openai", label: "OpenAI", api: Api::OpenAiCompat,
        base_url: "https://api.openai.com/v1", key_env: "OPENAI_API_KEY",
        models: &["gpt-4o", "gpt-4o-mini", "o3-mini"],
            // OpenAI：tool_choice = none/auto/required/命名；temperature 0–2。
            // 没有 thinking 字段，发了会被当成未知参数，所以不碰。
            caps: crate::caps::Caps {
                thinking: crate::caps::Thinking::Untouched,
                forced: crate::caps::Forced::Any,
                temperature: true,
                temp_max: Some(2.0),
                max_tokens_field: "max_tokens".into(),
                stream_usage: true,
            },
    },
    Preset {
        name: "deepseek", label: "DeepSeek", api: Api::OpenAiCompat,
        base_url: "https://api.deepseek.com/v1", key_env: "DEEPSEEK_API_KEY",
        // deepseek-chat / deepseek-reasoner 已于 2026-07-24 废弃
        models: &["deepseek-v4-flash", "deepseek-v4-pro"],
            // DeepSeek：四种 tool_choice 全支持；temperature 0–2、默认 1；
            // thinking = {type: enabled|disabled}，默认 enabled。
            caps: crate::caps::Caps {
                thinking: crate::caps::Thinking::Disabled,
                forced: crate::caps::Forced::Any,
                temperature: true,
                temp_max: Some(2.0),
                max_tokens_field: "max_tokens".into(),
                stream_usage: true,
            },
    },
    Preset {
        name: "zhipu", label: "智谱 GLM", api: Api::OpenAiCompat,
        base_url: "https://open.bigmodel.cn/api/paas/v4", key_env: "ZHIPUAI_API_KEY",
        models: &["glm-5.3", "glm-5.3-flash", "glm-5.2", "glm-4.7", "glm-4.6"],
            // 智谱：temperature 0–1、默认 1；thinking = {type: enabled|disabled}，
            // 但 GLM-5.3 起不允许 disabled（400 / 1210），得改成
            // enabled + reasoning_effort=low —— 协商会自动走那一档。
            // tool_choice 的取值我没拿到官方原文，按 OpenAI 兼容通例给 required。
            caps: crate::caps::Caps {
                thinking: crate::caps::Thinking::Disabled,
                forced: crate::caps::Forced::Any,
                temperature: true,
                temp_max: Some(1.0),
                max_tokens_field: "max_tokens".into(),
                stream_usage: true,
            },
    },
    Preset {
        name: "moonshot", label: "月之暗面 Kimi", api: Api::OpenAiCompat,
        base_url: "https://api.moonshot.cn/v1", key_env: "MOONSHOT_API_KEY",
        // moonshot-v1 系列与 kimi-k2.5 已于 2026-08-31 下线
        models: &["kimi-k3", "kimi-k2.6", "kimi-k2.7-code"],
            // Moonshot：tool_choice 四种全支持；temperature 上限 1（不是 OpenAI 的 2）；
            // thinking = {type: enabled|disabled}，但 k2.7-code 关不掉 ——
            // 那种型号上协商会退到 enabled + reasoning_effort。
            caps: crate::caps::Caps {
                thinking: crate::caps::Thinking::Disabled,
                forced: crate::caps::Forced::Any,
                temperature: true,
                temp_max: Some(1.0),
                max_tokens_field: "max_tokens".into(),
                stream_usage: true,
            },
    },
    Preset {
        // 自建 / vLLM / Ollama / LM Studio 都走这条。地址是最常见的那个默认端口。
        name: "custom", label: "自定义（OpenAI 兼容）", api: Api::OpenAiCompat,
        base_url: "http://localhost:8000/v1", key_env: "CUSTOM_API_KEY",
        models: &[],
            // 自建：什么都不知道，给最严的一档，让协商去试。
            caps: crate::caps::Caps {
                thinking: crate::caps::Thinking::Untouched,
                forced: crate::caps::Forced::Any,
                temperature: true,
                temp_max: None,
                max_tokens_field: "max_tokens".into(),
                stream_usage: true,
            },
    },
    ]
}

impl Preset {
    pub fn cfg(&self) -> ProviderCfg {
        ProviderCfg {
            api: self.api,
            base_url: self.base_url.into(),
            key_env: self.key_env.into(),
            // 种子写进去。**它落进 config.json 之后就是普通配置项** ——
            // 用户改得动，协商也会就地改它。代码里这份只用这一次。
            caps: Some(self.caps.clone()),
        }
    }
}

/// 给 UI 的预设清单。
pub fn presets_json() -> serde_json::Value {
    serde_json::Value::Array(
        presets()
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name, "label": p.label,
                    "api": match p.api { Api::Anthropic => "anthropic", Api::OpenAiCompat => "open_ai_compat" },
                    "base_url": p.base_url, "key_env": p.key_env, "models": p.models,
                    "caps": serde_json::to_value(&p.caps).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect(),
    )
}

/// 一个模型服务方。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCfg {
    pub api: Api,
    pub base_url: String,
    /// 去哪个环境变量里找密钥。**这里只写变量名，永远不写密钥本身。**
    pub key_env: String,
    /// 实测出来的调用方式。第一次撞墙时协商出来，之后直接用。
    ///
    /// 放在 config.json 里而不是一个隐藏缓存：用户看得见、改得动，
    /// 换了模型把这一段删掉就会重新协商一次。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<crate::caps::Caps>,
}

/// 一个角色用什么模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCfg {
    /// [`Settings::providers`] 里的键。
    pub provider: String,
    pub model: String,
    /// 采样温度。**`None` = 一个字都不发，用服务端自己的默认。**
    ///
    /// 不给默认值是有意的：各家的取值范围不一样（Moonshot / GLM 是 [0,1]，
    /// OpenAI 是 [0,2]），deepseek-reasoner 干脆不收这个参数。我随手填一个
    /// 「看起来合理」的数，就是替用户在一个他没同意过的取向上做了决定 ——
    /// 而这类默认值错了不会报错，只会让输出悄悄偏掉。
    ///
    /// 老的 config.json 里写着一个数字，照样解析成 `Some(n)`，不用迁移。
    #[serde(default)]
    pub temperature: Option<f32>,
    pub max_tokens: u32,
}

/// 三个角色各自的配置。
///
/// 分开配是有意的：判断段每轮都跑但不需要强模型，回答段需要，subagent 吃大块
/// 上下文。合成一个就没法「便宜模型跑判断段」，而那是这套两段式循环能便宜的前提。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roles {
    pub judge: ModelCfg,
    pub answer: ModelCfg,
    pub subagent: ModelCfg,
}

impl Roles {
    pub fn of(&self, r: Role) -> &ModelCfg {
        match r {
            Role::Judge => &self.judge,
            Role::Answer => &self.answer,
            Role::Subagent => &self.subagent,
        }
    }

    fn of_mut(&mut self, r: Role) -> &mut ModelCfg {
        match r {
            Role::Judge => &mut self.judge,
            Role::Answer => &mut self.answer,
            Role::Subagent => &mut self.subagent,
        }
    }
}

pub const ROLES: [Role; 3] = [Role::Judge, Role::Answer, Role::Subagent];

fn role_key(r: Role) -> &'static str {
    match r {
        Role::Judge => "JUDGE",
        Role::Answer => "ANSWER",
        Role::Subagent => "SUBAGENT",
    }
}

/// 落盘的那份配置。**不含密钥。**
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub providers: BTreeMap<String, ProviderCfg>,
    pub roles: Roles,
    /// 工具的权限与体量上限。和模型配置放同一个文件，因为用户改它们的时机是一样的。
    #[serde(default)]
    pub tools: crate::policy::PolicyCfg,
    /// 旧版联网后端配置。当前运行统一使用内置 fetch，字段只为兼容已有配置。
    #[serde(default)]
    pub web: crate::web::WebCfg,
    /// 每个字段的来源。**不落盘** —— 它描述的是「这一次是怎么加载的」。
    #[serde(skip)]
    pub origins: Vec<(String, Src)>,
    /// 加载中遇到的问题。像界面报错一样报出来，不能只 eprintln。
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        // 花名册直接从预设生成。`custom` 不进默认 —— 它是「添加 provider」
        // 时的模板，摆在默认里只会多一条永远连不上的条目。
        let providers: BTreeMap<String, ProviderCfg> = presets()
            .iter()
            .filter(|p| p.name != "custom")
            .map(|p| (p.name.to_string(), p.cfg()))
            .collect();
        Settings {
            providers,
            roles: Roles {
                // 判断段每轮都跑、只做场景判定与图更新，配便宜的那档
                judge: ModelCfg {
                    provider: "anthropic".into(),
                    model: "claude-haiku-4-5-20251001".into(),
                    temperature: None,
                    max_tokens: 2048,
                },
                answer: ModelCfg {
                    provider: "anthropic".into(),
                    model: "claude-opus-5".into(),
                    temperature: None,
                    max_tokens: 8192,
                },
                // subagent 吃仓库/论文这类大块上下文，要能力也要便宜
                subagent: ModelCfg {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-5".into(),
                    temperature: None,
                    max_tokens: 8192,
                },
            },
            tools: crate::policy::PolicyCfg::default(),
            web: crate::web::WebCfg::default(),
            origins: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// 取环境变量的方式。注进来而不是直接 `std::env::var`，见模块头。
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// 真环境。生产路径用它。
pub fn real_env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

impl Settings {
    /// 三层加载：内置默认 → `config.json` → 环境变量。
    ///
    /// 文件坏了**不致命**：回退到默认并记一条 warning。配置文件写错一个逗号就
    /// 起不来，对一个用户随时手改的文件来说太脆。
    pub fn load(dir: &Path, env: Env<'_>) -> Settings {
        let mut s = Settings::default();
        let mut origins: Vec<(String, Src)> = Vec::new();

        let path = dir.join(CONFIG_FILE);
        let mut from_file = false;
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(parsed) => {
                    s.providers = parsed.providers;
                    s.roles = parsed.roles;
                    s.tools = parsed.tools;
                    s.web = parsed.web;
                    from_file = true;
                }
                Err(e) => s.warnings.push(format!("{CONFIG_FILE} 解析失败，已回退到默认：{e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => s.warnings.push(format!("{CONFIG_FILE} 读不了，已回退到默认：{e}")),
        }
        let base = if from_file { Src::File } else { Src::Default };

        // ── 环境变量覆盖 ──
        for r in ROLES {
            let k = role_key(r);
            let mut src = base;
            if let Some(v) = env(&format!("PREMORTEM_{k}_MODEL")) {
                s.roles.of_mut(r).model = v;
                src = Src::Env;
            }
            if let Some(v) = env(&format!("PREMORTEM_{k}_PROVIDER")) {
                s.roles.of_mut(r).provider = v;
                src = Src::Env;
            }
            if let Some(v) = env(&format!("PREMORTEM_{k}_MAX_TOKENS")) {
                match v.parse() {
                    Ok(n) => {
                        s.roles.of_mut(r).max_tokens = n;
                        src = Src::Env;
                    }
                    Err(_) => s
                        .warnings
                        .push(format!("PREMORTEM_{k}_MAX_TOKENS 不是数字，忽略：{v}")),
                }
            }
            if let Some(v) = env(&format!("PREMORTEM_{k}_TEMPERATURE")) {
                match v.parse() {
                    Ok(n) => {
                        s.roles.of_mut(r).temperature = Some(n);
                        src = Src::Env;
                    }
                    Err(_) => s
                        .warnings
                        .push(format!("PREMORTEM_{k}_TEMPERATURE 不是数字，忽略：{v}")),
                }
            }
            origins.push((format!("roles.{}", k.to_lowercase()), src));
        }

        let names: Vec<String> = s.providers.keys().cloned().collect();
        for name in names {
            let mut src = base;
            if let Some(v) = env(&format!("PREMORTEM_{}_BASE_URL", name.to_uppercase())) {
                if let Some(p) = s.providers.get_mut(&name) {
                    p.base_url = v;
                }
                src = Src::Env;
            }
            origins.push((format!("providers.{name}"), src));
        }

        // 引用了不存在的 provider 是个必须报出来的错：不报的话症状是运行时
        // 「模型没反应」，用户会去查网络、查密钥，查不到这里。
        for r in ROLES {
            let want = &s.roles.of(r).provider;
            if !s.providers.contains_key(want) {
                s.warnings.push(format!(
                    "角色 {} 指定的 provider「{want}」没有定义，可用的是：{}",
                    role_key(r).to_lowercase(),
                    s.providers.keys().cloned().collect::<Vec<_>>().join(" / ")
                ));
            }
        }

        s.origins = origins;
        s
    }

    /// 写回 `config.json`。**先写临时文件再 rename** —— 用户编辑器可能正开着它。
    /// **只改指定的那几个字段**，别的原样留在盘上。
    ///
    /// # 为什么不能「读出来 → 改 → 整份写回」
    ///
    /// 两个原因，都实际发生过：
    ///
    /// 1. **并发覆盖。** 界面上每一处保存都写整份文件时，浏览器手上那份
    ///    是上一次 boot 的快照 —— 用户在配置页改了模型、再去输入区点一下
    ///    「应用目录」，roles 就被快照里的旧值盖回去；反过来点保存，
    ///    `tools.roots` 又被盖回去。谁后点谁赢，另一边默默丢掉。
    /// 2. **解析失败会变成清空。** [`Settings::load`] 读不动文件时回退到默认值
    ///    并只留一条 warning；这时候再 `save()`，就把默认值（roles 全指向
    ///    anthropic、roots 变成 `.`）**写到了用户的文件上**。
    ///
    /// 所以这里走**原始 JSON**：读进来是什么就是什么，只动点名的那几个键，
    /// 认不出的字段原样保留。写之前反序列化一遍做校验 —— 改坏了当场拒绝，
    /// 而不是等下一次启动时回退到默认。
    ///
    /// `changes` 的键是点分路径，如 `roles.judge.provider`、`tools.roots`。
    /// 数字段名当数组下标用（`tools.roots.0`）。
    pub fn patch_file(
        dir: &Path,
        changes: &[(String, serde_json::Value)],
    ) -> Result<(), String> {
        let path = dir.join(CONFIG_FILE);
        let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| format!("{CONFIG_FILE} 现在就读不出来，没敢动它：{e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                serde_json::to_value(Settings::default()).map_err(|e| e.to_string())?
            }
            Err(e) => return Err(format!("{CONFIG_FILE} 读不了：{e}")),
        };
        for (k, v) in changes {
            put_path(&mut root, k, v.clone())?;
        }
        // 自己写出来的东西自己先读一遍。改坏了在这里拒绝，
        // 而不是留给下一次启动去「回退到默认」。
        serde_json::from_value::<Settings>(root.clone())
            .map_err(|e| format!("改完之后不是合法配置，没保存：{e}"))?;
        let text =
            serde_json::to_string_pretty(&root).map_err(|e| format!("序列化失败：{e}"))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
        atomic_write(&path, text.as_bytes(), false).map_err(|e| format!("写入失败：{e}"))
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        atomic_write(&dir.join(CONFIG_FILE), text.as_bytes(), false)
    }

    /// 把当前生效的配置连同来源一起打出来。
    ///
    /// 密钥只报「有 / 没有」和它来自哪 —— 值一个字都不出现，包括前几位。
    pub fn describe(&self, secrets: &Secrets, env: Env<'_>) -> String {
        let src_of = |k: &str| {
            self.origins
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, s)| s.label())
                .unwrap_or("默认")
        };
        let mut out = String::from("== 模型配置 ==\n");
        for r in ROLES {
            let key = role_key(r).to_lowercase();
            let m = self.roles.of(r);
            out.push_str(&format!(
                "  {:9} {} / {}  (temp {}, max {})  ← {}\n",
                key,
                m.provider,
                m.model,
                match m.temperature {
                    Some(t) => format!("{t:.1}"),
                    None => "服务端默认".into(),
                },
                m.max_tokens,
                src_of(&format!("roles.{key}"))
            ));
        }
        out.push_str("== provider ==\n");
        for (name, p) in &self.providers {
            // 注意别照搬 Src::label()：Src::File 对配置项来说是 config.json，
            // 但密钥在 secrets.json 里 —— 标错了会让人去翻错的文件。
            let (has, from) = match secrets.resolve(name, p, env) {
                Some((_, Src::Env)) => ("有", "环境变量"),
                Some(_) => ("有", SECRETS_FILE),
                None => ("缺", "—"),
            };
            out.push_str(&format!(
                "  {:9} {:?} {}  密钥{}（{}，变量名 {}）  ← {}\n",
                name,
                p.api,
                p.base_url,
                has,
                from,
                p.key_env,
                src_of(&format!("providers.{name}"))
            ));
        }
        if !self.warnings.is_empty() {
            out.push_str("== 配置告警 ==\n");
            for w in &self.warnings {
                out.push_str(&format!("  ! {w}\n"));
            }
        }
        out
    }

    /// 按配置搭出三个角色的真实客户端。**这是「填了 key 就能跑」的落点。**
    ///
    /// 任一角色缺密钥就整体失败并说清缺哪个、该设哪个变量 —— 不做部分降级：
    /// 三个角色里少一个，跑起来的症状是某一段莫名其妙不工作，比启动时报错难查得多。
    pub fn build_models(
        &self,
        secrets: &Secrets,
        env: Env<'_>,
    ) -> Result<crate::model::Models, String> {
        self.build_models_with(secrets, env, None)
    }

    /// 同上，外加一个协商结果的出口：客户端试出新的调用方式时往里发一条，
    /// 由 server 写回 config.json。传 `None` 就是不落盘（测试用）。
    pub fn build_models_with(
        &self,
        secrets: &Secrets,
        env: Env<'_>,
        sink: Option<crate::client::CapsSink>,
    ) -> Result<crate::model::Models, String> {
        let mk = |r: Role| -> Result<std::sync::Arc<dyn crate::model::ModelClient>, String> {
            let m = self.roles.of(r);
            let p = self.providers.get(&m.provider).ok_or_else(|| {
                format!("角色 {} 指定的 provider「{}」没有定义", role_key(r).to_lowercase(), m.provider)
            })?;
            let (key, _) = secrets.resolve(&m.provider, p, env).ok_or_else(|| {
                format!(
                    "provider「{}」缺密钥：设环境变量 {} 或写进 {}",
                    m.provider, p.key_env, SECRETS_FILE
                )
            })?;
            let role = match r {
                Role::Judge => "判断段",
                Role::Answer => "回答段",
                Role::Subagent => "子任务",
            };
            Ok(std::sync::Arc::new(crate::client::HttpClient::with_sink(
                p,
                m,
                key,
                role,
                sink.clone(),
            )?))
        };
        Ok(crate::model::Models {
            judge: mk(Role::Judge)?,
            answer: mk(Role::Answer)?,
            subagent: mk(Role::Subagent)?,
        })
    }

    /// firecrawl 的密钥。它不是模型 provider，所以单独走一条。
    /// 本地 crawl4ai 用不到它 —— 缺了就是 `None`，不是错误。
    pub fn web_key(&self, secrets: &Secrets, env: Env<'_>) -> Option<String> {
        env("FIRECRAWL_API_KEY")
            .filter(|v| !v.trim().is_empty())
            .or_else(|| secrets.get("firecrawl"))
    }

    /// 密钥齐不齐。缺的 provider 名单，供 UI 提示「去填一下」。
    pub fn missing_keys(&self, secrets: &Secrets, env: Env<'_>) -> Vec<String> {
        let mut used: Vec<&String> = ROLES.iter().map(|r| &self.roles.of(*r).provider).collect();
        used.sort();
        used.dedup();
        used.into_iter()
            .filter(|n| {
                self.providers
                    .get(*n)
                    .is_none_or(|p| secrets.resolve(n, p, env).is_none())
            })
            .cloned()
            .collect()
    }
}

/// 密钥。**单独一个文件，0600，不进 `config.json`，不进日志。**
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    /// provider 名 → 密钥。
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

impl Secrets {
    pub fn load(dir: &Path) -> (Secrets, Vec<String>) {
        let path = dir.join(SECRETS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(t) => match serde_json::from_str::<Secrets>(&t) {
                Ok(s) => (s, vec![]),
                // 这条一定要报：解析失败会静默地变成「所有密钥都没配」，
                // 而那个症状看起来完全像是密钥填错了。
                Err(e) => (Secrets::default(), vec![format!("{SECRETS_FILE} 解析失败：{e}")]),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Secrets::default(), vec![]),
            Err(e) => (Secrets::default(), vec![format!("{SECRETS_FILE} 读不了：{e}")]),
        }
    }

    /// 「填入」。空字符串等于删掉这一条。
    pub fn put(&mut self, provider: &str, key: &str) {
        if key.trim().is_empty() {
            self.keys.remove(provider);
        } else {
            self.keys.insert(provider.to_string(), key.trim().to_string());
        }
    }

    pub fn has(&self, provider: &str) -> bool {
        self.keys.contains_key(provider)
    }

    /// 按名字取原值。给没有 `ProviderCfg` 的东西用（比如 firecrawl 的密钥）。
    pub fn get(&self, name: &str) -> Option<String> {
        self.keys.get(name).cloned()
    }

    /// 「存入」。0600 + 原子替换。
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        atomic_write(&dir.join(SECRETS_FILE), text.as_bytes(), true)?;
        ensure_gitignored(dir)
    }

    /// 取密钥。**环境变量优先** —— CI 和临时调试不该被迫改文件。
    ///
    /// 返回值刻意带上来源：`describe` 要报「这个密钥是从哪来的」，
    /// 而「文件里有一个但环境变量盖住了」正是最容易查半天的那种情况。
    pub fn resolve(
        &self,
        name: &str,
        p: &ProviderCfg,
        env: Env<'_>,
    ) -> Option<(String, Src)> {
        if let Some(v) = env(&p.key_env).filter(|v| !v.trim().is_empty()) {
            return Some((v, Src::Env));
        }
        self.keys.get(name).map(|k| (k.clone(), Src::File))
    }
}

/// 按点分路径写一个值。中间缺的层自动补成对象；数字段名当数组下标。
///
/// 越界的下标当追加处理 —— `tools.roots.0` 在空数组上就是「加第一条」。
fn put_path(root: &mut serde_json::Value, path: &str, val: serde_json::Value) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err("空路径".into());
    }
    let mut cur = root;
    for (i, seg) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if let Ok(idx) = seg.parse::<usize>()
            && cur.is_array()
        {
            let arr = cur.as_array_mut().expect("刚判过是数组");
            if idx >= arr.len() {
                if last {
                    arr.push(val);
                    return Ok(());
                }
                arr.push(serde_json::json!({}));
                let n = arr.len() - 1;
                cur = &mut arr[n];
                continue;
            }
            if last {
                arr[idx] = val;
                return Ok(());
            }
            cur = &mut arr[idx];
            continue;
        }
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        let obj = cur.as_object_mut().expect("刚补成对象");
        if last {
            obj.insert((*seg).to_string(), val);
            return Ok(());
        }
        cur = obj.entry((*seg).to_string()).or_insert_with(|| serde_json::json!({}));
    }
    Ok(())
}

/// 先写 `.tmp` 再 rename。中途断电最多留下一个临时文件，不会留下半个配置。
fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    if private {
        set_private(&tmp)?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
fn set_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Windows 上没有等价的一行做法（要动 ACL）。**不假装做到了** ——
/// 报一句，让用户自己知道这个文件在这个平台上只有目录权限保护。
#[cfg(not(unix))]
fn set_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// 密钥文件必须进 `.gitignore`。**自动加，不指望用户记得。**
fn ensure_gitignored(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(".gitignore");
    let cur = std::fs::read_to_string(&path).unwrap_or_default();
    if cur.lines().any(|l| l.trim() == SECRETS_FILE) {
        return Ok(());
    }
    let mut next = cur;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(SECRETS_FILE);
    next.push('\n');
    std::fs::write(path, next)
}

/// 配置目录下的模型工作目录名。见 [`crate::scratch`]。
pub fn scratch_dir(root: &Path) -> PathBuf {
    root.join("workspace")
}
