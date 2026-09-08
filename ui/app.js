// premortem UI —— 零构建步骤的纯 ES module。
//
// 组织方式：一个 S（全局状态）+ 若干 render 函数。收到消息就改 S、重画受影响的那块。
// 没有框架、没有虚拟 DOM —— 一次对话几十到几百条事件，整块重画是微秒级，
// 换来的是「状态只有一份、画法只有一处」，比手动打补丁好维护得多。
//
// 唯一用 innerHTML 的地方是**服务端渲染好的 markdown**。那份 HTML 在 Rust 里
// 已经把 raw HTML 事件丢掉了（见 src/markdown.rs），是安全的；除此之外
// 所有文本都走 textContent。

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

function h(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k === 'html') e.innerHTML = v;          // 只给服务端渲染过的 markdown
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v === true) e.setAttribute(k, '');
    else e.setAttribute(k, v);
  }
  for (const kid of kids.flat()) {
    if (kid === null || kid === undefined || kid === false) continue;
    e.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return e;
}

// ───────────────────────── 状态 ─────────────────────────

const S = {
  session: null, sessions: [], settings: null, keys: {}, warnings: [],
  tools: [], web: '无', metrics: '', memory: [],
  timeline: [], snap: null,
  mode: 'explore', tokens: 0, queued: 0,
  scenes: [],             // 本轮判成的一组场景（多选）
  playbook: [],           // 场景库全文，左栏一块一块地改
  defaultTools: [],
  running: false, turn: null, connected: false,
  stream: null,           // { turn, text }
  fresh: new Set(),       // 刚被改过的图元素 id，画一次高亮就够
  presets: [],            // provider 预设，来自后端（base_url/密钥变量名只有那一份）
  saved: null,            // boot 时的配置原件。左栏表单会改 S.settings，这份不动
  over: null,             // 遮罩里正开着什么 { kind, dirty(), ... }
  editor: null,           // 遮罩里那个文件的 { kind, file, orig }
  browse: null,           // 目录浏览器的当前一层
  distill: null,          // 蒸馏草稿的分节结果，等用户勾选写回
  caps: null,             // 协商失败报告。在没换模型之前它一直有效
  focusRole: null,        // 配置面板要展开并滚到哪个角色
  banners: [],            // { level, text, key }
};

// ───────────────────────── 连接 ─────────────────────────

let ws = null, retry = 0;

function connect() {
  ws = new WebSocket(`ws://${location.host}/ws`);
  ws.onopen = () => { retry = 0; S.connected = true; renderTop(); };
  ws.onclose = () => {
    S.connected = false; renderTop();
    retry = Math.min(retry + 1, 6);
    setTimeout(connect, 400 * retry);
  };
  ws.onmessage = (ev) => {
    let m; try { m = JSON.parse(ev.data); } catch { return; }
    onMsg(m);
  };
}

function send(op, extra = {}) {
  if (!ws || ws.readyState !== 1) return false;
  ws.send(JSON.stringify({ op, ...extra }));
  return true;
}

function onMsg(m) {
  switch (m.t) {
    case 'boot':
      S.session = m.session; S.sessions = m.sessions; S.settings = m.settings;
      S.keys = m.keys; S.warnings = m.warnings || []; S.tools = m.tools || [];
      S.web = m.web; S.metrics = m.metrics; S.memory = m.memory || [];
      S.playbook = m.scenes || [];
      S.defaultTools = m.default_tools || [];
      S.presets = m.presets || [];
      S.saved = m.settings ? JSON.parse(JSON.stringify(m.settings)) : null;
      S.timeline = m.timeline || []; S.snap = m.snap; S.stream = null;
      S.turn = m.snap?.turn ?? null; S.running = S.turn !== null;
      S.mode = 'explore'; S.scenes = []; S.queued = 0;
      S.foot = null;
      S.tokens = S.timeline.filter(e => e.kind === 'cost').reduce((n, e) => n + (e.body.usage?.prompt || 0) + (e.body.usage?.completion || 0), 0);
      if (m.snap) { S.mode = m.snap.mode; S.scenes = m.snap.scenes || []; S.queued = m.snap.queued; }
      S.banners = S.banners.filter(b => b.level === 'bad').concat(S.warnings.map((w, i) => ({ level: 'warn', text: w, key: 'cfg' + i })));
      renderAll();
      break;
    case 'event':
      if (S.timeline.some(e => e.seq === m.seq)) break;
      S.timeline.push(m);
      if (['asked', 'answered', 'judged', 'scene_overridden'].includes(m.kind)) send('snap');
      if (m.kind === 'wrote') S.stream = null;
      renderStream(); renderTop();
      break;
    case 'delta':
      if (!S.stream || S.stream.turn !== m.turn) S.stream = { turn: m.turn, text: '' };
      S.stream.text += m.text;
      liveDelta();
      break;
    case 'turn_started': S.turn = m.turn; S.running = true; S.stream = null; renderTop(); renderStream(); break;
    case 'turn_closed':
      if (S.turn !== null && S.turn !== m.turn) break;
      S.turn = null; S.running = false; S.stream = null; dropBanner('stop'); dropBanner('run'); renderTop(); renderStream(); send('snap');
      // 标题是后端按第一句用户发言算的，但那是 boot 时算的。刚说完第一句时
      // 本地补一下，不然侧栏会一直挂着「空对话」直到下次切会话。
      titleSelf();
      break;
    case 'stopping': banner('info', '正在停止…', 'stop'); break;
    case 'queued': S.queued = m.pending; renderTop(); break;
    case 'state_changed':
      // 记下这一批动过的图元素，画图时让它们亮一下再褪去
      for (const o of m.ops || []) if (o.id && typeof o.id === 'string') S.fresh.add(o.id);
      send('snap');
      break;
    case 'snap':
      S.snap = m.snap; S.mode = m.snap.mode;
      S.scenes = m.snap.scenes || []; S.queued = m.snap.queued;
      S.turn = m.snap.turn ?? null; S.running = S.turn !== null;
      renderRight(); renderTop(); renderAsk();
      break;
    case 'cost': S.tokens = m.total; renderTop(); break;
    case 'mode': S.mode = m.to; renderTop(); break;
    case 'still_running':
      banner('info', `工具仍在跑：${m.pending.join(', ')}（${(m.elapsed_ms / 1000) | 0}s）`, 'run');
      break;
    case 'task_done': dropBanner('run'); break;
    case 'compacted':
      banner('info', `已折叠 ${m.folded} 条早期对话：${m.before} → ${m.after} tok`, 'fold');
      break;
    case 'distilled':
      // 分好节的草稿直接铺进遮罩。只有用户勾选并点写入，才会碰持久层。
      S.distill = { path: m.path, error: m.error, sections: (m.sections || []).map(s => ({ ...s, on: true })) };
      dropBanner('distill-run');
      if (m.error) banner('bad', m.error, 'distill');
      if (S.distill.sections.length) openDistill();
      break;
    case 'applied':
      if (m.errors && m.errors.length) banner('bad', m.errors.join('；'), 'distill');
      if (m.ok) {
        banner('info', `写入 ${m.ok} 个文件：${(m.files || []).join('、')}`, 'distill');
        S.distill = null; closeOver(true);
      }
      break;
    case 'footprint': S.foot = m; renderTop(); break;
    case 'persist':
      if (m.ok) dropBanner('persist');
      else banner('bad', `落盘失败（已积压 ${m.pending} 条），对话不受影响：${m.why}`, 'persist');
      break;
    case 'memory_bad': banner('warn', `${m.file} 解析失败，已回退内置：${m.err}`, 'mem' + m.file); break;
    // 协商试成了：静默更新配置，让面板显示实测出来的调用方式。
    case 'caps':
      S.settings = m.settings; S.saved = JSON.parse(JSON.stringify(m.settings));
      renderConfig();
      break;
    // 协商到底了。**这是持续状态**，不按 10 秒赶走 —— 在换模型之前，
    // 每一轮的场景判定都是空的，而那件事在对话里完全看不出来。
    case 'caps_failed':
      S.caps = m;
      banner('bad', `${m.role}用不了 ${m.provider}·${m.model}：${m.verdict.split('。')[0]}。`, 'caps');
      openCaps();
      break;
    case 'memory': S.memory = m.files; renderMemory(); break;
    case 'scenes':
      S.playbook = m.scenes || [];
      S.defaultTools = m.default_tools || S.defaultTools;
      renderMemory();
      if (!m.quiet) banner('info', '场景库已保存，下一轮生效', 'scene');
      break;
    case 'sessions': S.sessions = m.sessions || []; renderSessions(); break;
    case 'recovered':
      banner('info', `恢复了上次的会话：${m.events} 条事件，修补 ${m.crashed} 个中断轮次，${m.reopened} 个提问重新打开`, 'rec');
      break;
    case 'forked': banner('info', `已从第 ${m.from_turn} 轮分出新分支`, 'fork'); break;
    case 'probe': $$('.probe-out').forEach(el => el.textContent = m.text); break;
    case 'browse':
      S.browse = m;
      if (m.error) banner('bad', m.error, 'browse');
      else renderBrowser();
      break;
    case 'err': banner('bad', m.msg, 'err' + Date.now()); break;
    case 'lagged': banner('warn', `推送积压，正在同步 ${m.n} 条遗漏事件`, 'lag'); send('sync'); break;
  }
}

/** 这两条是**持续状态**，要等对应的解除事件才消失，不能按时间赶走：
 *  盘还在坏、工具还在跑的时候，横幅消失了等于骗人。其余的都停 10 秒。 */
const STICKY = new Set(['persist', 'run']);
const HOLD_MS = 10000;
const timers = {};

function banner(level, text, key) {
  S.banners = S.banners.filter(b => b.key !== key);
  S.banners.push({ level, text, key });
  clearTimeout(timers[key]);
  if (!STICKY.has(key)) timers[key] = setTimeout(() => dropBanner(key), HOLD_MS);
  renderBanner();
}

function dropBanner(key) {
  clearTimeout(timers[key]);
  delete timers[key];
  S.banners = S.banners.filter(b => b.key !== key);
  renderBanner();
}

// ───────────────────────── 顶栏 ─────────────────────────

function renderTop() {
  $$('#mode-seg button').forEach(b => b.classList.toggle('on', b.dataset.mode === S.mode));
  // 场景是一组。判断段自己会判，用户想插手就点开多选 —— 选中的几份 guidance
  // 会一起注入，不是「换个标签」。
  const sc = $('#scene-chip');
  const names = S.scenes.map(id => S.playbook.find(x => x.id === id)?.label || id);
  const shown = names.filter(n => n !== '不做特殊干预');
  sc.textContent = shown.length ? '场景 ' + shown.join(' + ') : '场景 自动';
  sc.title = '点开选场景（可多选）。选中的 guidance 下一轮会一起注入。';
  const foot = S.foot ? `　·　上下文 ${S.foot.total} tok` : '';
  const tc = $('#token-chip');
  tc.textContent = `${S.tokens} tok${foot}`;
  // 逼近上限时自己变色。数字要人去比对，颜色不用。
  const frac = S.foot ? S.foot.total / 128000 : 0;
  tc.className = 'chip ghost' + (frac > 0.9 ? ' bad' : frac > 0.7 ? ' warn' : '');
  tc.title = frac > 0.7 ? '上下文快满了，下一轮会自动折叠早期对话' : '本会话累计 token';
  const c = $('#conn-chip');
  c.textContent = S.connected ? (S.session ? S.session.slice(0, 8) : '无会话') : '断线重连中…';
  c.className = 'chip ' + (S.connected ? 'ghost' : 'bad');
  $('#stop').hidden = !S.running;
  const ready = S.connected && !!S.session;
  $('#send').disabled = !ready;
  $('#send').title = ready ? '' : '还没有会话 —— 先在左栏配置里填模型密钥';
  $('#input').placeholder = ready
    ? '说点什么…  Enter 发送 · Shift+Enter 换行'
    : '还没有会话。左栏「配置」里填一个模型密钥。';
  const q = $('#queue-chip');
  q.hidden = !S.queued;
  if (S.queued) q.textContent = `${S.queued} 条排队`;
}

function renderBanner() {
  const b = $('#banner'); b.textContent = '';
  for (const n of S.banners) {
    b.append(h('div', {
      class: 'note ' + n.level,
      title: '点掉',
      onclick: () => dropBanner(n.key),
    }, n.text));
  }
}

// ───────────────────────── 消息流 ─────────────────────────

/** 把线性事件流分成「块」：轮外的用户发言各成一块，一轮里的东西合成一块。 */
function blocks() {
  const out = []; let cur = null;
  const flush = () => { if (cur) out.push(cur); cur = null; };
  for (const e of S.timeline) {
    if (e.kind === 'turn_open') { flush(); cur = { type: 'turn', turn: e.turn, proc: [], msgs: [], closed: false }; continue; }
    if (e.kind === 'turn_close') { if (cur) { cur.closed = true; cur.stats = e.body.stats; cur.aborted = e.body.aborted; } flush(); continue; }
    if (e.kind === 'said' || e.kind === 'answered') { flush(); out.push({ type: 'user', e }); continue; }
    if (!cur) { cur = { type: 'turn', turn: e.turn, proc: [], msgs: [], closed: true }; }
    if (e.kind === 'wrote' || e.kind === 'asked') cur.msgs.push(e);
    else cur.proc.push(e);
  }
  flush();
  return out;
}

const PROC_LABEL = {
  judged: '场景判定', called: '发起工具', returned: '工具返回', aborted: '调用中止',
  inferred: '推断更新', edited: '你的编辑', noted: '备注', folded: '折叠',
  cost: '计费', scene_overridden: '换场景',
};

function renderStream() {
  const st = $('#stream');
  const stick = st.scrollTop + st.clientHeight > st.scrollHeight - 120;
  st.textContent = '';

  for (const b of blocks()) {
    if (b.type === 'user') {
      const e = b.e;
      const who = e.kind === 'answered' ? '回答' : '你';
      st.append(h('div', { class: 'turn' },
        h('div', { class: 'msg user' },
          h('div', { class: 'who' }, who),
          h('div', { class: 'bubble md', html: e.html || '' }))));
      continue;
    }
    const box = h('div', { class: 'turn-block' });
    if (b.proc.length) box.append(procBlock(b));
    for (const e of b.msgs) {
      if (e.kind === 'asked') {
        box.append(h('div', { class: 'turn' },
          h('div', { class: 'msg ask' },
            h('div', { class: 'who' }, '提问'),
            h('div', { class: 'bubble md', html: e.html || '' }))));
      } else {
        box.append(h('div', { class: 'turn' },
          h('div', { class: 'msg assistant' + (e.body.interrupted ? ' note' : '') },
            h('div', { class: 'who' }, e.body.interrupted ? '半截' : '助理'),
            h('div', { class: 'bubble md', html: e.html || '' }))));
      }
    }
    if (b.closed) {
      // 那一坨 stats JSON 不摊在对话里 —— 它是排查用的，不是读对话时要看的。
      // 挂成 title，想看的时候悬停；执行过程本来就在上面的折叠块里。
      box.append(h('div', { class: 'turn-foot' },
        h('button', {
          title: '从这一轮分出一条新分支。原会话一条都不动。',
          onclick: () => send('fork', { turn: b.turn, title: `从第 ${b.turn} 轮分支` }),
        }, '⑂ 从这里分支'),
        b.stats ? h('span', { class: 'hint mono-hint', title: b.stats }, statLine(b.stats)) : null,
        b.aborted ? h('span', { class: 'chip warn' }, '中止收尾') : null));
    }
    st.append(box);
  }

  if (S.stream) {
    st.append(h('div', { class: 'turn', id: 'live' },
      h('div', { class: 'msg assistant streaming' },
        h('div', { class: 'who' }, '助理'),
        h('div', { class: 'bubble', id: 'live-text' }, S.stream.text))));
  }
  if (!S.timeline.length && !S.stream) {
    // 只报**三个角色真正要用**的那几个 provider。S.keys 里还有 firecrawl 这类
    // 非模型条目，全列出来会让人以为不填就起不来。
    const need = [...new Set(Object.values(S.settings?.roles || {}).map(r => r.provider))];
    const missing = need.filter(n => !S.keys[n]?.has);
    const noSession = !S.session;
    // 不写死某一家。默认配置里三个角色都指向 anthropic，照着 missing 直接报
    // 会让人以为「必须先有 anthropic 的 key」—— 而任何一家都行。
    st.append(noSession
      ? h('div', { class: 'blank' },
          h('h2', {}, '未配置 API key'),
          h('p', {}, missing.length
            ? `当前三个角色指向 ${missing.join(' / ')}，还没有密钥。换成你有 key 的那家、或者把密钥填上，会话就会自动起来。`
            : '会话还没起来，看看左栏配置里有没有报错。'),
          h('div', { class: 'cta' },
            h('button', { class: 'primary', onclick: () => openTab('config') }, '去配置'),
            h('button', { onclick: () => send('open', { session: '' }) }, '再试一次')))
      : h('div', { class: 'blank' },
          h('h2', {}, '说点什么开始'),
          h('p', {}, '把你想做的实验讲一遍就行。右栏会跟着长出一张推断图 —— ',
            h('b', { style: 'color:var(--warn)' }, '虚线的部分是模型自己猜的'),
            '，那才是值得先聊的地方。')));
  }
  if (stick) st.scrollTop = st.scrollHeight;
  addCopyButtons();
  updateToBottom();
}

/** 一轮的指标压成一行人话。全文挂在 title 上，想查还是查得到。 */
function statLine(raw) {
  let s; try { s = JSON.parse(raw); } catch { return ''; }
  const bits = [];
  const secs = (ms) => (ms / 1000).toFixed(1) + 's';
  bits.push(secs((s.judge_ms || 0) + (s.answer_ms || 0) + (s.tool_ms || 0)));
  if (s.tools_run) bits.push(`工具 ${s.tools_run}`);
  if (s.inferred_ops) bits.push(`推断 ${s.inferred_ops}`);
  if (s.dropped_ops) bits.push(`丢弃 ${s.dropped_ops}`);
  if (s.asked_user) bits.push(`提问 ${s.asked_user}`);
  if (s.compacted) bits.push(`折叠 ${s.compacted}`);
  return bits.join(' · ');
}

/** 代码块的复制按钮。聊天 UI 的基本便利，没有它就得手动框选。 */
function addCopyButtons() {
  for (const pre of $$('#stream .md pre')) {
    if ($('.copy', pre)) continue;
    const b = h('button', {
      class: 'copy', title: '复制',
      onclick: async (e) => {
        e.stopPropagation();
        try {
          await navigator.clipboard.writeText(pre.textContent.replace(/复制$/, ''));
          b.textContent = '已复制'; b.classList.add('done');
          setTimeout(() => { b.textContent = '复制'; b.classList.remove('done'); }, 1400);
        } catch { b.textContent = '复制不了'; }
      },
    }, '复制');
    pre.append(b);
  }
}

function updateToBottom() {
  const st = $('#stream'), btn = $('#to-bottom');
  if (!btn) return;
  // 两个条件都要：内容够长**而且**确实翻上去了。少了前一个，
  // 短对话时按钮会一直挂在那儿，点了什么也不会发生。
  const scrollable = st.scrollHeight - st.clientHeight > 120;
  const nearBottom = st.scrollTop + st.clientHeight > st.scrollHeight - 160;
  // 空态那一屏本来就没有「最新」可回
  btn.hidden = !S.timeline.length || !scrollable || nearBottom;
}

/** 流式只改那一个文本节点，不重画整条流 —— 否则每来一个 delta 都会滚动跳一下。 */
function liveDelta() {
  let n = $('#live-text');
  if (!n) { renderStream(); n = $('#live-text'); if (!n) return; }
  n.textContent = S.stream.text;
  const st = $('#stream');
  if (st.scrollTop + st.clientHeight > st.scrollHeight - 200) st.scrollTop = st.scrollHeight;
}

/** 执行过程：与回答分开、默认折叠。想看细节再展开。 */
function procBlock(b) {
  const steps = h('div', { class: 'steps' });
  for (const e of b.proc) steps.append(procStep(e));
  const kinds = [...new Set(b.proc.map(e => PROC_LABEL[e.kind] || e.kind))];
  return h('div', { class: 'proc' },
    h('details', {},
      h('summary', {}, `第 ${b.turn} 轮 · ${b.proc.length} 步`,
        h('span', { class: 'hint' }, kinds.join(' · '))),
      steps));
}

function procStep(e) {
  const k = PROC_LABEL[e.kind] || e.kind;
  const v = h('div', { class: 'v' });
  const B = e.body || {};
  switch (e.kind) {
    // scenes 是一组。上一轮把单值改成数组时漏了这里，于是显示成 undefined。
    case 'judged': v.append(`${(B.scenes || []).join(' + ') || '无'}　—　${B.rationale || ''}`); break;
    case 'called':
      for (const c of B.calls || []) v.append(h('div', {}, h('code', {}, c.name), ' ', JSON.stringify(c.args)));
      break;
    case 'returned':
      v.append(h('div', {}, h('code', {}, B.name), ' ', h('span', { class: 'hint' }, B.outcome)));
      v.append(h('pre', {}, B.content || ''));
      break;
    case 'inferred': {
      const ops = (B.ops || []).map(o => o.op + (o.path ? ' ' + o.path : '') + (o.id ? ' ' + o.id : ''));
      v.append(`生效 ${(B.ops || []).length} 条${ops.length ? '：' + ops.join('、') : ''}`);
      if ((B.dropped || []).length) {
        v.append(h('div', { class: 'hint' }, `丢弃 ${B.dropped.length} 条（你本轮改过这些位置）：${B.dropped.join('、')}`));
      }
      break;
    }
    case 'edited':
      v.append((B.ops || []).map(o => o.op + ' ' + (o.path || o.id?.node || o.id?.edge || o.id || '')).join('、'));
      break;
    case 'noted': v.append(B.text || ''); break;
    case 'folded': v.append(`把 #${B.from}–#${B.to} 折成摘要（${B.folded} 条）`); break;
    case 'cost': v.append(`${B.role}　输入 ${B.usage?.prompt ?? 0} / 输出 ${B.usage?.completion ?? 0}${B.usage?.estimated ? '（估算）' : ''}`); break;
    case 'aborted': v.append(`${B.call_id}：${B.why}`); break;
    case 'scene_overridden': v.append(`${B.from} → ${B.to}`); break;
    // phase_set 是已废弃的事件，只有老会话里还有。显示原样那一个值就够了。
    case 'phase_set': v.append(String(B.to)); break;
    default: v.append(JSON.stringify(B));
  }
  const bad = e.kind === 'aborted' || (e.kind === 'returned' && B.outcome && B.outcome !== 'ok');
  return h('div', { class: 'step' + (bad ? ' bad' : '') }, h('div', { class: 'k' }, k), v);
}

/** 模型提了问 ⇒ 输入区上方出现选项按钮。提问是持久实体，重启也还在。 */
function renderAsk() {
  const row = $('#ask-row'); row.textContent = '';
  const qs = S.snap?.open_questions || [];
  row.hidden = !qs.length;
  for (const q of qs) {
    row.append(h('div', { class: 'q' }, '模型在问：' + q.question));
    for (const o of q.options) {
      row.append(h('button', { onclick: () => send('answer', { seq: q.seq, choice: o }) }, o));
    }
    row.append(h('button', {
      onclick: () => { $('#input').focus(); },
      title: '也可以直接在下面自由回答',
    }, '自己写'));
  }
}

// ───────────────────────── 右栏：推断 ─────────────────────────

function isGuess(prov) {
  const s = prov?.source;
  return !s || s === 'Guess';
}

function renderRight() {
  const ws = S.snap?.ws;
  const g = ws?.flow;
  drawGraph(g);
  $('#graph-hint').textContent = g?.view === 'sketch' ? '（模型自己画的，不能点选）' : '';
  const src = $('#graph-src');
  const mer = S.snap?.mermaid;
  src.hidden = !mer;
  if (mer) $('pre', src).textContent = mer;

  const fl = $('#fields'); fl.textContent = '';
  const fields = Object.entries(ws?.fields || {});
  if (!fields.length) fl.append(h('div', { class: 'empty' }, '还没有图外推断'));
  for (const [path, f] of fields) {
    fl.append(h('div', {
      class: 'frow', title: '点开可以改。改过之后模型本轮不能再动它。',
      onclick: () => editField(path, f),
    },
      h('span', { class: 'p' }, path),
      h('span', { class: 'v' }, typeof f.value === 'string' ? f.value : JSON.stringify(f.value)),
      h('span', { class: 'm' }, f.prov?.origin === 'User' ? '你' : (isGuess(f.prov) ? '猜' : '有据'))));
  }

  // 待落定 / 已搁置：**逐条可删，也能一键挪到另一边**。
  // 这两个清单是会一直长的东西，只给一个「编辑」按钮去改一整块文本，
  // 意味着删一条要先读懂整块 —— 那就没人删了，于是它们只增不减。
  for (const [id, key] of [['#open-list', 'open'], ['#parked-list', 'parked']]) {
    const box = $(id); box.textContent = '';
    const items = ws?.[key] || [];
    const other = key === 'open' ? 'parked' : 'open';
    if (!items.length) box.append(h('div', { class: 'empty' }, '（空）'));
    items.forEach((q, i) => {
      box.append(h('div', { class: 'qrow' },
        h('span', { class: 't' }, q),
        h('button', {
          class: 'icon', title: key === 'open' ? '先搁置' : '挪回待落定',
          onclick: () => moveItem(key, other, i),
        }, key === 'open' ? '⇩' : '⇧'),
        h('button', {
          class: 'icon', title: '删掉这条',
          onclick: () => dropItem(key, i),
        }, '✕')));
    });
    box.append(h('button', {
      style: 'margin-top:6px;font-size:12px',
      onclick: () => editList(key, items),
    }, '整块编辑'));
  }
}

/** 删掉待落定/已搁置里的一条。
 *
 * `Op::Set{open|parked}` 是**整份替换**，所以这里发的是删掉之后剩下的那一份。
 * 走 `edit` 而不是别的口子：这是用户的改动，要占仲裁键，模型这一轮不能覆盖它。 */
function dropItem(key, i) {
  const items = [...(S.snap?.ws?.[key] || [])];
  items.splice(i, 1);
  send('edit', { ops: [{ op: 'set', path: key, [key]: items }] });
}

/** 待落定 ⇄ 已搁置。两边都是整份替换，所以一次发两条 op。 */
function moveItem(from, to, i) {
  const a = [...(S.snap?.ws?.[from] || [])];
  const b = [...(S.snap?.ws?.[to] || [])];
  const [moved] = a.splice(i, 1);
  if (moved === undefined) return;
  b.push(moved);
  send('edit', { ops: [
    { op: 'set', path: from, [from]: a },
    { op: 'set', path: to, [to]: b },
  ] });
}

/** 分层布局 + SVG。**故意不引 mermaid**：节点要能点选编辑，就得是我们自己画的。 */
function drawGraph(g) {
  const box = $('#graph'); box.textContent = '';
  const nodes = g?.nodes || {}, edges = g?.edges || {};
  const ids = Object.keys(nodes);
  $('.legend').hidden = !ids.length;
  if (!ids.length) {
    box.append(h('div', { class: 'empty' }, g?.sketch ? '模型画的图见下方源码' : '还没有推断图'));
    return;
  }
  const groupOf = {}, isGroup = {};
  for (const id of ids) { const p = nodes[id].parent; if (p) { isGroup[p] = true; groupOf[id] = p; } }
  const flat = ids.filter(id => !isGroup[id]);

  // 层号 = 从任一根出发的最长路径。有环也不会死循环（跑固定轮数）。
  const layer = {}; flat.forEach(id => layer[id] = 0);
  const es = Object.values(edges).filter(e => layer[e.from] !== undefined && layer[e.to] !== undefined);
  for (let i = 0; i < flat.length; i++) {
    let moved = false;
    for (const e of es) if (layer[e.to] < layer[e.from] + 1) { layer[e.to] = layer[e.from] + 1; moved = true; }
    if (!moved) break;
  }
  const rows = {};
  for (const id of flat) (rows[layer[id]] ||= []).push(id);
  Object.values(rows).forEach(r => r.sort());

  const W = 148, H = 38, GX = 22, GY = 34, PAD = 14;
  const pos = {};
  let maxCols = 1;
  for (const [L, r] of Object.entries(rows)) {
    maxCols = Math.max(maxCols, r.length);
    r.forEach((id, i) => { pos[id] = { x: PAD + i * (W + GX), y: PAD + (+L) * (H + GY) }; });
  }
  const width = PAD * 2 + maxCols * W + (maxCols - 1) * GX;
  const height = PAD * 2 + H + Math.max(0, ...Object.keys(rows).map(Number)) * (H + GY);

  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
  svg.setAttribute('width', width); svg.setAttribute('height', height);
  const mk = (t, a, parent = svg) => {
    const e = document.createElementNS(NS, t);
    for (const [k, v] of Object.entries(a)) e.setAttribute(k, v);
    parent.append(e); return e;
  };

  // 分组：围住它的子节点
  for (const gid of Object.keys(isGroup)) {
    const kids = ids.filter(i => groupOf[i] === gid).map(i => pos[i]).filter(Boolean);
    if (!kids.length) continue;
    const x0 = Math.min(...kids.map(p => p.x)) - 8, y0 = Math.min(...kids.map(p => p.y)) - 18;
    const x1 = Math.max(...kids.map(p => p.x)) + W + 8, y1 = Math.max(...kids.map(p => p.y)) + H + 8;
    const gg = mk('g', { class: 'g' });
    mk('rect', { x: x0, y: y0, width: x1 - x0, height: y1 - y0, rx: 8 }, gg);
    const t = mk('text', { x: x0 + 6, y: y0 + 12 }, gg);
    t.textContent = nodes[gid].label || gid;
  }

  const anchor = (id) => {
    if (pos[id]) return { cx: pos[id].x + W / 2, top: pos[id].y, bot: pos[id].y + H };
    const kids = ids.filter(i => groupOf[i] === id).map(i => pos[i]).filter(Boolean);
    if (!kids.length) return null;
    const x0 = Math.min(...kids.map(p => p.x)), x1 = Math.max(...kids.map(p => p.x)) + W;
    const y0 = Math.min(...kids.map(p => p.y)) - 18, y1 = Math.max(...kids.map(p => p.y)) + H;
    return { cx: (x0 + x1) / 2, top: y0, bot: y1 };
  };

  for (const e of Object.values(edges)) {
    const a = anchor(e.from), b = anchor(e.to);
    if (!a || !b) continue;
    const dy = Math.max(12, (b.top - a.bot) / 2);
    const d = `M${a.cx},${a.bot} C${a.cx},${a.bot + dy} ${b.cx},${b.top - dy} ${b.cx},${b.top}`;
    mk('path', { d, class: 'e' + (isGuess(e.prov) ? ' guess' : '') });
    if (e.label) {
      const t = mk('text', { x: (a.cx + b.cx) / 2 + 4, y: (a.bot + b.top) / 2, class: 'elabel' });
      t.textContent = e.label;
    }
  }

  for (const id of flat) {
    const n = nodes[id], p = pos[id];
    const cls = 'n' + (isGuess(n.prov) ? ' guess' : '') +
      (n.prov?.origin === 'User' ? ' user' : '') + (S.fresh.has(id) ? ' fresh' : '');
    const gg = mk('g', { class: cls });
    gg.addEventListener('click', () => editNode(id, n));
    const title = document.createElementNS(NS, 'title');
    title.textContent = `${id} · ${n.kind}\n${n.body || ''}`;
    gg.append(title);
    mk('rect', { x: p.x, y: p.y, width: W, height: H, rx: 6 }, gg);
    const label = (n.label || id);
    const t1 = mk('text', { x: p.x + W / 2, y: p.y + (n.kind ? 16 : 23), 'text-anchor': 'middle' }, gg);
    t1.textContent = label.length > 20 ? label.slice(0, 19) + '…' : label;
    if (n.kind) {
      const t2 = mk('text', { x: p.x + W / 2, y: p.y + 29, 'text-anchor': 'middle', class: 'elabel' }, gg);
      t2.textContent = n.kind;
    }
  }
  box.append(svg);
}

// ───────────────────────── 编辑弹层 ─────────────────────────

function modal(title, bodyNodes, onOk, okLabel = '应用') {
  const m = $('#modal'), body = $('.sheet-body', m);
  body.textContent = '';
  body.append(h('h3', {}, title), ...bodyNodes,
    h('div', { class: 'acts', style: 'display:flex;gap:8px;margin-top:14px' },
      h('button', { class: 'primary', onclick: () => { onOk(); close(); } }, okLabel),
      h('button', { onclick: close }, '取消')));
  m.hidden = false;
  function close() { m.hidden = true; }
  m.onclick = (e) => { if (e.target === m) close(); };
}

function editNode(id, n) {
  const label = h('input', { value: n.label || '' });
  const kind = h('input', { value: n.kind || '' });
  const body = h('textarea', { rows: 4 }); body.value = n.body || '';
  modal(`节点 ${id}`, [
    h('div', { class: 'field' }, h('label', {}, '标题'), label),
    h('div', { class: 'field' }, h('label', {}, '类型（形状由 memory/prompts.toml 的 graph.shape 决定）'), kind),
    h('div', { class: 'field' }, h('label', {}, '细节'), body),
    h('p', { class: 'hint' }, '改完之后，模型在本轮里不能再动这个节点。'),
    h('button', {
      class: 'danger', style: 'margin-top:4px',
      onclick: () => { send('edit', { ops: [{ op: 'drop', id: { node: id } }] }); $('#modal').hidden = true; },
    }, '删除这个节点'),
  ], () => send('edit', {
    ops: [{ op: 'node', id, label: label.value, kind: kind.value, body: body.value }],
  }));
}

function editField(path, f) {
  const v = h('input', { value: typeof f.value === 'string' ? f.value : JSON.stringify(f.value) });
  modal(`推断 ${path}`, [
    h('div', { class: 'field' }, h('label', {}, '值'), v),
    h('p', { class: 'hint' }, `来源：${f.prov?.origin === 'User' ? '你填的' : '模型推断'}` +
      (f.prov?.seq ? ` · 第 #${f.prov.seq} 条事件` : '')),
    h('button', {
      class: 'danger', style: 'margin-top:4px',
      onclick: () => { send('edit', { ops: [{ op: 'remove', path }] }); $('#modal').hidden = true; },
    }, '删掉这条'),
  ], () => {
    let val = v.value;
    try { val = JSON.parse(v.value); } catch { /* 不是 JSON 就当字符串 */ }
    send('edit', { ops: [{ op: 'set', path, value: val }] });
  });
}

function editList(key, items) {
  const t = h('textarea', { rows: 8 }); t.value = items.join('\n');
  modal(key === 'open' ? '待落定' : '已搁置', [
    h('p', { class: 'hint' }, '一行一条。'), t,
  ], () => {
    const lines = t.value.split('\n').map(s => s.trim()).filter(Boolean);
    send('edit', { ops: [{ op: 'set', path: key, value: null, [key]: lines }] });
  });
}

// ───────────────────────── 左栏 ─────────────────────────

/** 本会话有内容了就把侧栏里那条的标题补上。 */
function titleSelf() {
  const me = S.sessions.find(x => x.id === S.session);
  if (!me || me.title) return;
  const first = S.timeline.find(e => e.kind === 'said');
  if (!first) return;
  const t = (first.body.text || '').split(/\s+/).join(' ');
  me.title = t.length > 24 ? t.slice(0, 24) + '…' : t;
  renderSessions();
}

function renderSessions() {
  const ul = $('#session-list'); ul.textContent = '';
  if (!S.sessions.length) ul.append(h('li', { class: 'hint' }, '还没有对话'));
  for (const s of S.sessions) {
    const me = s.id === S.session;
    const name = s.title || (s.parent ? '空分支' : '空对话');
    ul.append(h('li', {
      class: (me ? 'on' : '') + (s.parent ? ' child' : ''),
      onclick: () => send('open', { session: s.id }),
    },
      h('span', { class: 'nm' }, name),
      h('span', { class: 'sub' }, s.parent ? '⑂ ' + s.id.slice(0, 6) : s.id.slice(0, 6)),
      h('span', { class: 'ops' },
        h('button', {
          class: 'icon', title: '改名',
          onclick: (e) => {
            e.stopPropagation();
            const t = prompt('这段对话叫什么？', name);
            if (t !== null) send('session_rename', { session: s.id, title: t.trim() });
          },
        }, '✎'),
        h('button', {
          class: 'icon',
          // 当前这条不给删：删了之后界面还挂在一个列表里已经没有的会话上。
          title: me ? '当前对话不能删，先切到别的' : '从列表里删掉（对话内容留在库里，不会连累从它分出去的分支）',
          disabled: me,
          onclick: (e) => {
            e.stopPropagation();
            if (confirm(`把「${name}」从列表里删掉？内容仍在库里。`)) {
              send('session_del', { session: s.id });
            }
          },
        }, '✕'))));
  }
}



function switchRight(which) {
  document.body.classList.remove('no-right');
  document.body.classList.add('want-right');
  $$('#right-seg button').forEach(b => b.classList.toggle('on', b.dataset.rt === which));
  $$('#right .pane').forEach(p => p.hidden = p.dataset.rp !== which);
}

// ───────────────────────── 编辑遮罩 ─────────────────────────
//
// 改文件、改场景、审阅蒸馏草稿，都开这一层。
//
// # 为什么是遮罩不是常驻的一栏
//
// 这三件事都是**编辑的时候才需要一下**的。给它们留一整栏，平时那栏是空的，
// 真要用的时候又嫌窄（一篇 prompts.toml 在侧栏里根本读不下来）。
// 遮罩铺得开、关掉就没了，而且三种编辑共用同一套外框 —— 标题、脏标记、
// 关闭前拦一下未保存改动，只写一遍。
//
// `S.over` 是**唯一**的打开状态：{ kind, title, note, dirty(), save(), del(), body }。
// 各种编辑只提供内容和动作，开关、脏检查、Esc、点空白关闭都在这里。

function openOver(o) {
  S.over = o;
  $('#ov-title').textContent = o.title;
  $('#ov-note').textContent = o.note || '';
  $('#ov-note').hidden = !o.note;
  const body = $('#ov-body'); body.textContent = '';
  body.append(...o.body);
  const acts = $('#ov-acts'); acts.textContent = '';
  for (const a of o.acts || []) acts.append(a);
  $('#over').hidden = false;
  overDirty();
  o.focus?.();
}

function overDirty() {
  const o = S.over;
  $('#ov-dirty').hidden = !o?.dirty?.();
}

function closeOver(force) {
  const o = S.over;
  if (!force && o?.dirty?.() && !confirm('有没保存的改动，确定关掉？')) return;
  S.over = null;
  $('#over').hidden = true;
}

/** 遮罩里的一个操作按钮。 */
function act(label, fn, cls) {
  return h('button', { class: cls || '', onclick: fn }, label);
}

// ── 文件编辑 ──

/** 打开一个持久层文件或 config.json。`kind` 是 'memory' 或 'config'。 */
function openEditor(kind, file) {
  let text = '', note = '', deletable = false, title = file;
  if (kind === 'config') {
    // 用 boot 时存下的那份，不是 S.settings —— 后者会被左栏表单的草稿改动污染，
    // 而这里明写着「盘上那份」，说了就得是真的。
    text = JSON.stringify(S.saved ?? S.settings, null, 2);
    note = '这是盘上那份 config.json。左栏表单里还没保存的改动不在里面。保存会重启当前会话（历史不丢）。';
    title = 'config.json';
  } else {
    const f = S.memory.find(x => x.file === file);
    if (!f) return;
    text = f.text;
    note = (MEM_NAME[file] ? MEM_NAME[file][1] : null) || (file.startsWith('draft-')
      ? '蒸馏草稿，还没应用。审阅后把要留的内容并进对应的记忆文件。'
      : '一条参考案例。命中对应场景时注入回答段。');
    deletable = file.startsWith('cases/') || file.startsWith('draft-');
    title = (MEM_NAME[file]?.[0] || file) + '　' + file;
  }
  const ta = h('textarea', { id: 'ov-text', spellcheck: 'false' });
  ta.value = text;
  ta.addEventListener('input', overDirty);
  ta.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === 's') { e.preventDefault(); saveEditor(); }
  });
  // 存元素本身，不是等会儿再按 id 去 DOM 里捞：这个 textarea 是我们刚建的，
  // 再查一次只会多一处可能查不到的地方。
  // orig 取**元素读回来的那份**，不是我们塞进去的那份：textarea 会把 CRLF
  // 归一成 LF，直接拿原文比的话，一个 Windows 换行的文件刚打开就是「未保存」。
  const st = { kind, file: file || 'config.json', orig: ta.value, el: ta };
  S.editor = st;
  openOver({
    kind: 'file', title, note,
    body: [ta],
    dirty: () => ta.value !== st.orig,
    acts: [
      act('保存', saveEditor, 'primary'),
      act('还原', () => { ta.value = st.orig; overDirty(); }),
      deletable ? act('删除', () => {
        if (!confirm(`删掉 ${st.file}？`)) return;
        send('memory_del', { file: st.file });
        closeOver(true);
      }, 'danger') : null,
    ].filter(Boolean),
    // 光标放开头：focus 默认落到末尾，一打开就滚到文件底部，看到的是最没用的那段
    focus: () => { ta.focus(); ta.setSelectionRange(0, 0); ta.scrollTop = 0; },
  });
  renderMemory();
}

function saveEditor() {
  const st = S.editor; if (!st) return;
  const text = st.el.value;
  if (st.kind === 'config') {
    let next;
    try { next = JSON.parse(text); }
    catch (err) { banner('bad', 'config.json 格式不对，没保存：' + err.message, 'ed'); return; }
    send('settings_put', { settings: next });
  } else {
    send('memory_put', { file: st.file, text });
  }
  st.orig = text;
  overDirty();
  banner('info', `已保存 ${st.file}`, 'ed');
}

// ── 场景编辑 ──
//
// 场景是**独立的块**，不是 playbook.toml 里的一段文本。用户改一个场景的措辞时，
// 该看到的是「这个场景什么时候触发、命中之后跟模型说什么」，不是一整份 TOML。

function openScene(sc) {
  const isNew = !sc;
  const cur = sc || { id: '', label: '', when: '', guidance: '', tools: [] };
  const orig = JSON.stringify(cur);
  const f = (label, key, rows, hint) => {
    const el = rows
      ? h('textarea', { rows: String(rows), spellcheck: 'false' })
      : h('input', { spellcheck: 'false' });
    el.value = Array.isArray(cur[key]) ? cur[key].join(', ') : (cur[key] || '');
    el.addEventListener('input', () => {
      cur[key] = key === 'tools'
        ? el.value.split(/[,，\s]+/).filter(Boolean)
        : el.value;
      overDirty();
    });
    return h('div', { class: 'field' },
      h('label', {}, label), el, hint ? h('p', { class: 'hint' }, hint) : null);
  };
  openOver({
    kind: 'scene',
    title: isNew ? '新场景' : `场景 · ${cur.label || cur.id}`,
    note: '判断段只看 id 和「什么时候」；命中之后注入回答段的是「跟模型说什么」。',
    body: [
      f('id', 'id', 0, isNew ? '英文小写，字母数字和 - _。定了就别改，历史事件里存的是它。' : null),
      f('显示名', 'label'),
      f('什么时候判成它', 'when', 2, '这一行会进判断段的场景目录。写得越具体，判得越准。'),
      f('命中之后跟模型说什么', 'guidance', 8,
        '措辞是给模型的建议与约束，不是给程序的指令。「建议先把这条链路讲通」是对的；「必须调用三次」不是。'),
      f('额外暴露的工具', 'tools', 0, '逗号分隔。留空 = 只给默认工具集。名字必须是下面「工具」页里真有的。'),
    ],
    dirty: () => JSON.stringify(cur) !== orig,
    acts: [
      act('保存', () => {
        if (!cur.id.trim()) return banner('bad', '场景要有 id', 'scene');
        send('scene_put', { scene: cur });
        closeOver(true);
      }, 'primary'),
      !isNew && cur.id !== 'none' ? act('删除', () => {
        if (!confirm(`删掉场景 ${cur.label || cur.id}？`)) return;
        send('scene_del', { scene: { id: cur.id } });
        closeOver(true);
      }, 'danger') : null,
    ].filter(Boolean),
  });
}

/** 下一轮的场景选择器。**多选** —— 判断段本来就可以一次判出好几个，
 * 用户插手时没道理只准挑一个。勾中的每一个，它的 guidance 下一轮都会真的注入。 */
function openScenePicker() {
  if (!S.playbook.length) return;
  const picked = new Set(S.scenes);
  const list = h('div', { class: 'sc-pick' });
  for (const sc of S.playbook) {
    const cb = h('input', { type: 'checkbox' });
    cb.checked = picked.has(sc.id);
    cb.addEventListener('change', () => cb.checked ? picked.add(sc.id) : picked.delete(sc.id));
    list.append(h('label', { class: 'sc-row' },
      cb,
      h('span', { class: 'n' }, sc.label || sc.id),
      h('span', { class: 'w' }, sc.when)));
  }
  modal('下一轮的场景（可多选）', [
    list,
    h('p', { class: 'hint', style: 'margin-top:8px' },
      '选中的场景，guidance 与参考案例下一轮会一起注入，工具取并集。'
      + '一个都不选 = 交回给判断段自己判。最多注入 3 个。'),
  ], () => send('scene', { to: [...picked] }), '用这些');
}

// ── 蒸馏审阅 ──
//
// 模型按目标文件名分节输出，这里逐节预览 / 修改 / 勾选，一次写回。
// **不做成「给你个草稿路径，自己去改」** —— 判断哪一段属于哪个文件、
// 复制粘贴进去，这一步本来就该程序做，那才叫一键。

const DI_MODE = {
  replace: ['整份替换', 'rep', '会覆盖这个文件现在的全部内容'],
  append: ['追加', 'app', '接在文件末尾，原有内容不动'],
};

function openDistill() {
  const d = S.distill; if (!d) return;
  const box = h('div', { id: 'di-list' });
  for (const s of d.sections) {
    const [label, cls, why] = DI_MODE[s.mode] || DI_MODE.replace;
    const ta = h('textarea', { spellcheck: 'false' });
    ta.value = s.text;
    ta.addEventListener('input', () => { s.text = ta.value; });
    const cb = h('input', { type: 'checkbox' });
    cb.checked = s.on !== false;
    const row = h('div', { class: 'dsec' + (s.on === false ? ' off' : '') });
    cb.addEventListener('change', () => {
      s.on = cb.checked;
      row.className = 'dsec' + (cb.checked ? '' : ' off');
    });
    row.append(
      h('label', { class: 'dsec-head' },
        cb, h('span', { class: 'f' }, s.file),
        h('span', { class: 'tag ' + cls, title: why }, label)),
      ta);
    box.append(row);
  }
  openOver({
    kind: 'distill',
    title: '蒸馏草稿',
    note: d.error || '模型写的草稿，还没生效。改完把要留的勾上，一次写进对应文件。'
      + (d.path ? `　全文：${d.path}` : ''),
    body: [box],
    dirty: () => false,
    acts: [act('写入选中的', applyDistill, 'primary')],
  });
}

function applyDistill() {
  const d = S.distill; if (!d) return;
  const picked = d.sections.filter(s => s.on !== false && s.text.trim());
  if (!picked.length) return banner('bad', '一节都没勾，没有可写的', 'distill');
  send('distill_apply', { sections: picked.map(s => ({ file: s.file, mode: s.mode, text: s.text })) });
}


// ───────────────────────── 能力协商失败 ─────────────────────────
//
// 判断段拿不到强制工具调用时，场景判定整个失效 —— 而这件事在对话里
// 一点痕迹都没有：模型照样回答，只是再也没有 guidance 和案例注入。
// 所以它必须是**一屏说清楚**的东西，而不是一行红字。
//
// 报告的顺序是有讲究的：先说这个角色需要什么能力、缺了会少什么，
// 再说试了哪几种发法、厂商原话是什么，最后给一个能立刻动手的出口。
// 反过来（先甩一堆 400）用户只会知道「坏了」，不知道该改什么。

function openCaps() {
  const c = S.caps; if (!c) return;
  const need = h('div', { class: 'caps-need' });
  for (const r of c.required || []) {
    need.append(h('div', { class: 'caps-row' },
      h('code', { class: 'cc' }, r.code),
      h('div', {}, h('b', {}, r.label), h('p', { class: 'hint' }, r.need))));
  }
  const steps = h('ol', { class: 'caps-steps' });
  for (const s of c.steps || []) {
    steps.append(h('li', { class: 'st-' + s.outcome },
      h('code', { class: 'cc' }, s.code),
      h('span', { class: 'tr' }, s.tried),
      h('span', { class: 'oc' }, { ok: '成了', rejected: '被拒', exhausted: '没招了' }[s.outcome] || s.outcome),
      s.detail ? h('pre', { class: 'raw' }, s.detail) : null));
  }
  openOver({
    kind: 'caps',
    title: `${c.role}用不了这个模型`,
    note: c.verdict,
    body: [
      h('h4', {}, `${c.provider} · ${c.model}`),
      h('p', { class: 'hint' }, '这个角色需要下面这些能力：'),
      need,
      h('p', { class: 'hint' }, '按这个顺序换过发法（降级的是怎么发，不是要什么）：'),
      steps,
    ],
    dirty: () => false,
    acts: [
      // 出口一：直接跳到这个角色的配置，展开、滚过去。
      act('去改这个角色', () => {
        S.focusRole = { judge: 'judge', 判断段: 'judge', 回答段: 'answer', 子任务: 'subagent' }[c.role] || 'judge';
        closeOver(true);
        openTab('config');
        renderConfig();
      }, 'primary'),
      // 出口二：整份日志拷走，去搜、去问厂商客服。原话不改写就是为了能搜到。
      act('复制日志', async () => {
        try { await navigator.clipboard.writeText(c.text); banner('info', '日志已复制', 'caps-copy'); }
        catch { banner('bad', '复制失败，手动选中吧', 'caps-copy'); }
      }),
    ],
  });
}

// ───────────────────────── 目录浏览器 ─────────────────────────
//
// **它不走 Policy**，因为它的用途正是「挑一个还没授权的目录加进白名单」。
// 边界在别处：服务只绑本地回环，而且这是 server 的指令不是工具 —— 模型碰不到，
// 模型读文件仍然只能走 toolkit 并且必须过闸门。

let pickCb = null;

function openBrowser(start, cb) {
  pickCb = cb;
  S.browse = null;
  send('browse', { path: start || '' });
}

function renderBrowser() {
  const b = S.browse; if (!b) return;
  const manual = h('input', { value: b.path, spellcheck: 'false',
    onkeydown: (e) => { if (e.key === 'Enter') send('browse', { path: e.target.value }); } });
  const list = h('div', { class: 'br-list' });
  if (b.parent) {
    list.append(h('div', { class: 'br-row d', onclick: () => send('browse', { path: b.parent }) },
      h('span', { class: 'ic' }, '↰'), h('span', {}, '上一级')));
  }
  for (const it of b.entries) {
    const row = h('div', {
      class: 'br-row' + (it.dir ? ' d' : ''),
      onclick: () => { if (it.dir) send('browse', { path: join(b.path, it.name) }); },
    },
      h('span', { class: 'ic' }, it.dir ? '▸' : '·'),
      h('span', {}, it.name),
      h('button', {
        class: 'pick',
        onclick: (e) => { e.stopPropagation(); pick(join(b.path, it.name)); },
      }, '选它'));
    list.append(row);
  }
  const cuts = h('div', { class: 'br-cuts' });
  for (const c of b.shortcuts || []) {
    cuts.append(h('button', { onclick: () => send('browse', { path: c.path }) }, c.label));
  }
  modal('选一个目录或文件', [
    cuts,
    h('div', { class: 'field' }, h('label', {}, '当前位置（可直接输入路径后回车）'), manual),
    list,
    h('p', { class: 'hint', style: 'margin-top:8px' },
      '选目录 = 这个目录下的东西模型都能读；选单个文件 = 只放行那一个。'),
  ], () => pick(b.path), '选当前目录');
}

function pick(p) {
  $('#modal').hidden = true;
  const cb = pickCb; pickCb = null;
  if (cb) cb(p);
}

function join(base, name) {
  const sep = base.includes('\\') && !base.includes('/') ? '\\' : '/';
  return base.endsWith(sep) ? base + name : base + sep + name;
}

function renderConfig() {
  const box = $('#config-form'); box.textContent = '';
  const st = S.settings; if (!st) return;
  // 加/删 provider 要带着改了一半的草稿重画，所以草稿要跨一次重画活下来。
  // **但它不写回 S.settings** —— S.settings 是「服务端说盘上是什么」，
  // 让一份没保存的草稿冒充它，任何别处一发送就会把没保存的东西一起写出去。
  const draft = cfgDraft || JSON.parse(JSON.stringify(st));
  cfgDraft = null;

  const num = (obj, k, label) => {
    const i = h('input', { type: 'number', value: obj[k], oninput: e => obj[k] = +e.target.value });
    return h('div', { class: 'field' }, h('label', {}, label), i);
  };
  const txt = (obj, k, label) => {
    const i = h('input', { value: obj[k], oninput: e => obj[k] = e.target.value });
    return h('div', { class: 'field' }, h('label', {}, label), i);
  };
  const sel = (obj, k, label, opts) => {
    const s = h('select', { onchange: e => obj[k] = e.target.value });
    for (const o of opts) s.append(h('option', { value: o, selected: obj[k] === o }, o));
    return h('div', { class: 'field' }, h('label', {}, label), s);
  };
  /** 路径列表：一行一个 + 浏览添加。比让人往小框里手打路径靠谱得多。 */
  const pathList = (obj, k, label) => {
    const rows = h('div', {});
    const redraw = () => {
      rows.textContent = '';
      (obj[k] || []).forEach((v, i) => {
        rows.append(h('div', { class: 'prow' },
          h('span', { title: v }, v),
          h('button', {
            class: 'icon', title: '移除',
            onclick: () => { obj[k].splice(i, 1); redraw(); },
          }, '✕')));
      });
      rows.append(h('div', { class: 'pact' },
        h('button', { onclick: () => openBrowser(obj[k]?.[0] || '', p => { (obj[k] ||= []).push(p); redraw(); }) },
          '浏览添加…')));
    };
    redraw();
    return h('div', { class: 'field' }, h('label', {}, label), rows);
  };
  const lines = (obj, k, label) => {
    const t = h('textarea', { rows: 3, oninput: e => obj[k] = e.target.value.split('\n').map(s => s.trim()).filter(Boolean) });
    t.value = (obj[k] || []).join('\n');
    return h('div', { class: 'field' }, h('label', {}, label), t);
  };
  const block = (title, tag, kids, open = false, tagOn = false) => {
    const d = h('details', { class: 'block', open });
    d.append(h('summary', {}, title,
      tag ? h('span', { class: 'tag' + (tagOn ? ' on' : '') }, tag) : null));
    d.append(h('div', { class: 'body' }, ...kids));
    return d;
  };

  const provs = Object.keys(draft.providers);
  const presetOf = (name) => S.presets.find(p => p.name === name);
  /** 模型名给候选但不锁死 —— 厂商加新模型比这个列表更新快，写死会拦住人。 */
  const modelInput = (r) => {
    const id = 'ml-' + r.provider;
    const dl = h('datalist', { id });
    for (const m of presetOf(r.provider)?.models || []) dl.append(h('option', { value: m }));
    const i = h('input', { value: r.model, list: id, oninput: e => r.model = e.target.value });
    return h('div', { class: 'field' }, h('label', {}, '模型名'), i, dl);
  };
  for (const [role, cn] of [['judge', '判断段'], ['answer', '回答段'], ['subagent', '子任务']]) {
    const r = draft.roles[role];
    // 协商失败的那个角色自动展开并标红，用户点「去改这个角色」就落在这里。
    const broken = S.caps && S.caps.role === cn;
    const b = block(`${cn} · ${role}`, r.model, [
      broken ? h('p', { class: 'note bad', style: 'margin:0 0 8px' },
        `${S.caps.verdict}　`,
        h('button', { class: 'ghost', onclick: (e) => { e.preventDefault(); openCaps(); } }, '看完整日志')) : null,
      sel(r, 'provider', 'provider', provs), modelInput(r),
      h('div', { class: 'two' }, num(r, 'temperature', '温度'), num(r, 'max_tokens', 'max tokens')),
    ].filter(Boolean));
    if (broken || S.focusRole === role) {
      b.open = true;
      b.className = 'block bad';
      if (S.focusRole === role) { S.focusRole = null; setTimeout(() => b.scrollIntoView?.({ block: 'center' }), 0); }
    }
    box.append(b);
  }

  for (const name of provs) {
    const p = draft.providers[name];
    const st_ = S.keys[name] || {};
    const keyIn = h('input', { type: 'password', placeholder: st_.has ? '已有密钥（留空保持不变）' : '粘贴密钥…' });
    box.append(block(`provider · ${name}`, st_.has ? '密钥已存' : '缺密钥', [
      sel(p, 'api', 'API 协议', ['anthropic', 'open_ai_compat']),
      txt(p, 'base_url', 'base_url'), txt(p, 'key_env', '密钥的环境变量名'),
      h('div', { class: 'field' }, h('label', {}, `密钥（存进 secrets.json，0600，不进 config.json）`), keyIn),
      h('div', { class: 'acts' },
        h('button', {
          onclick: () => { if (keyIn.value.trim()) send('secret_put', { provider: name, key: keyIn.value }); keyIn.value = ''; },
        }, '存入'),
        st_.has ? h('button', { class: 'danger', onclick: () => send('secret_put', { provider: name, key: '' }) }, '清除') : null),
      h('p', { class: 'hint' }, `环境变量 ${st_.env} 优先级高于这里。`),
      // 被某个角色用着的 provider 不给删 —— 删了之后症状是「模型没反应」，
      // 而错误信息要到下一次起会话才出来。
      (() => {
        const used = Object.values(draft.roles).filter(r => r.provider === name).length;
        return h('div', { class: 'acts' }, h('button', {
          class: 'danger', disabled: used > 0,
          title: used ? '有角色正在用它，先把那个角色改到别的 provider' : '从花名册里移除',
          onclick: () => { delete draft.providers[name]; renderConfigFrom(draft); },
        }, '移除这个 provider'));
      })(),
    ]));
  }

  // ＋ 添加 provider：预设来自后端，base_url 与密钥变量名只有那一份
  {
    const psel = h('select', {});
    let firstPreset = '';
    for (const p of S.presets) {
      if (!draft.providers[p.name] || p.name === 'custom') {
        psel.append(h('option', { value: p.name }, `${p.label}（${p.name}）`));
        firstPreset ||= p.name;
      }
    }
    psel.value = firstPreset;
    const nameIn = h('input', { placeholder: '花名册里的名字，留空用预设名' });
    box.append(block('＋ 添加 provider', S.presets.length ? `${S.presets.length} 个预设` : '', [
      h('div', { class: 'field' }, h('label', {}, '厂商'), psel),
      h('div', { class: 'field' }, h('label', {}, '名字'), nameIn),
      h('p', { class: 'hint' }, 'DeepSeek / 智谱 GLM / 月之暗面都是 OpenAI 兼容协议，选了就自动带上地址和密钥变量名。自建 vLLM、Ollama 选「自定义」再改地址。'),
      h('div', { class: 'acts' }, h('button', {
        class: 'primary',
        onclick: () => {
          const pre = S.presets.find(x => x.name === psel.value);
          if (!pre) return;
          const key = (nameIn.value.trim() || pre.name).replace(/[^A-Za-z0-9_-]/g, '');
          if (!key) return banner('bad', '名字只能用字母数字和 - _', 'prov');
          if (draft.providers[key]) return banner('bad', `已经有一个叫 ${key} 的了`, 'prov');
          draft.providers[key] = { api: pre.api, base_url: pre.base_url, key_env: pre.key_env };
          renderConfigFrom(draft);
        },
      }, '加进花名册')),
    ]));
  }

  const t = draft.tools;
  box.append(block('工具权限与上限', t.net ? '抓取开' : '抓取关', [
    h('div', { class: 'kv' },
      h('b', {}, '网页抓取'),
      h('input', { type: 'checkbox', checked: t.net, style: 'width:auto', onchange: e => t.net = e.target.checked })),
    pathList(t, 'roots', '可读目录 / 文件'),
    lines(t, 'allow_hosts', '可抓域名（后缀匹配，* 表示不限）'),
    lines(t, 'deny_hosts', '禁止抓取的域名（优先于允许列表）'),
    lines(t, 'deny_names', '永不读取的名字'),
    lines(t, 'exec_allow', '工具后端可启动的程序名'),
    h('div', { class: 'two' }, num(t, 'max_bytes', '单次字节上限'), num(t, 'max_lines', '行数上限')),
    h('div', { class: 'two' }, num(t, 'max_matches', '检索条数'), num(t, 'max_depth', '遍历深度')),
    h('div', { class: 'two' }, num(t, 'max_line_len', '单行长度'), num(t, 'max_entries', '文件树条目上限')),
    num(t, 'max_file_bytes', '单文件读取字节上限'),
  ]));

  box.append(h('button', {
    class: 'primary wide', style: 'margin-top:10px',
    onclick: () => send('settings_put', { settings: draft }),
  }, '保存并重启会话'));
  box.append(h('button', {
    class: 'wide', style: 'margin-top:6px',
    onclick: () => openEditor('config'),
    title: '整份 config.json，在右栏改',
  }, '✎ 直接改 config.json'));
  box.append(h('p', { class: 'hint', style: 'margin-top:8px' },
    '改动会重启当前会话（历史不丢，从库里恢复）。'));
}

/** 跨一次重画传递的配置草稿。只有 `renderConfig` 读它，读完就清掉。 */
let cfgDraft = null;

/** 带着一份改过的 draft 重画配置面板。加/删 provider 之后要用它刷新。 */
function renderConfigFrom(draft) {
  cfgDraft = draft;
  renderConfig();
}

/** 文件名 → [给人看的名字, 一句话说明]。
 *
 * 直接列 `prompts.toml` 这种文件名，等于要求用户先知道每个文件装什么。
 * 列「系统提示词」他一眼就知道该点哪个。文件名仍然显示在旁边 ——
 * 它是真实存在的东西，藏起来只会让「我自己去改这个文件」变难。 */
const MEM_NAME = {
  'prompts.toml': ['系统提示词', '注入模型的全部提示词：角色定位、用户字段约束、折叠指令、两个 mode 各自的说明与工具、推断图的形状词表。'],
  'project.md': ['项目记忆', '项目概述、阶段目标、进展（成了的和没成的）。新对话靠它快速入手。'],
  'preferences.md': ['合作偏好', '你希望它怎么跟你配合：讲多细、怎么提问、什么时候该打断你。'],
  'knowledge.md': ['知识评估', '你在各知识域的掌握程度。它决定模型是直接问你，还是先把机制讲通。'],
  'playbook.toml': ['场景库（原文）', '下面那些场景块的底稿。一般改上面的块就够了，除非你要整份重排。'],
};

function renderMemory() {
  const box = $('#memory-list'); box.textContent = '';
  const openFile = S.over?.kind === 'file' ? S.editor?.file : null;
  const named = [];
  const rest = [];
  for (const f of S.memory) (MEM_NAME[f.file] ? named : rest).push(f);
  // 有名字的按 MEM_NAME 的顺序排：系统提示词在最前，它最常被改。
  named.sort((a, b) => Object.keys(MEM_NAME).indexOf(a.file) - Object.keys(MEM_NAME).indexOf(b.file));

  for (const f of named.concat(rest)) {
    const isDraft = f.file.startsWith('draft-');
    const isCase = f.file.startsWith('cases/');
    const name = MEM_NAME[f.file]?.[0]
      || (isDraft ? '蒸馏草稿' : isCase ? '参考案例' : f.file);
    // 一行一个入口，正文开遮罩改。侧栏里塞不下一篇 prompts.toml。
    box.append(h('div', {
      class: 'frow file' + (f.file === openFile ? ' on' : ''),
      onclick: () => openEditor('memory', f.file),
      title: (MEM_NAME[f.file]?.[1] || '') + '　' + f.file,
    },
      h('span', { class: 'p' }, name),
      h('span', { class: 'm mono' }, f.file),
      isDraft ? h('span', { class: 'm warn' }, '待审阅') : null,
      h('span', { class: 'm' }, `${f.text.split('\n').length} 行`)));
  }
  renderScenes();
}

/** 场景库：**一个场景一个块**，点开就改它自己的触发条件和 guidance。
 *
 * 用户要调的是「什么时候该提醒我固定种子」，不是「playbook.toml 第 47 行」。
 * 整份 TOML 仍然在上面那个「场景库（原文）」里，要整份重排还是走那条。 */
function renderScenes() {
  const box = $('#scene-list'); if (!box) return;
  box.textContent = '';
  const live = new Set(S.scenes);
  for (const sc of S.playbook) {
    box.append(h('div', {
      class: 'frow file' + (live.has(sc.id) ? ' on' : ''),
      title: sc.when,
      onclick: () => openScene(sc),
    },
      h('span', { class: 'p' }, sc.label || sc.id),
      h('span', { class: 'm mono' }, sc.id),
      live.has(sc.id) ? h('span', { class: 'm' }, '本轮命中') : null));
  }
  box.append(h('button', {
    class: 'wide', style: 'margin-top:6px',
    onclick: () => openScene(null),
  }, '＋ 新场景'));
  $('#scene-note').textContent =
    '以上提示词与场景多数是模型蒸馏出来的，随时可以点开查看和修改，改完下一轮就生效。'
    + (S.defaultTools.length ? `　每个场景都能用的工具：${S.defaultTools.join('、')}。` : '');
}

function renderTools() {
  const box = $('#tools-panel'); box.textContent = '';
  box.append(h('p', { class: 'hint' }, '模型这一轮能用的工具。全部只读 —— 没有写文件的工具，也没有 shell。'));
  for (const t of S.tools) {
    box.append(h('div', { class: 'kv' }, h('b', { class: 'mono' }, t)));
  }
  box.append(h('h4', { style: 'margin:14px 0 6px;font-size:11px;color:var(--muted)' }, '本会话用量'));
  box.append(h('div', { class: 'hint', style: 'font-family:var(--mono);font-size:11px' }, S.metrics || '（还没调用过）'));
  box.append(h('p', { class: 'hint', style: 'margin-top:12px' },
    'subagent 目前用在上下文折叠和一键蒸馏上。检索型 subagent 还没接 —— 要查资料，回答段自己调 fs_* / web_*。'));
}

function renderAll() {
  renderTop(); renderBanner(); renderStream(); renderRight();
  renderSessions(); renderConfig(); renderMemory(); renderTools(); renderAsk();
  const roots = S.settings?.tools?.roots || [];
  $('#root-path').value = roots[0] || '.';
}

// ───────────────────────── 查找 ─────────────────────────

let hits = [], cur = -1;

function clearHits() {
  for (const m of $$('mark.hit')) {
    const t = document.createTextNode(m.textContent);
    m.replaceWith(t); t.parentNode && t.parentNode.normalize();
  }
  hits = []; cur = -1;
}

function doFind(q) {
  clearHits();
  if (!q) { $('#find-count').textContent = '0/0'; return; }
  const needle = q.toLowerCase();
  const walker = document.createTreeWalker($('#stream'), NodeFilter.SHOW_TEXT);
  const targets = [];
  while (walker.nextNode()) {
    const n = walker.currentNode;
    if (n.nodeValue.toLowerCase().includes(needle)) targets.push(n);
  }
  for (const n of targets) {
    const parts = n.nodeValue.split(new RegExp(`(${q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')})`, 'ig'));
    const frag = document.createDocumentFragment();
    for (const p of parts) {
      if (p.toLowerCase() === needle) { const m = h('mark', { class: 'hit' }, p); frag.append(m); hits.push(m); }
      else if (p) frag.append(document.createTextNode(p));
    }
    n.replaceWith(frag);
  }
  cur = hits.length ? 0 : -1;
  focusHit();
}

function focusHit() {
  hits.forEach((m, i) => m.classList.toggle('cur', i === cur));
  $('#find-count').textContent = `${hits.length ? cur + 1 : 0}/${hits.length}`;
  if (cur >= 0) hits[cur].scrollIntoView({ block: 'center', behavior: 'smooth' });
}

function step(d) { if (!hits.length) return; cur = (cur + d + hits.length) % hits.length; focusHit(); }

// ───────────────────────── 交互接线 ─────────────────────────

function grip(el, cssVar, side) {
  el.addEventListener('mousedown', (e) => {
    e.preventDefault();
    const move = (ev) => {
      const w = side === 'left' ? ev.clientX : window.innerWidth - ev.clientX;
      document.documentElement.style.setProperty(cssVar, Math.max(180, Math.min(560, w)) + 'px');
    };
    const up = () => { removeEventListener('mousemove', move); removeEventListener('mouseup', up); };
    addEventListener('mousemove', move); addEventListener('mouseup', up);
  });
}

function openTab(name) {
  document.body.classList.remove('no-left');
  $('#left-show').hidden = true;
  $$('#left-tabs button').forEach(x => x.classList.toggle('on', x.dataset.tab === name));
  $$('#left .pane').forEach(p => p.hidden = p.dataset.pane !== name);
}

function boot() {
  grip($('#lgrip'), '--left', 'left');
  grip($('#rgrip'), '--right', 'right');

  $('#left-hide').onclick = () => { document.body.classList.add('no-left'); $('#left-show').hidden = false; };
  $('#left-show').onclick = () => { document.body.classList.remove('no-left'); $('#left-show').hidden = true; };
  $('#right-toggle').onclick = () => {
    document.body.classList.toggle('no-right');
    document.body.classList.add('want-right');
  };

  $$('#left-tabs button').forEach(b => b.onclick = () => openTab(b.dataset.tab));

  $$('#mode-seg button').forEach(b => b.onclick = () => send('mode', { to: b.dataset.mode }));
  $('#scene-chip').onclick = openScenePicker;
  $('#distill-btn').onclick = () => {
    banner('info', '正在蒸馏这段会话…完成后会弹出草稿供你逐节审阅', 'distill-run');
    send('distill');
  };
  $('#new-chat').onclick = () => send('open', { session: '' });
  $('#graph-refresh').onclick = () => send('snap');
  $('#stop').onclick = () => { if (S.turn !== null) send('interrupt', { turn: S.turn }); };

  $$('#right-seg button').forEach(b => b.onclick = () => switchRight(b.dataset.rt));
  $('#ov-close').onclick = () => closeOver();
  // 点遮罩的空白处关掉。判定用 e.target 是不是遮罩本身 —— 面板里的点击会冒泡上来，
  // 不这么判的话在里面选个字都会把面板关掉。
  $('#over').addEventListener('click', (e) => { if (e.target.id === 'over') closeOver(); });
  $('#root-browse').onclick = () => openBrowser($('#root-path').value, (p) => {
    $('#root-path').value = p;
  });
  $('#stream').addEventListener('scroll', updateToBottom, { passive: true });
  $('#to-bottom').onclick = () => {
    const st = $('#stream'); st.scrollTop = st.scrollHeight;
  };

  const input = $('#input');
  const grow = () => { input.style.height = 'auto'; input.style.height = Math.min(input.scrollHeight, window.innerHeight * 0.4) + 'px'; };
  input.addEventListener('input', grow);
  const fire = () => {
    const text = input.value;
    if (!text.trim()) return;
    // 轮次在跑的时候发出去 = 插话；Core 会决定排队还是打断
    if (!S.session || !send('send', { text, interrupt: false })) return;
    input.value = ''; grow(); input.focus();
  };
  $('#send').onclick = fire;
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); fire(); }
  });

  // 只发路径。**不发整份 settings** —— 浏览器手上这份是上一次 boot 的快照，
  // 拿它覆盖磁盘会把用户在别处改过的模型配置一起打回默认。
  // 空输入直接不发：原来 `|| '.'` 的兜底会把可读目录悄悄改成当前目录。
  $('#root-save').onclick = () => {
    const p = $('#root-path').value.trim();
    if (!p) return banner('bad', '目录是空的。点「浏览…」挑一个，或者直接把路径粘进去。', 'roots');
    send('roots_put', { path: p });
  };

  // 查找
  const openFind = () => { $('#find').hidden = false; $('#find-input').focus(); $('#find-input').select(); };
  $('#find-btn').onclick = openFind;
  $('#find-close').onclick = () => { $('#find').hidden = true; clearHits(); };
  $('#find-next').onclick = () => step(1);
  $('#find-prev').onclick = () => step(-1);
  let t = null;
  $('#find-input').addEventListener('input', (e) => {
    clearTimeout(t); const v = e.target.value; t = setTimeout(() => doFind(v), 120);
  });
  $('#find-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); step(e.shiftKey ? -1 : 1); }
    if (e.key === 'Escape') { $('#find').hidden = true; clearHits(); }
  });
  addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === 'f') { e.preventDefault(); openFind(); }
    if ((e.ctrlKey || e.metaKey) && e.key === 'b') { e.preventDefault(); $('#left-hide').click(); }
    if (e.key === 'Escape' && !$('#modal').hidden) { $('#modal').hidden = true; return; }
    if (e.key === 'Escape' && !$('#over').hidden) closeOver();
  });

  connect();
}

boot();
