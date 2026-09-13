# Premortem

Premortem 是一个面向科研实验设计的本地 AI 助手:让模糊的idea正确收敛，把与agent痛苦的意图对齐固化成agent必须执行的流程，探索更符合科研学生体质的workspace。

## Quick start

准备支持 Rust 2024 edition 的工具链，以及至少一个兼容的模型 API 密钥。

```sh
cargo run --bin serve -- . 7878
```

第一个参数是项目数据目录，也是本地文件工具默认可读的根目录；分析其他项目时，把 `.` 换成对应路径。

启动后会自动用系统默认浏览器打开 [http://127.0.0.1:7878](http://127.0.0.1:7878)（WSL 里交给 Windows 侧的浏览器）；没打开的话手动访问这个地址。不想自动打开就加 `--no-browser`，或设环境变量 `PREMORTEM_NO_BROWSER=1`。

打开之后：

1. 在“配置”中为 `judge`、`answer`、`subagent` 三个角色选择 provider 和模型，并存入相应provider的密钥。
2. 在输入框下方确认模型可读的项目目录；需要时可在“工具”中调整目录、联网和返回体量限制。
3. 点击“新对话”开始体验吧！

也可以先通过环境变量提供密钥。

Windows PowerShell：

```powershell
$env:ANTHROPIC_API_KEY = "sk-..."
cargo run --bin serve -- . 7878
```

macOS / Linux / WSL：

```sh
export ANTHROPIC_API_KEY="sk-..."
cargo run --bin serve -- . 7878
```

服务只监听本机 `127.0.0.1`。HTML、CSS 和 JavaScript 已编进 Rust 二进制，不需要 Node 构建步骤。网页抓取也内置在 Rust 进程中：抓取开关、域名白名单和体量限制可在“配置 → 工具权限与上限”中调整。

## 核心功能
- 设计"场景分流"机制，在对话前先判断当前是否命中某些经验的相似场景，将相应经验灌入上下文供agent参考。同时给对话提供一键蒸馏，形成经验闭环。用户也可以选择蒸馏与其它agent的协作记录，选择保存。
- 维护一层工作memory：把梳理架构图固定为必须执行的动作，要求agent设计时考虑完整实验链路，对于未定部分诚实标出；用户可以随时修改架构图，同时随每轮对话对实验设计有清晰把握，有利于意图快速对齐。显示维护待决策和已经决策的部分，减少错过设计要点的可能。
- 强调个性化：分设行动和探索两种模式，对于希望快速落实方案的场景在不损失精度的情况下要求agent对用户精准发问，减少交互；对于用户希望拓展学习的场景鼓励agent进行发散和开放式提问。所有prompt方便修改，经验场景易于插拔。

## 演示用例
受本课堂AI lab启发，您可以选择要求agent根据 https://lab.cs.tsinghua.edu.cn/rust/ 中的四个lab内容设计一个实验，用于对比编码前设计test和编码后设计test的成功率。
prompt可以是：
```
https://lab.cs.tsinghua.edu.cn/rust/ 找到4个lab作业。我想借4个vibe-coding题目设计实验，测试agent辅助coding时在写业务代码前写测试代码/写业务代码后写测试代码的准确率差异。给我实验流程图.
```

## 设计文档

- [设计文档](docs/设计文档.html)
- [数据契约](docs/数据契约.html)
- [推断图](docs/推断图.html)
- [实现与 UI 接线审计](docs/implementation-audit.md)
