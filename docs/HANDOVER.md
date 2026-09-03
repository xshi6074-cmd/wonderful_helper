# 交接：Premortem（科研新手实验设计前置检查 agent）

来源会话：Cowork「Research assistant agent design」
https://claude.ai/cowork/cse_01CaqkEX1nrM8jEcyCbrp538
交接时间：2026-09-03。本地工作目录 `D:\worksapce\wonderful_helper` 此前为空，无代码。

---

## 1. 项目是什么

课程作业（清华 Rust 课，t02-ai-agent）。名字 **Premortem**（事前验尸）。

一句话理念：**把"撞墙之后才学到的东西"，前移成"动手之前的强制检查"；检查的触发条件与做法是可积累、可交换的数据资产，而不是 agent 的自由发挥。**

服务的场景：科研新手有了模糊 idea 之后、写代码之前的那一段。不写代码。

与 Codex/Claude Code 的结构性差异（必要性论证，写进文档的核心）：
- 它们是 **model-driven** loop，"尽快收敛到可执行代码"的目标会持续压制提问倾向 → 只有撞墙回来才拿到那些问题。
- 本项目是 **harness-driven** loop：判断阶段由 harness 强制执行，模型无权跳过；判断抢占正常回复；phase 推进只能由用户确认。

> 已放弃的早期方案：ExpSpec（结构化实验设计 schema + 一致性校验引擎）。用户明确不要"好看但没用的结构化设计和一堆状态机"。**唯一保留下来的产物是 P5 交接件**——一段给下游执行 agent（Codex）的 brief，不是给人看的表格。

## 2. 已定的设计

### 2.1 判断循环

```
每轮用户输入
├─ [判断阶段] ← harness 强制执行，模型无权跳过
│    载入当前 phase 的卡 → 硬条件预过滤 → 判定 → 收集 fired
├─ 有卡 fired 且打扰预算未耗尽
│    └→ 取优先级最高的 1 张 → 执行它的动作 → 交回用户  ← 抢占正常回复
└─ 无卡 fired
     └→ [正常回复/执行阶段]
```

卡的生命周期只有一个 enum：`Pending / Fired / Satisfied / Dismissed`。

### 2.2 判断卡（checks/*.toml，纯数据，零代码可加）

动作是**有限集**，模型只能在其中选：
`ask_user`（必须带候选项）/ `read_repo` / `enumerate`（强制发散）/ `prune`（强制收敛）/ `cite` / `block`（拒绝进入下一 phase）。

准入规则：**没有 `options` 和 `satisfied_when` 的卡不许入库**——新手答不上开放问题。

字段：`phase` / `requires`（硬条件预过滤，不过就不调模型）/ `judge`（软判据 prompt，输出 `{fired, evidence, missing}`）/ `action` / `options` / `satisfied_when` / `budget_cost`，外加**来源**（对应哪一次真实返工）。

### 2.3 phase（按撞墙的时间顺序划，不按学术流程）

P0 你到底想验证什么 → P1 现有代码/方法实际能做什么 → P2 改动挂在哪、会波及什么 → P3 对照与消融（发散再剪枝）→ P4 算力/数据/时间够不够 → P5 交接。

### 2.4 五张种子卡

| 卡 | 触发 | 动作 |
|---|---|---|
| 旧结论未复现就叠加 | 要在师兄 FlyGCL 结果上加东西，没先跑出原 repo 的数字 | `block` |
| 链路未讲通就动手 | 要改 generator，没确认过它的输出在训练里被谁消费 | `read_repo` |
| 指标测不出 claim | claim 讲"减少遗忘"，指标只有最终平均准确率 | `ask_user` |
| 多改动单对照 | 同时引入新损失 + 新采样，只打算和原方法比 | `enumerate` → `prune` |
| 预算不匹配 | 5 benchmark × 3 seeds × 4 baselines，只有一张卡三天 | `block` + `prune` |

### 2.5 可插拔三层

- **L1** 加一张卡：往 `checks/` 丢一个 toml。扩展性的地板，必须做。
- **L2** 换一组 phase：`playbook.toml` 声明 phase 序列 + 每个 phase 装哪些卡 + 出口条件。性价比高。
- **L3** 换动作类型：需要写 Rust（加 enum variant + 执行逻辑）。**文档里明说这层要改代码**，比假装全可插拔可信。

自举机制：元卡 `postmortem`——用户撞墙回头时引导回答"根因 / 本该在哪个 phase 被拦住 / 什么信号当时已出现"，自动生成新卡草稿。

### 2.6 go / explore mode

**不做两套 prompt**，只是一个维度上的偏置，改三样配置：

| | explore | go |
|---|---|---|
| 允许的协作动作 | 全集（含开放式追问、讲解基础） | 禁用讲解，开放式追问限额 1 次/轮 |
| 打扰预算 | 高 | 低，超了自动进入"给结论" |
| 论文抽取目标 | 动机、领域背景、术语定义 | 方法、超参、实现细节 |

### 2.7 验收（双指标，防作弊）

ground truth = Notion 里的真实返工记录。挑 K 个真实撞墙案例。
- 主指标：**撞墙点提前命中率**（同样模糊指令喂给本 agent 和 Codex，本 agent 是否在写代码前提出了那个后来导致返工的问题）
- 必须配副指标：**干预次数 / 打扰预算消耗**
- 报成 `命中率 @ 干预 ≤3 次` 最稳。

### 2.8 课程要求映射

- R1 Rust 核心 = 判断阶段调度器（卡的加载解析、硬条件预过滤、批量判定、生命周期、打扰预算、phase 闸门、抢占逻辑）
- R2 TUI（ratatui）：侧栏显示本轮触发了哪张卡 / 为什么 / evidence，一键推迟 / 永久忽略
- R3 模型可配
- R4 可打断——**特殊语义：用户可以打断 agent 的干预，不只是打断生成**
- R5 上下文存取
- R6 token 计费——可支撑论证"判断阶段只占总 token 的 X%"

---

## 3. 并发架构：方案 B（已定）

**核心矛盾**：actor 里直接 `.await` 一个完整的模型-工具往返，actor 就被占住，R4 失效。规矩是 **actor 永远不 await 会阻塞的东西**。

方案 A（把 turn 拆成事件驱动 handler）已被否决——控制流散落、易漏分支。用户原话："不要把 handler 分散开了"。

### 三条原则
1. Core 是**唯一写权**。
2. Core 只处理快消息，不 await 长任务、不做磁盘 IO（写盘丢给单独 writer task，顺序由单一 writer 保证）。
3. turn 的控制流全在 `run_turn` 一个函数里；Core 的 match 臂只改状态，不含任何流程。

### 通道拓扑
```
UI ──cmd──▶ Core ──spawn──▶ TurnTask ──state msg──▶ Core
 ▲                                          │
 └────────── UiBus ◀───────────────────────┘   流式 chunk 直发 UI，不经 Core
```

### 关键结构（伪代码骨架已在会话中写全）

```rust
enum CoreMsg {
    // 来自 UI
    UserInput { client_id: Uuid, text: String, mode: SendMode },  // Queue | InterruptAndSend
    UserEdit  { patch: Patch },                                   // origin = User
    Unlock    { paths: Vec<Path> },
    Interrupt { turn: TurnId },
    // 来自 TurnTask
    Snapshot       { reply: oneshot::Sender<Arc<StateView>> },
    TakeInjections { turn: TurnId, reply: oneshot::Sender<Vec<UserMsg>> },
    Propose        { patch: Patch, reply: oneshot::Sender<ApplyReport> },
    Cost           { task: TaskId, role: Role, usage: Usage },
    TurnFinished   { turn: TurnId, outcome: TurnOutcome },
}

struct Core {
    state: StateSnapshot,      // 唯一可变副本
    version: Version,
    locked: HashSet<Path>,     // 用户碰过的字段 —— 见 §4，不持久化
    pending: Patch,            // 本轮模型改动，未提交
    turn: TurnPhase,           // Idle | Running{id, token} | Closing{id}
    inbox: VecDeque<UserMsg>,
    seen: LruSet<Uuid>,        // client_id 去重
    ui: broadcast::Sender<UiEvent>,
    cost: CostLedger,
    writer: mpsc::Sender<WriteJob>,
}
```

`run_turn` 是一条线性控制流：检查点 0（吸收插话）→ 判断段 → run_tools → 检查点 1（subagent 期间用户说话了吗）→ 回答段（重新取快照）。工具往返靠 `continue 'turn` 回到循环顶，顺便重过检查点。每个长 await 都用 `guard!` 宏包一层 `select!` + `token.cancelled()`。

`run_tools` 里 `tick`（30s HEARTBEAT）那一臂发 `StillRunning{pending, elapsed}` —— 这是用户要的"定时先返回一个仍在进行的状态"，串行性没破。后续优化成"先处理已好的部分"只需改 while 的退出条件（"全部 Some" → "关键项 Some"），结构不动。

### 打断是两段式
`Stopping` 立刻发（用户点了停就该马上看到反应），`TurnClosed` 等收敛完再发。中间 UI 显示"正在停止"。

### 必须处理的四件事
1. **陈旧事件**：每个事件带 `turn_id`，对不上就丢。这是这套设计里最常见的 bug。
2. **tool 消息顺序**：用 `Vec<Option<_>>` 按调用顺序占位，不用 push。乱序返回必须重组。
3. **背压**：有界 channel，让生产者在 send 上自然等待。
4. **UI 不能反压 core**：发前端的通道单独一条。

### 两条硬纪律
- **永远不要 `handle.abort()`**，只用 `CancellationToken`。abort 会在任意 await 点撕掉 task，收尾逻辑跑不到。
- `close_open_calls`：打断时①给缺失的 `call_id` 补 `{role:"tool", content:"[interrupted]"}`（不补下次请求被 API 直接拒）；②`partial` 非空写成 assistant 消息打 interrupted 标记（不写历史里就是"用户问了、助手没答"）。

### 落盘顺序
- turn 结束：① append history → ② commit / 标记 pending → ③ 写 snapshot
- 用户编辑：立即 apply + 立即 persist，不等 turn
- 恢复：读最新 snapshot + 重放 history 里 version 之后的记录

先写日志再改状态，崩溃点最坏情况是"历史里有但 state 没应用"，重放即可；反过来会丢。

---

## 4. 四个待定项 —— 用户已在输入框写好答复，**但尚未发送**

⚠️ 以下是我从 Cowork 输入框读到的用户草稿，会话里 Claude 还没有看到、也还没据此改设计。**这是当前最重要的未完成动作。**

| # | 原问题 | 用户答复 | 对设计的影响 |
|---|---|---|---|
| 1 | 用户锁（`locked`）要不要持久化 | **不持久化**。用户可能搞错；改用 prompt 约束模型改前询问 | `locked` 从 state 里拿掉，不进 snapshot；"模型不许覆盖用户字段"的强制点从数据结构挪到 prompt + 询问动作。**这一条推翻了原伪代码里"锁要持久化"的要点** |
| 2 | 打断后 pending 怎么办 | **保留完整 workspace**，只是 turn 死了；turn 里已产生的东西该落盘落盘，用户可以继续 | 比 `mark_pending_orphan` 更进一步——不要"孤儿"标记语义，直接当正常产物落盘 |
| 3 | 注入默认语义 Queue vs InterruptAndSend | **默认 Queue**。理由：interrupt 之后原来未完成的直接丢弃的话，那还不如 queue，更廉价 | 与原默认一致，但理由不同：是成本论证，不是"补充 vs 纠正"的语义论证 |
| 4 | HARD_LIMIT 到了带 timeout 结果继续 vs 整轮失败 | **继续**。模型可以重试，且不要过度干扰用户 | 与原选择一致 |
| 5 | 取消时的成本记账（流式中断拿不到 usage） | 「这是小问题」—— 低优先级，估算兜底即可 | 不阻塞主线，R6 用估算补 |

---

## 5. 已有产物

在 Cowork 会话的 Outputs 面板里（不在本地）：
- `选题帖.md` —— 选题帖终稿
- `设计文档.html` —— Artifact「Premortem 设计文档」，独立 HTML 双击可开。含五张图（模块 / 单轮 / state 三层 / turn 事务与打断 / 上下文分层）。图 4 caption 已写明"控制流由 turn task 持有，SessionActor 只裁决状态写入"。文档刻意**不含**任何"为什么这么选"的论证（那属于开发文档/答辩口头讲）。

已确定的技术选型：Rust core + `tokio`（SessionActor / turn task / subagent）+ `tokio_util::sync::CancellationToken` + `serde` + `ratatui`。模型客户端 `rig` 或 `async-openai`。**不碰 Web**。

---

## 6. 下一步（按优先级）

1. **把 §4 的五条答复真正应用到 core 设计上**，尤其第 1 条会改数据结构。（在原会话里发出草稿，或在本地直接改。）
2. 卡库第一版：10 张卡的完整 toml，来源从 Notion 开发日志里翻真实返工记录。**卡的质量就是项目的质量**——少于 8–10 张或拍脑袋编的，就退化成会打岔的 chatbot。
3. Rust 工程骨架：crate 选型 + 判断阶段并发批量判定（唯一有技术含量的地方）。
4. 扩展性证据：找 2 个别方向的同学各加 2 张卡，记录耗时；自己做一份非 ML playbook。

## 7. 两个坦率的风险（原文保留）

- 卡的质量就是项目的质量。设计文档里每张卡都要标来源：对应哪一次真实返工。
- "问一堆正确的废话"是最大的失败模式。没有 `options` 和 `satisfied_when` 的卡不许入库。
