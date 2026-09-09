// 端到端驱动：起一个真的 serve，连 WebSocket，按脚本说话，把全过程记下来。
//
// 用法：
//   node tests/drive.mjs <场景名> <端口> <工作目录> <模型> "第一句" ["第二句" ...]
//
// 输出一份 JSON 报告到 stdout（最后一行是 `===REPORT===` 之后的整块），
// 里面有：时间线各类事件、每次工具调用与返回、最终的推断图与图外记忆、
// 协商结果、所有报错横幅。**判断「有没有真的生效」看的是这份，不是模型说了什么。**
//
// 为什么不复用 mock：这一整套要验的恰恰是 mock 覆盖不到的那一侧 ——
// 真实厂商会不会接受我们发的 tool_choice、会不会把 thinking 吐进正文、
// 模型给的 op 能不能被 actions::parse 收下。

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const [scenario, portArg, dir, model, ...says] = process.argv.slice(2);
const port = Number(portArg);
const KEY = JSON.parse(fs.readFileSync('secrets.json', 'utf8')).keys.moonshot;

// ── 工作目录：每个场景一份，互不干扰 ──
fs.rmSync(dir, { recursive: true, force: true });
fs.mkdirSync(path.join(dir, 'repo'), { recursive: true });
fs.writeFileSync(path.join(dir, 'secrets.json'), JSON.stringify({ keys: { moonshot: KEY } }, null, 2));
fs.writeFileSync(path.join(dir, 'config.json'), JSON.stringify({
  providers: {
    moonshot: { api: 'open_ai_compat', base_url: 'https://api.moonshot.cn/v1', key_env: 'MOONSHOT_API_KEY' },
  },
  roles: {
    judge: { provider: 'moonshot', model, max_tokens: 2048 },
    answer: { provider: 'moonshot', model, max_tokens: 4096 },
    subagent: { provider: 'moonshot', model, max_tokens: 4096 },
  },
  web: { fetch: 'http', fetch_base: '', search: 'none', search_base: '' },
  tools: {
    roots: [path.resolve(dir)], net: true, allow_hosts: ['example.com'], deny_hosts: [],
    deny_names: [], exec_allow: [], max_bytes: 40000, max_lines: 400, max_matches: 60,
    max_depth: 6, max_line_len: 400, max_entries: 200, max_file_bytes: 400000,
  },
}, null, 2));

// 一个小仓库，给 fs_* / repo_tree 用
fs.writeFileSync(path.join(dir, 'repo', 'model.py'), `import torch.nn as nn

class Encoder(nn.Module):
    """把图像编码成 512 维表示。"""
    def __init__(self, dim=512):
        self.dim = dim

    def forward(self, x):
        return self.proj(x)

class Head(nn.Module):
    """线性分类头，消费 Encoder 的输出。"""
    def forward(self, z):
        return self.fc(z)
`);
fs.writeFileSync(path.join(dir, 'repo', 'train.py'), `from model import Encoder, Head

LR = 3e-4
SEED = 42

def train(loader):
    enc, head = Encoder(), Head()
    for x, y in loader:
        loss = ce(head(enc(x)), y)
    return loss
`);
fs.writeFileSync(path.join(dir, 'repo', 'README.md'), '# 玩具仓库\n\nEncoder → Head，train.py 是入口。\n');

const log = { scenario, model, says, events: [], calls: [], banners: [], errors: [], msgs: [] };

// ── 起服务 ──
const srv = spawn('./target/debug/serve', [dir, String(port)], { stdio: ['ignore', 'pipe', 'pipe'] });
let srvOut = '';
srv.stdout.on('data', (d) => { srvOut += d; });
srv.stderr.on('data', (d) => { srvOut += d; });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
await sleep(2500);

const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
let booted = null, closedTurns = new Set(), snap = null, settings = null;

ws.onmessage = (ev) => {
  let m; try { m = JSON.parse(ev.data); } catch { return; }
  log.msgs.push(m.t);
  switch (m.t) {
    case 'boot':
      booted = m; settings = m.settings; snap = m.snap;
      break;
    case 'event':
      log.events.push({ seq: m.seq, kind: m.kind, body: m.body });
      if (m.kind === 'called') for (const c of m.body.calls || []) log.calls.push({ name: c.name, args: c.args });
      if (m.kind === 'returned') {
        const last = log.calls.filter((c) => !c.result).pop();
        if (last) { last.result = String(m.body.content).slice(0, 700); last.outcome = m.body.outcome; }
      }
      break;
    case 'turn_closed': closedTurns.add(m.turn); break;
    case 'snap': snap = m.snap; break;
    case 'caps': settings = m.settings; log.negotiated = m.provider; break;
    case 'caps_failed': log.capsFailed = m; break;
    case 'err': log.errors.push(m.msg); break;
    case 'memory_bad': log.errors.push(`${m.file}: ${m.err}`); break;
  }
};

await new Promise((res, rej) => {
  ws.onopen = res;
  ws.onerror = () => rej(new Error('WebSocket 连不上'));
  setTimeout(() => rej(new Error('连接超时')), 15000);
});

for (let i = 0; i < 60 && !booted; i++) await sleep(250);
if (!booted) { console.error('没收到 boot'); console.error(srvOut); process.exit(1); }
log.session = booted.session;
log.tools = booted.tools;

// ── 按脚本说话 ──
for (let i = 0; i < says.length; i++) {
  ws.send(JSON.stringify({ op: 'send', text: says[i], interrupt: false }));
  const want = i + 1;
  const t0 = Date.now();
  while (!closedTurns.has(want) && Date.now() - t0 < 240000) await sleep(500);
  if (!closedTurns.has(want)) log.errors.push(`第 ${want} 轮 240s 没结束`);
  ws.send(JSON.stringify({ op: 'snap' }));
  await sleep(1500);
}

// ── 收尾：把最终状态记下来 ──
log.settings = settings;
log.caps = settings?.providers?.moonshot?.caps ?? null;
log.ws = snap?.ws ?? null;
log.mermaid = snap?.mermaid ?? null;
log.scenes = snap?.scenes ?? [];
log.stats = log.events.filter((e) => e.kind === 'turn_close').map((e) => {
  try { return JSON.parse(e.body.stats); } catch { return null; }
});
log.judged = log.events.filter((e) => e.kind === 'judged').map((e) => e.body);
log.wrote = log.events.filter((e) => e.kind === 'wrote').map((e) => String(e.body.text).slice(0, 1200));
log.noted = log.events.filter((e) => e.kind === 'noted').map((e) => String(e.body.text).slice(0, 500));
log.serverOut = srvOut.slice(-2000);

ws.close();
srv.kill('SIGKILL');
await sleep(400);

console.log('===REPORT===');
console.log(JSON.stringify(log, null, 2));
