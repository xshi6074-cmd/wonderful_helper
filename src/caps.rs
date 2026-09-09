//! 调用方式的**协商**：先按最严的要求发，被拒了就换一种发法，试成的记下来。
//!
//! # 为什么不写一张能力表
//!
//! 写死「Moonshot 的 temperature ≤ 1、DeepSeek-reasoner 不收 tool_choice」这类表，
//! 两个月就过期一次。实测：GLM-5.3 上线之后 `thinking:{type:"disabled"}` 直接变成
//! 400（code 1210「本模型必须思考」），而在它之前的所有 GLM 上这是标准关法。
//! 我自己那份 PRESETS 的候选型号也已经过期了 —— 静态表就是会过期。
//!
//! 更糟的是**把表贴给用户看**：那等于让用户替程序记住每家的坑，然后自己绕开。
//! 那不是配置，那是让人给屎山做向下兼容。
//!
//! # 所以：正常路径一次调用，只有被拒才降级**发法**
//!
//! 关键区别 —— 降级的是**怎么发**，不是**要什么**。判断段要的始终是一个
//! schema 校验过的场景判定；如果一路试到底都拿不到工具调用，就明确报「这个模型
//! 当不了判断段」，**不假装判成 none**。悄悄降级的代价是：用户以为场景判定在工作，
//! 实际上每一轮都是空的，而这件事在界面上完全看不出来。
//!
//! # 试成的组合写进 config.json
//!
//! 落在用户看得见、改得动的地方，而不是一个隐藏缓存。下次直接用，不再协商；
//! 用户换了模型就把那段删掉，重新协商一次。
//!
//! # 编码
//!
//! 每一项能力有一个码（C1–C9），每一次尝试是一条 [`Step`]。失败时整份阶梯
//! 原样交给界面：需要什么、试了什么、厂商原话是什么、结论是什么。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 一项被协商的能力。**码是给人看的锚点**，日志、界面、文档用同一套。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cap {
    /// C1 基本对话
    Chat,
    /// C2 独立的 system 消息
    SystemRole,
    /// C3 `max_tokens` 的字段名
    MaxTokensField,
    /// C4 流式
    Stream,
    /// C5 流式末尾给 usage
    StreamUsage,
    /// C6 工具调用
    ToolUse,
    /// C7 **强制**工具调用
    ForcedTool,
    /// C8 关掉 thinking
    ThinkingOff,
    /// C9 temperature
    Temperature,
}

impl Cap {
    pub fn code(&self) -> &'static str {
        match self {
            Cap::Chat => "C1",
            Cap::SystemRole => "C2",
            Cap::MaxTokensField => "C3",
            Cap::Stream => "C4",
            Cap::StreamUsage => "C5",
            Cap::ToolUse => "C6",
            Cap::ForcedTool => "C7",
            Cap::ThinkingOff => "C8",
            Cap::Temperature => "C9",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Cap::Chat => "基本对话",
            Cap::SystemRole => "system 消息",
            Cap::MaxTokensField => "max_tokens 字段名",
            Cap::Stream => "流式",
            Cap::StreamUsage => "流式末尾给 usage",
            Cap::ToolUse => "工具调用",
            Cap::ForcedTool => "强制工具调用",
            Cap::ThinkingOff => "关闭 thinking",
            Cap::Temperature => "temperature",
        }
    }

    /// 这个程序为什么需要它。**失败日志的第一行就是这句** ——
    /// 用户要先知道「缺了它会少什么」，才谈得上决定换模型还是改配置。
    pub fn need(&self) -> &'static str {
        match self {
            Cap::Chat => "所有功能的前提。",
            Cap::SystemRole => "规则、持久层、场景目录、推断图都在 system 段里。",
            Cap::MaxTokensField => "不带长度上限的请求会被部分服务拒绝。",
            Cap::Stream => "回答段边写边显示靠它；不支持就只能等整段生成完。",
            Cap::StreamUsage => "拿不到就只能按字数估算，成本账目会漂。",
            Cap::ToolUse => "读文件、抓网页、写推断图都是工具调用。",
            Cap::ForcedTool => {
                "判断段靠它拿到 schema 校验过的场景判定。\
                 拿不到就没有场景判定 —— 而那意味着 guidance 与案例永远注不进回答段。"
            }
            Cap::ThinkingOff => {
                "thinking 开着时多数服务会拒绝强制工具调用（C7），\
                 而且会额外吐一段 reasoning_content。判断段不需要思考链。"
            }
            Cap::Temperature => "判断段要确定性（0），回答段要一点发散。各家取值范围不同。",
        }
    }
}

/// 一次尝试的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// 这么发行得通。
    Ok,
    /// 被拒了，换下一种发法。
    Rejected,
    /// 试完了所有发法。
    Exhausted,
}

/// 阶梯上的一级。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub cap: Cap,
    /// 这一次改成怎么发的，人话。
    pub tried: String,
    pub outcome: Outcome,
    /// 服务端**原话**。不要改写 —— 用户拿它去搜、去问客服，改写过就搜不到了。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// 这一次请求想不想要思考链。
///
/// **工具调用优先于 thinking。** 需要强制工具调用的请求（判断段）一律关思考：
/// 多数服务在 thinking 开着时会拒绝或悄悄把 `tool_choice` 降级成 auto，
/// 而判断段拿不到工具调用就等于没有场景判定。其余请求（回答段、折叠、蒸馏）
/// 让它好好想 —— 关思考是判断段的需要，不是全局策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Think {
    /// 为了拿到工具调用而关掉。关不掉的型号退到最低思考量。
    OffForTools,
    /// 开着。
    On,
}

/// thinking **能不能**关、怎么关。这是 provider 的属性，
/// 和「这一次想不想关」（[`Think`]）是两回事。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    /// 什么都不发，用服务端默认。
    Untouched,
    /// `thinking: {"type": "disabled"}`
    Disabled,
    /// `thinking: {"type":"enabled"}` + `reasoning_effort`。
    /// GLM-5.3 这类「必须思考」的模型只能走这条。
    Effort(String),
}

/// 强制工具调用怎么写。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Forced {
    /// OpenAI 兼容：`"required"`；Anthropic：`{"type":"any"}`
    Any,
    /// 指名那一个工具。有的服务只认这种。
    Named,
    /// 不发 tool_choice，只挂一个工具，靠 prompt 要求它调。
    /// **这是最后一档**，到这里已经不保证拿得到工具调用了。
    None,
}

/// 一个 provider 实测出来的调用方式。协商成功就写进 `config.json` 这个
/// provider 名下，之后直接用。用户换了模型就把它删掉，重新协商一次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    #[serde(default = "default_thinking")]
    pub thinking: Thinking,
    #[serde(default = "default_forced")]
    pub forced: Forced,
    /// 发不发 temperature。
    #[serde(default = "yes")]
    pub temperature: bool,
    /// 上限。超了就钳到这个值 —— Moonshot / GLM 是 1，OpenAI 是 2。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_max: Option<f32>,
    /// `max_tokens` 还是 `max_completion_tokens`。
    #[serde(default = "default_max_tokens_field")]
    pub max_tokens_field: String,
    /// 流式请不请求 usage。
    #[serde(default = "yes")]
    pub stream_usage: bool,
}

fn default_thinking() -> Thinking {
    Thinking::Disabled
}
fn default_forced() -> Forced {
    Forced::Any
}
fn yes() -> bool {
    true
}
fn default_max_tokens_field() -> String {
    "max_tokens".into()
}

impl Default for Caps {
    /// 最严的一档：关思考、强制工具调用、发 temperature。
    /// **先按这个发**，被拒了才往下走。
    fn default() -> Self {
        Caps {
            thinking: Thinking::Disabled,
            forced: Forced::Any,
            temperature: true,
            temp_max: None,
            max_tokens_field: "max_tokens".into(),
            stream_usage: true,
        }
    }
}

impl Caps {
    /// 把 temperature 钳进这家能收的范围。
    pub fn clamp(&self, t: f32) -> f32 {
        match self.temp_max {
            Some(m) if t > m => m,
            _ => t,
        }
    }

    /// 按错误原文挑下一种发法。`None` = 没招了。
    ///
    /// # 为什么按错误原文分派，而不是按型号
    ///
    /// 按型号就是那张会过期的表。错误原文是服务端**当场**告诉我们哪里不行，
    /// 新模型出来也一样能读懂。代价是要认几种措辞，认漏了就多试一档 ——
    /// 多试一档只是慢一点，认错型号是直接不能用。
    pub fn next(&mut self, err: &str) -> Option<Step> {
        let e = err.to_lowercase();
        let has = |k: &str| e.contains(k);

        // ── C8 thinking ──
        // 「这个模型必须思考」（GLM-5.3 的 1210 是这一类）
        if self.thinking == Thinking::Disabled
            && (has("cannot be disabled")
                || has("always engages in thinking")
                || has("1210")
                || (has("thinking") && has("disable")))
        {
            self.thinking = Thinking::Effort("low".into());
            return Some(Step {
                cap: Cap::ThinkingOff,
                tried: "关不掉 thinking，改成开启 + reasoning_effort=low".into(),
                outcome: Outcome::Rejected,
                detail: err.to_string(),
            });
        }
        // 服务端根本不认 thinking 这个字段
        if self.thinking != Thinking::Untouched
            && (has("unknown") || has("unrecognized") || has("unsupported") || has("extra"))
            && has("thinking")
        {
            self.thinking = Thinking::Untouched;
            return Some(Step {
                cap: Cap::ThinkingOff,
                tried: "服务端不认 thinking 字段，改成不发".into(),
                outcome: Outcome::Rejected,
                detail: err.to_string(),
            });
        }

        // ── C7 强制工具调用 ──
        if has("tool_choice") || (has("tool") && has("thinking")) {
            // thinking 还开着的话，先把它关掉再要强制 —— 多数服务的限制是
            // 「thinking 开着时不能指定工具」，不是「永远不能」。
            if self.thinking == Thinking::Effort("low".into()) || self.thinking == Thinking::Untouched
            {
                if self.thinking != Thinking::Disabled && self.forced == Forced::Any {
                    self.forced = Forced::Named;
                    return Some(Step {
                        cap: Cap::ForcedTool,
                        tried: "改成指名那一个工具".into(),
                        outcome: Outcome::Rejected,
                        detail: err.to_string(),
                    });
                }
            }
            match self.forced {
                Forced::Any => {
                    self.forced = Forced::Named;
                    return Some(Step {
                        cap: Cap::ForcedTool,
                        tried: "改成指名那一个工具".into(),
                        outcome: Outcome::Rejected,
                        detail: err.to_string(),
                    });
                }
                Forced::Named => {
                    self.forced = Forced::None;
                    return Some(Step {
                        cap: Cap::ForcedTool,
                        tried: "不发 tool_choice，只挂一个工具并在 prompt 里要求调用".into(),
                        outcome: Outcome::Rejected,
                        detail: err.to_string(),
                    });
                }
                Forced::None => {}
            }
        }

        // ── C9 temperature ──
        if has("temperature") {
            // 「范围是 0..1」这类：钳一下再试
            if self.temp_max.is_none()
                && (has("range") || has("between") || has("less than") || has("范围"))
            {
                self.temp_max = Some(1.0);
                return Some(Step {
                    cap: Cap::Temperature,
                    tried: "超出取值范围，钳到 1.0".into(),
                    outcome: Outcome::Rejected,
                    detail: err.to_string(),
                });
            }
            if self.temperature {
                self.temperature = false;
                return Some(Step {
                    cap: Cap::Temperature,
                    tried: "这个模型不收 temperature，改成不发".into(),
                    outcome: Outcome::Rejected,
                    detail: err.to_string(),
                });
            }
        }

        // ── C3 max_tokens 字段名 ──
        if has("max_tokens") && self.max_tokens_field == "max_tokens" {
            self.max_tokens_field = "max_completion_tokens".into();
            return Some(Step {
                cap: Cap::MaxTokensField,
                tried: "改用 max_completion_tokens".into(),
                outcome: Outcome::Rejected,
                detail: err.to_string(),
            });
        }

        // ── C5 流式 usage ──
        if has("stream_options") && self.stream_usage {
            self.stream_usage = false;
            return Some(Step {
                cap: Cap::StreamUsage,
                tried: "不要流式 usage（成本改为估算）".into(),
                outcome: Outcome::Rejected,
                detail: err.to_string(),
            });
        }

        None
    }

    /// 这套发法拼进请求体里的那几个字段。`anthropic` 决定 thinking 与
    /// tool_choice 的写法 —— 两家形状不同。
    pub fn apply(
        &self,
        body: &mut Value,
        anthropic: bool,
        temperature: Option<f32>,
        max_tokens: u32,
        want: Think,
    ) {
        let o = body.as_object_mut().expect("请求体是对象");
        o.insert(self.max_tokens_field.clone(), json!(max_tokens));
        // 两个条件都要满足才发：用户填了值，而且这个模型收这个参数。
        // 用户没填就一个字都不发 —— 那正是「用服务端默认」的意思。
        if let Some(t) = temperature.filter(|_| self.temperature) {
            o.insert("temperature".into(), json!(self.clamp(t)));
        }
        // 这家根本不认 thinking 字段 ⇒ 一个字都不发，不管这次想不想要。
        if self.thinking == Thinking::Untouched {
            let _ = anthropic;
            return;
        }
        match want {
            // 要强制工具调用 ⇒ 关思考。**工具调用优先级高于 thinking**：
            // 多数服务在 thinking 开着时会拒绝或悄悄降级 tool_choice，
            // 而判断段没有工具调用就等于没有场景判定 —— 那比少一段思考链贵得多。
            Think::OffForTools => match &self.thinking {
                // 这个型号关不掉（GLM-5.3 / kimi-k2.7-code）⇒ 退而求其次，
                // 开着但把思考量压到最低。
                Thinking::Effort(level) => {
                    o.insert("thinking".into(), json!({ "type": "enabled" }));
                    o.insert("reasoning_effort".into(), json!(level));
                }
                _ => {
                    o.insert("thinking".into(), json!({ "type": "disabled" }));
                }
            },
            // 不强制工具调用的请求 ⇒ 让它好好想。关思考是判断段的需要，
            // 不是全局策略。
            Think::On => {
                o.insert("thinking".into(), json!({ "type": "enabled" }));
            }
        }
    }

    /// 判断段的 `tool_choice`。`None` = 这一档不发这个字段。
    pub fn tool_choice(&self, anthropic: bool, tool: &str) -> Option<Value> {
        match self.forced {
            Forced::None => None,
            Forced::Any => Some(if anthropic { json!({ "type": "any" }) } else { json!("required") }),
            Forced::Named => Some(if anthropic {
                json!({ "type": "tool", "name": tool })
            } else {
                json!({ "type": "function", "function": { "name": tool } })
            }),
        }
    }
}

/// 一次协商的完整记录。**只有失败时才交给界面。**
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub provider: String,
    pub model: String,
    /// 哪个角色。判断段失败最要紧 —— 它一挂，场景判定整个失效。
    pub role: String,
    /// 这个角色**必须**有的能力。日志的第一段写它们。
    pub required: Vec<Cap>,
    pub steps: Vec<Step>,
    /// 一句话结论。
    pub verdict: String,
}

impl Report {
    /// 渲染成纯文本，给日志和「复制」按钮用。界面按结构自己排版。
    pub fn text(&self) -> String {
        let mut s = format!(
            "[{}] {} · {}\n\n这个角色需要：\n",
            self.role, self.provider, self.model
        );
        for c in &self.required {
            s.push_str(&format!("  {} {}　—　{}\n", c.code(), c.label(), c.need()));
        }
        s.push_str("\n试过：\n");
        for (i, st) in self.steps.iter().enumerate() {
            s.push_str(&format!(
                "  {}. [{}] {}　⇒ {}\n",
                i + 1,
                st.cap.code(),
                st.tried,
                match st.outcome {
                    Outcome::Ok => "成了",
                    Outcome::Rejected => "被拒",
                    Outcome::Exhausted => "没招了",
                }
            ));
            if !st.detail.is_empty() {
                s.push_str(&format!("     服务端原话：{}\n", st.detail.trim()));
            }
        }
        s.push_str(&format!("\n结论：{}\n", self.verdict));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GLM-5.3：关不掉 thinking，得改成 low effort。
    #[test]
    fn glm_53_cannot_disable_thinking() {
        let mut c = Caps::default();
        let s = c
            .next("code 1210: This model always engages in thinking and cannot be disabled; please use low, high, or max")
            .expect("要有下一档");
        assert_eq!(s.cap, Cap::ThinkingOff);
        assert_eq!(c.thinking, Thinking::Effort("low".into()));
        let mut body = json!({});
        c.apply(&mut body, false, Some(0.0), 100, Think::OffForTools);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "low");
    }

    /// Anthropic：thinking 开着不能指名工具。关掉之后 any 就能用。
    #[test]
    fn forced_tool_falls_back_by_shape_not_by_giving_up() {
        let mut c = Caps::default();
        assert_eq!(c.tool_choice(false, "t"), Some(json!("required")));
        let s = c
            .next("tool_choice 'specified' is incompatible with thinking enabled")
            .expect("要有下一档");
        assert_eq!(s.cap, Cap::ForcedTool);
        // 换成指名，而不是直接放弃强制
        assert_eq!(
            c.tool_choice(false, "t"),
            Some(json!({ "type": "function", "function": { "name": "t" } }))
        );
        // 再被拒才退到「不发 tool_choice」
        c.next("does not support tool_choice").expect("还有一档");
        assert_eq!(c.tool_choice(false, "t"), None);
        // 到底了
        assert!(c.next("does not support tool_choice").is_none(), "★ 试完了要说没招了，不是无限重试");
    }

    /// DeepSeek-reasoner：不收 temperature。
    #[test]
    fn temperature_clamped_then_dropped() {
        let mut c = Caps::default();
        // 先按「超范围」钳
        c.next("temperature must be in range [0, 1]").unwrap();
        assert_eq!(c.temp_max, Some(1.0));
        assert_eq!(c.clamp(1.7), 1.0);
        assert_eq!(c.clamp(0.3), 0.3, "范围内的不动");
        // 再被拒就干脆不发
        let s = c
            .next("deepseek-reasoner does not support the parameter `temperature`")
            .unwrap();
        assert_eq!(s.cap, Cap::Temperature);
        let mut body = json!({});
        c.apply(&mut body, false, Some(0.7), 100, Think::OffForTools);
        assert!(body.get("temperature").is_none(), "★ 不收就一个字都不发");
        assert_eq!(body["max_tokens"], 100);
        // 模型收，但用户没填 ⇒ 同样一个字都不发，用服务端默认。
        // **不替用户猜一个「看起来合理」的值** —— 猜错了不报错，只是输出悄悄偏。
        let mut body = json!({});
        Caps::default().apply(&mut body, false, None, 100, Think::OffForTools);
        assert!(body.get("temperature").is_none(), "★ 没填就不发");
    }

    #[test]
    fn max_tokens_field_and_stream_usage() {
        let mut c = Caps::default();
        c.next("Unsupported parameter: 'max_tokens' is not supported with this model")
            .unwrap();
        let mut body = json!({});
        c.apply(&mut body, false, Some(0.0), 42, Think::OffForTools);
        assert_eq!(body["max_completion_tokens"], 42);
        assert!(body.get("max_tokens").is_none());

        assert!(c.next("stream_options is not supported").is_some());
        assert!(!c.stream_usage);
    }

    /// 认不出来的错误不能假装有下一档 —— 那会变成无限重试。
    /// 工具调用优先于 thinking，但**只在需要强制工具调用的那次请求上**。
    /// 关思考是判断段的需要，不是全局策略 —— 回答段还是要它好好想。
    #[test]
    fn thinking_is_off_only_where_tools_must_be_forced() {
        let c = Caps::default();
        let mut judge = json!({});
        c.apply(&mut judge, false, None, 100, Think::OffForTools);
        assert_eq!(judge["thinking"]["type"], "disabled");

        let mut answer = json!({});
        c.apply(&mut answer, false, None, 100, Think::On);
        assert_eq!(answer["thinking"]["type"], "enabled", "★ 不强制工具的请求开思考");

        // 关不掉的型号（GLM-5.3 / kimi-k2.7-code）：退到最低思考量，
        // 而不是放弃工具调用。
        let mut c2 = Caps::default();
        c2.next("This model always engages in thinking and cannot be disabled").unwrap();
        let mut b = json!({});
        c2.apply(&mut b, false, None, 100, Think::OffForTools);
        assert_eq!(b["thinking"]["type"], "enabled");
        assert_eq!(b["reasoning_effort"], "low", "★ 关不掉就压到最低，不是不管了");

        // 这家根本没有 thinking 字段（OpenAI）⇒ 一个字都不发
        let c3 = Caps { thinking: Thinking::Untouched, ..Caps::default() };
        let mut b = json!({});
        c3.apply(&mut b, false, None, 100, Think::On);
        assert!(b.get("thinking").is_none(), "★ 不认这个字段就别发，发了是 400");
    }

    /// 五家的种子都得是能直接用的一档。
    #[test]
    fn every_preset_seeds_a_usable_caps() {
        for p in crate::config::presets() {
            let c = &p.caps;
            assert_eq!(c.forced, Forced::Any, "{} 该从强制工具调用起步", p.name);
            assert!(!c.max_tokens_field.is_empty(), "{} 的 max_tokens 字段名不能是空", p.name);
            // 查证过的上限：Anthropic / Moonshot / 智谱是 1，OpenAI / DeepSeek 是 2
            if let Some(m) = c.temp_max {
                assert!((0.0..=2.0).contains(&m), "{} 的 temp_max 不合理：{m}", p.name);
            }
            // OpenAI 没有 thinking 字段，发了会被当未知参数
            if p.name == "openai" {
                assert_eq!(c.thinking, Thinking::Untouched);
            }
        }
    }

    #[test]
    fn unknown_error_stops_the_ladder() {
        let mut c = Caps::default();
        assert!(c.next("insufficient balance").is_none());
        assert!(c.next("rate limit exceeded").is_none());
    }

    /// 报告要把「为什么需要」写在最前面。用户先要知道缺了它会少什么。
    #[test]
    fn report_leads_with_the_requirement() {
        let r = Report {
            provider: "moonshot".into(),
            model: "kimi-k2.6".into(),
            role: "判断段".into(),
            required: vec![Cap::ForcedTool, Cap::ThinkingOff],
            steps: vec![Step {
                cap: Cap::ForcedTool,
                tried: "改成指名那一个工具".into(),
                outcome: Outcome::Rejected,
                detail: "tool_choice is normalized to auto".into(),
            }],
            verdict: "这个模型当不了判断段".into(),
        };
        let t = r.text();
        assert!(t.contains("C7"), "{t}");
        assert!(t.contains("判断段靠它拿到"), "先写需求：{t}");
        assert!(t.contains("tool_choice is normalized to auto"), "厂商原话原样保留：{t}");
    }
}
