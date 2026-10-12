# Premortem · 逐轮规划图

把用户给出的 parts 和逐 part 大体规划，维护成细节充分、包含未决项、允许扩展的规划图。

核心是 `maintain-plan-map`：用户负责给出工作分区和大体方向；Codex 把路径、接口、依据、实验分支和验收具体化，并按每轮真实进展增量更新。候选仍标为候选，旧结果保留其配置和证据范围。

## 使用

Skill 位于 [.agents/skills/maintain-plan-map](.agents/skills/maintain-plan-map/SKILL.md)。在包含该目录的仓库中使用 Codex，可按名称调用：

```text
用 $maintain-plan-map 维护这项工作。
目标与交付：……
我给出的 parts 和大体规划：
A：……；打算先……，产物是……
B：……；打算先……，产物是……
C：……；打算先……，产物是……
已有图／材料：……
这轮进展或改动：……
目前不确定：……
```

首次默认写入当前项目的 `docs/plan-map.md`，存在规划图时优先原位更新。只写已知内容，缺项变成具名未决项；可以先做局部图，不要求把所有疑问解决完才起步。

跨项目使用时，把整个 `maintain-plan-map` 目录放到目标项目的 `.agents/skills/`，或个人 `~/.agents/skills/`，保留目录内的引用。目录发现与调用方式见 [Codex 官方 skill 文档](https://developers.openai.com/codex/skills)。

## 图中要看见什么

- 总览中的 parts、真实依赖、产物与分支；逐 part 展开中的实际操作、关键接口和实验臂。
- 同一对象的稳定 ID，以及由图指向的详细记录。
- 方案状态、执行进度和证据来源；未决项的影响、候选、解决方式和复查时点。
- 消融究竟改变什么决定、只改什么、固定什么、怎样比较；哪些可并行，哪些依赖前一结果。
- 数据、目标或接口变更后，具体哪些结论需要复核；已解决问题如何成为带依据的决定。
- 交付格式、验收口径与实际约定的对齐时点。

图和明细保存在一个权威 Markdown 文件里。默认用 Mermaid，复杂处局部展开。调用 skill 本身只维护规划；实验执行使用用户已给出的授权。

## 内容

| 文件 | 用途 |
| --- | --- |
| [SKILL.md](.agents/skills/maintain-plan-map/SKILL.md) | 调用范围、协作分工、图与明细、逐轮更新、验收 |
| [experiment-detail.md](.agents/skills/maintain-plan-map/references/experiment-detail.md) | 数据、结构、loss、状态、消融、有效性和交付细节 |
| [plan-map.template.md](.agents/skills/maintain-plan-map/assets/plan-map.template.md) | 按实际任务删改的初建骨架 |
| [example-wind-plan.md](.agents/skills/maintain-plan-map/references/example-wind-plan.md) | 假想风速任务的总览、展开、记录和第二轮更新 |
| [openai.yaml](.agents/skills/maintain-plan-map/agents/openai.yaml) | Codex 的显示名称和调用提示 |

## 设计来源

从原 Premortem 实现中提取以下规则：

| 原设计 | Skill 中的做法 |
| --- | --- |
| Graph 的节点、边与开放词表 | 用稳定 ID 与 Mermaid 表达模块、数据流、依赖和实验分支 |
| Field.anchor 与图外工作记忆 | 同文件中的锚定明细记录目标、理由、未决项与验收 |
| patch 与用户编辑优先 | 增量修改同一对象，保留用户直接编辑，不通过换 ID 绕开 |
| 来源、观测与假设的区分 | 分别标记依据、方案与执行状态，不把运行当作验证 |
| 上游修改后的完整链路复查 | 沿依赖标记下游需要复核的 loss、对照、指标与交付 |

原始规则见 [memory/prompts.toml](https://github.com/xshi6074-cmd/wonderful_helper/blob/b565b6260066f5cad81415bbeb731413fb8ce4ac/memory/prompts.toml) 和 [src/state.rs](https://github.com/xshi6074-cmd/wonderful_helper/blob/b565b6260066f5cad81415bbeb731413fb8ce4ac/src/state.rs)。旧应用保留在 Git 历史中；这里的可用单元是规划图 skill，不包含应用运行时。
