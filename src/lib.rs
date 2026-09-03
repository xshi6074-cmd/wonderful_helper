//! # Premortem —— 事前验尸
//!
//! 理念：把「撞墙之后才学到的东西」，前移成「动手之前的强制检查」。
//! 检查的触发条件与做法是可积累、可交换的数据资产（判断卡），不是模型的自由发挥。
//!
//! ## 这个 crate 的边界
//!
//! 本 crate 是 **core**：并发骨架 + 状态写权仲裁 + turn 控制流 + 打扰预算 + phase 闸门。
//! 判断卡库（`checks/*.toml` 的加载与判定）、TUI、真实模型客户端都在 core 之外，
//! 通过 [`model::ModelClient`] / [`tools::Tool`] 两个 trait 接入。
//!
//! ## 架构：方案 B（turn task 持有控制流）
//!
//! ```text
//!   UI ──cmd──▶ Core ──spawn──▶ TurnTask ──state msg──▶ Core
//!    ▲                                          │
//!    └────────── UiBus ◀───────────────────────┘   流式 chunk 直发 UI，不经 Core
//! ```
//!
//! 三条原则（贯穿全 crate，改代码前请先确认没有破坏它们）：
//!
//! 1. **Core 是唯一写权**。别人拿到的都是 [`state::StateView`] 不可变快照，
//!    改动一律提交 [`state::Patch`] 由 Core 裁决。
//! 2. **Core 只处理快消息**：[`core::Core::handle`] 全同步、微秒级返回，
//!    不 `.await` 长任务、不做磁盘 IO（写盘丢给 [`persist`] 的单一 writer task）。
//! 3. **turn 的控制流全在 [`turn::run_turn`] 一个函数里**，
//!    Core 的 match 臂只改状态，不含任何流程。
//!
//! ## 两条硬纪律
//!
//! - 永远不要 `JoinHandle::abort()`，只用 `CancellationToken`。
//!   abort 会在任意 await 点撕掉 task，[`turn::close_open_calls`] 的收尾根本跑不到。
//! - 每个长 await 都要过 [`turn::guarded`]，否则 cancel 之后任务还在跑，钱照烧。

pub mod context;
pub mod core;
pub mod cost;
pub mod handle;
pub mod ids;
pub mod memory;
pub mod mock;
pub mod model;
pub mod msg;
pub mod persist;
pub mod scene;
pub mod state;
pub mod tools;
pub mod turn;

pub use handle::CoreHandle;
pub use ids::{TaskId, TurnId, Version};
