// Runs the actual UI handlers with a small DOM double; no frontend dependencies.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

class Element {
  constructor(tag = 'div') {
    this.tag = tag; this.children = []; this.listeners = {}; this.attrs = {};
    this.value = ''; this.style = {}; this.dataset = {};
    this.classList = { toggle() {}, add() {}, remove() {} };
  }
  set textContent(v) { this.text = String(v); this.children = []; }
  get textContent() { return (this.text || '') + this.children.map(c => c.textContent).join(''); }
  append(...children) { for (const c of children) { if (c) { this.children.push(c); c.parent = this; } } }
  replaceChildren(...children) { this.text = ''; this.children = []; this.append(...children); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter(c => c !== this); }
  setAttribute(k, v) { this.attrs[k] = v; if (k === 'value') this.value = v; }
  addEventListener(k, fn) { (this.listeners[k] ||= []).push(fn); }
  // 子树查询：`$(sel, root)` 这种带根的查找要走到这里。只认 #id / .class / tag，
  // 够 app.js 用；不够的话它会抛，而不是悄悄返回 null。
  querySelector(sel) {
    const hit = (e) => sel.startsWith('#') ? e.attrs.id === sel.slice(1) || e.id === sel.slice(1)
      : sel.startsWith('.') ? String(e.className || '').split(/\s+/).includes(sel.slice(1))
      : e.tag === sel;
    const walk = (e) => {
      for (const c of e.children) { if (hit(c)) return c; const r = walk(c); if (r) return r; }
      return null;
    };
    return walk(this);
  }
  querySelectorAll(sel) {
    const out = [];
    const hit = (e) => sel.startsWith('.') ? String(e.className || '').split(/\s+/).includes(sel.slice(1)) : e.tag === sel;
    const walk = (e) => { for (const c of e.children) { if (hit(c)) out.push(c); walk(c); } };
    walk(this);
    return out;
  }
  // stopPropagation 必须真的停：不停的话，li 上那个「打开会话」会盖掉子按钮
  // 刚发出去的改名/删除 —— 而那正是这个属性在真实 DOM 里防的事。
  fire(k, props = {}) {
    let stopped = false;
    const e = { target: this, preventDefault() {}, stopPropagation() { stopped = true; }, ...props };
    for (let el = this; el && !stopped; el = el.parent) {
      for (const fn of el.listeners[k] || []) fn(e);
      if (!stopped) el['on' + k]?.(e);
    }
  }
  focus() {}
  select() {}
  setSelectionRange() {}
}
function setup() {
  const elements = new Map();
  const graphCalls = [];
  const doc = {
    querySelector(s) { if (!elements.has(s)) elements.set(s, new Element()); return elements.get(s); },
    // 真实 DOM 里 #modal 内部就有 .sheet-body；替身里让它们连上，
    // 这样 modal() 那条路（场景选择器、目录浏览器都走它）真的跑得起来
    _link() {
      const m = this.querySelector('#modal');
      const b = this.querySelector('.sheet-body');
      b.className = 'sheet-body';
      if (!m.children.includes(b)) m.append(b);
    },
    querySelectorAll() { return []; },
    createElement: tag => new Element(tag),
    createElementNS: (_, tag) => new Element(tag),
    createTextNode: text => { const e = new Element('#text'); e.textContent = text; return e; },
    body: new Element(), documentElement: new Element(),
  };
  doc._link();
  const graphRenderer = {
    renderGraph(host, payload, context) { graphCalls.push({host, payload, context}); context.onState?.({phase:'ready',warnings:[]}); },
    disposeGraph(host) { host.replaceChildren(); },
    setGraphZoom(host, value) { graphCalls.push({kind:'zoom', host, value}); return value; },
    fitGraph(host, viewport) { graphCalls.push({kind:'fit', host, viewport}); return 0.75; },
  };
  const ctx = vm.createContext({ document: doc, Node: Element, window: { innerHeight: 900 },
    __graphRenderer: graphRenderer, addEventListener() {}, setTimeout() {}, clearTimeout() {}, console });
  const source = fs.readFileSync('ui/app.js', 'utf8').replace(/boot\(\);\s*$/, '');
  vm.runInContext(source, ctx);
  const evalUI = s => vm.runInContext(s, ctx);
  evalUI(`globalThis.sent = []; ws = {readyState:1, send: s => sent.push(JSON.parse(s))};`);
  return { doc, evalUI, ctx, elements, graphCalls };
}
function all(e) { return [e, ...e.children.flatMap(all)]; }
const settings = {
  providers: { local: {api:'open_ai_compat',base_url:'http://localhost:1234',key_env:'LOCAL_KEY'} },
  roles: Object.fromEntries(['judge','answer','subagent'].map(r => [r,{provider:'local',model:r,temperature:0,max_tokens:200}])),
  web: {fetch:'http',fetch_base:'',search:'none',search_base:''},
  tools: {roots:['/first','/second'],net:false,allow_hosts:[],deny_hosts:[],deny_names:[],exec_allow:[],max_bytes:200,max_lines:20,max_matches:10,max_depth:4,max_line_len:100,max_entries:20,max_file_bytes:1000},
};
function form() {
  const f = setup();
  // S.saved 是「服务端说盘上是什么」，diff 的基准就是它
  f.evalUI(`S.settings = ${JSON.stringify(settings)}; S.saved = ${JSON.stringify(settings)}; renderConfig();`);
  const nodes = all(f.doc.querySelector('#config-form'));
  return {...f, nodes, save:nodes.find(e => e.tag === 'button' && e.textContent === '保存并重启会话')};
}
// 保存**只发改动过的字段**。整份发上去会拿浏览器这份快照覆盖磁盘 ——
// 用户刚在编辑器里手改的、或者别处刚落的东西就被冲掉了。这条正是
// 「应用目录把模型配置打回 anthropic」和「模型配置把目录打回 .」的病根。
test('saving sends only the changed fields, never the whole file', () => {
  const f = form();
  const model = f.nodes.find(e => e.tag === 'input' && e.value === 'answer');
  model.value = 'new-model'; model.fire('input');
  const net = f.nodes.find(e => e.attrs.type === 'checkbox');
  net.checked = true; net.fire('change');
  f.save.fire('click');
  const msg = f.evalUI('sent.at(-1)');
  assert.equal(msg.op, 'settings_patch');
  assert.equal(msg.changes['roles.answer.model'], 'new-model');
  assert.equal(msg.changes['tools.net'], true);
  assert.equal(msg.settings, undefined, '不发整份配置');
  assert.equal(msg.changes['tools.roots'], undefined, '没碰的字段一个都不发');
  assert.equal(msg.changes['roles.judge.provider'], undefined);
  assert.equal(Object.keys(msg.changes).length, 2, Object.keys(msg.changes).join(','));
});
test('config only exposes built-in fetch controls', () => {
  const f = form();
  const text = f.doc.querySelector('#config-form').textContent;
  assert.match(text, /网页抓取/);
  assert.doesNotMatch(text, /Crawl4AI|crawl4ai|SearXNG|searxng|Firecrawl|firecrawl|搜索/);
});
// 整份 JSON 在遮罩里改（侧栏小框读不了也改不动）。
// 强度不变：合法 JSON 存得进、坏的一条都发不出去。
test('config editor saves valid JSON and refuses malformed', () => {
  const f = form();
  f.evalUI(`S.saved = ${JSON.stringify(settings)}; openEditor('config');`);
  const next = structuredClone(settings); next.roles.judge.model = 'raw-model';
  f.evalUI(`S.editor.el.value = ${JSON.stringify(JSON.stringify(next))}; saveEditor();`);
  assert.equal(f.evalUI('sent.at(-1).settings.roles.judge.model'), 'raw-model');
  const before = f.evalUI('sent.length');
  f.evalUI(`S.editor.el.value = '{bad'; saveEditor();`);
  assert.equal(f.evalUI('sent.length'), before);
});
// 蒸馏的重点不是「模型写了什么」，是「用户不用自己搬」：分好的节要能逐节改、
// 逐节取舍，写回的必须**正好**是屏幕上那份。
test('distill sections are previewed, editable, and only the checked ones are written', () => {
  const f = setup();
  f.evalUI(`onMsg({ t:'distilled', path:'/w/memory/draft-1.md', sections:[
      { file:'project.md', mode:'replace', text:'新的项目描述', base_revision:'project-rev' },
      { file:'playbook.toml', mode:'merge', text:'[[scene]]\\nid = "x"', base_revision:'abc123' },
    ]});`);
  assert.equal(f.evalUI('S.distill.sections.length'), 2);
  assert.equal(f.doc.querySelector('#over').hidden, false, '草稿自己弹出来，不用去别处找');

  const rows = all(f.doc.querySelector('#ov-body'));
  const boxes = rows.filter(e => e.attrs.type === 'checkbox');
  const areas = rows.filter(e => e.tag === 'textarea');
  assert.equal(boxes.length, 2, '一节一个勾选框');
  assert.equal(areas[0].value, '新的项目描述', '正文直接铺出来给人改，不是只给个路径');

  // 改第一节、去掉第二节
  areas[0].value = '我改过的项目描述';
  areas[0].fire('input');
  boxes[1].checked = false;
  boxes[1].fire('change');

  f.evalUI('applyDistill()');
  const msg = f.evalUI('sent.at(-1)');
  assert.equal(msg.op, 'distill_apply');
  assert.equal(msg.sections.length, 1, '没勾的那节不写');
  assert.equal(msg.sections[0].file, 'project.md');
  assert.equal(msg.sections[0].text, '我改过的项目描述', '写回的是屏幕上那份，不是模型原文');
  assert.equal(msg.sections[0].mode, 'replace');
  assert.equal(msg.sections[0].base_revision, 'project-rev');

  // 一节都不勾就什么都不发 —— 静默写空文件是最糟的一种
  f.evalUI(`S.distill.sections.forEach(s => s.on = false);`);
  const before = f.evalUI('sent.length');
  f.evalUI('applyDistill()');
  assert.equal(f.evalUI('sent.length'), before);
});
test('empty distill output is shown as no update and opens no review sheet', () => {
  const f = setup();
  f.evalUI(`onMsg({ t:'distilled', path:'', sections:[], error:null });`);
  assert.notEqual(f.doc.querySelector('#over').hidden, false);
  assert.ok(f.evalUI(`S.banners.some(b => b.text.includes('没有可写回'))`));
});
// 持久层列的是**给人看的名字**（「系统提示词」而不是 prompts.toml），
// 正文开遮罩改 —— 侧栏里塞不下一篇 prompts.toml。
test('persistent files show human names and open in the overlay', () => {
  const f = setup();
  f.evalUI(`S.memory = [{file:'prompts.toml', text:'a\\nb'}]; renderMemory();`);
  const rows = all(f.doc.querySelector('#memory-list'));
  assert.equal(rows.some(e => e.tag === 'textarea'), false, '不内联大框');
  const row = rows.find(e => String(e.className || '').includes('frow'));
  assert.ok(row.textContent.includes('系统提示词'), '显示中文名：' + row.textContent);
  assert.ok(row.textContent.includes('prompts.toml'), '文件名也还在，不藏起来');
  row.fire('click');
  assert.equal(f.evalUI('S.editor.file'), 'prompts.toml');
  assert.equal(f.evalUI('S.editor.el.value'), 'a\nb');
  assert.equal(f.doc.querySelector('#over').hidden, false, '遮罩开着');
});
test('errors clear themselves but persistent states stay', () => {
  const f = setup();
  f.evalUI(`globalThis.fired = []; globalThis.setTimeout = fn => { fired.push(fn); return fired.length; };
    globalThis.clearTimeout = () => {}; renderBanner = () => {};
    banner('bad','boom','err1'); banner('bad','disk sick','persist');`);
  assert.equal(f.evalUI('S.banners.length'), 2);
  // 只有非持续状态排了定时；盘还在坏的时候横幅消失等于骗人
  assert.equal(f.evalUI('fired.length'), 1);
  f.evalUI('fired[0]()');
  // 在 vm 里造的数组跨 realm，deepEqual 认不出来 —— 比字符串就行
  assert.equal(f.evalUI(`S.banners.map(b => b.key).join(',')`), 'persist');
});
test('browser asks the server and hands the picked path back', () => {
  const f = setup();
  f.evalUI(`renderBrowser = () => {}; globalThis.got = null; openBrowser('/start', p => { got = p; });`);
  assert.equal(f.evalUI('sent.at(-1).op'), 'browse');
  assert.equal(f.evalUI('sent.at(-1).path'), '/start');
  f.evalUI(`pick('/start/sub')`);
  assert.equal(f.evalUI('got'), '/start/sub');
});
test('preset provider lands in the roster with its base_url and key env', () => {
  const f = setup();
  f.evalUI(`S.presets = [{name:'deepseek',label:'DeepSeek',api:'open_ai_compat',
      base_url:'https://api.deepseek.com/v1',key_env:'DEEPSEEK_API_KEY',models:['deepseek-chat']}];
    S.settings = ${JSON.stringify(settings)}; S.saved = ${JSON.stringify(settings)}; renderConfig();`);
  const nodes = all(f.doc.querySelector('#config-form'));
  nodes.find(e => e.tag === 'button' && e.textContent === '加进花名册').fire('click');
  // 加进的是**草稿**，不是 S.settings —— 后者是「服务端说盘上是什么」，
  // 让没保存的草稿冒充它，别处一发送就会把没保存的东西一起写出去。
  assert.equal(f.evalUI("S.settings.providers.deepseek"), undefined, '没保存前不动 S.settings');
  const after = all(f.doc.querySelector('#config-form'));
  assert.ok(after.some(e => String(e.textContent).includes('deepseek')), '但表单上已经有了');
  after.find(e => e.tag === 'button' && e.textContent === '保存并重启会话').fire('click');
  const put = f.evalUI('sent.at(-1)');
  assert.equal(put.op, 'settings_patch');
  assert.equal(put.changes['providers.deepseek'].base_url, 'https://api.deepseek.com/v1');
  assert.equal(put.changes['providers.deepseek'].key_env, 'DEEPSEEK_API_KEY');
  assert.equal(Object.keys(put.changes).length, 1, '只多了一个 provider，别的一个字都不动');
});
// 场景是多选：判断段本来就能一次判出几个，用户插手时没道理只准挑一个。
test('scene picker is multi-select and sends every checked id', () => {
  const f = setup();
  f.evalUI(`S.playbook = [
      {id:'none',label:'不做特殊干预',when:'闲聊'},
      {id:'trace_code',label:'从代码核对事实',when:'实现尚未读取'},
      {id:'cost_budget',label:'成本与可行性',when:'预算对不上'}];
    S.scenes = ['trace_code'];`);
  f.evalUI('openScenePicker()');
  const body = all(f.doc.querySelector('.sheet-body'));
  const boxes = body.filter(e => e.attrs.type === 'checkbox');
  assert.equal(boxes.length, 3, '每个场景一个勾选框');
  assert.equal(boxes[1].checked, true, '当前命中的预先勾上');
  boxes[2].checked = true; boxes[2].fire('change');
  body.find(e => e.tag === 'button' && e.textContent === '用这些').fire('click');
  const msg = f.evalUI('sent.at(-1)');
  assert.equal(msg.op, 'scene');
  assert.equal([...msg.to].sort().join(','), 'cost_budget,trace_code');
});

// 顶栏显示的是这一组场景，不是一个。
test('topbar shows every live scene', () => {
  const f = setup();
  f.evalUI(`S.playbook = [{id:'trace_code',label:'从代码核对事实'},{id:'cost_budget',label:'成本与可行性'}];
    S.scenes = ['trace_code','cost_budget']; renderTop();`);
  const t = f.doc.querySelector('#scene-chip').textContent;
  assert.ok(t.includes('从代码核对事实') && t.includes('成本与可行性'), t);
});

// 对话可改名 / 可删；当前这条不给删 —— 删了界面就挂在一个已经不存在的会话上。
test('sessions can be renamed and removed, except the live one', () => {
  const f = setup();
  f.evalUI(`S.session = 'a'; S.sessions = [{id:'a',title:'甲'},{id:'b',title:'乙'}];
    globalThis.prompt = () => '改过的名字'; globalThis.confirm = () => true;
    renderSessions();`);
  const rows = all(f.doc.querySelector('#session-list')).filter(e => e.tag === 'button');
  // 每条两个按钮：改名、删除
  assert.equal(rows.length, 4);
  rows[0].fire('click');
  assert.equal(f.evalUI('sent.at(-1).op'), 'session_rename');
  assert.equal(f.evalUI('sent.at(-1).session'), 'a');
  assert.equal(f.evalUI('sent.at(-1).title'), '改过的名字');
  // h() 对布尔属性写的是 setAttribute(k, '')，和真实 DOM 一致
  assert.equal(rows[1].attrs.disabled, '', '当前会话的删除是禁用的');
  rows[3].fire('click');
  assert.equal(f.evalUI('sent.at(-1).op'), 'session_del');
  assert.equal(f.evalUI('sent.at(-1).session'), 'b');
});

// 待落定/已搁置是会一直长的清单。只给「整块编辑」的话，删一条要先读懂整块，
// 于是没人删，于是它们只增不减。
test('working memory lists drop and move single items', () => {
  const f = setup();
  f.evalUI(`S.snap = { ws: { flow:{nodes:{},edges:{}}, fields:{}, open:['甲','乙'], parked:['丙'] } };
    drawGraph = () => {}; renderRight();`);
  const open = all(f.doc.querySelector('#open-list')).filter(e => e.tag === 'button');
  // 每条两个（挪走、删掉）+ 末尾一个「整块编辑」
  assert.equal(open.length, 5);
  open[1].fire('click');                       // 删掉「甲」
  let msg = f.evalUI('sent.at(-1)');
  assert.equal(msg.op, 'edit');
  assert.equal([...msg.ops[0].open].join(','), '乙', '发的是删完之后剩下的整份');

  f.evalUI(`renderRight();`);
  const again = all(f.doc.querySelector('#open-list')).filter(e => e.tag === 'button');
  again[0].fire('click');                      // 把「甲」挪去搁置
  msg = f.evalUI('sent.at(-1)');
  assert.equal(msg.ops.length, 2, '两个清单都是整份替换，所以要发两条');
  assert.equal([...msg.ops[0].open].join(','), '乙');
  assert.equal([...msg.ops[1].parked].join(','), '丙,甲');
});

// 场景是独立的块：用户要调的是「什么时候提醒我固定种子」，不是 playbook.toml 第 47 行。
test('a scene edits as its own block and writes back one scene', () => {
  const f = setup();
  f.evalUI(`S.playbook = [{id:'seed',label:'种子',when:'旧的触发条件',guidance:'旧的说法',tools:[]}];
    S.scenes = []; renderMemory();`);
  const row = all(f.doc.querySelector('#scene-list')).find(e => String(e.className || '').includes('frow'));
  assert.ok(row.textContent.includes('种子'));
  row.fire('click');
  assert.equal(f.doc.querySelector('#over').hidden, false);
  const areas = all(f.doc.querySelector('#ov-body')).filter(e => e.tag === 'textarea');
  areas[1].value = '新的说法';                  // guidance
  areas[1].fire('input');
  all(f.doc.querySelector('#ov-acts')).find(e => e.textContent === '保存').fire('click');
  const msg = f.evalUI('sent.at(-1)');
  assert.equal(msg.op, 'scene_put');
  assert.equal(msg.scene.id, 'seed');
  assert.equal(msg.scene.guidance, '新的说法');
  assert.equal(msg.scene.when, '旧的触发条件', '没动的字段原样带回去');
});

// 那一坨 stats JSON 不该摊在对话里 —— 它是排查用的。
test('turn footer shows a readable line, not the raw stats json', () => {
  const f = setup();
  const stats = JSON.stringify({ judge_ms: 2149, answer_ms: 8792, tool_ms: 0, loops: 1,
    tools_run: 2, inferred_ops: 5, dropped_ops: 0, asked_user: 0, compacted: 0 });
  f.evalUI(`S.timeline = [
      {seq:1,kind:'turn_open',turn:1,body:{}},
      {seq:2,kind:'wrote',turn:1,body:{text:'x'},html:'<p>x</p>'},
      {seq:3,kind:'turn_close',turn:1,body:{aborted:false,stats:${JSON.stringify(stats)}}}];
    renderStream();`);
  const foot = all(f.doc.querySelector('#stream')).find(e => String(e.className || '') === 'turn-foot');
  const txt = foot.textContent;
  assert.ok(!txt.includes('judge_ms'), '原始 JSON 不出现：' + txt);
  assert.ok(txt.includes('10.9s') && txt.includes('工具 2') && txt.includes('推断 5'), txt);
  const line = foot.children.find(e => String(e.className || '').includes('hint'));
  assert.ok(String(line.attrs.title).includes('judge_ms'), '全文挂在 title 上，想查还查得到');
});
test('boot restores active turn and resets stale per-session counters', () => {
  const f = setup();
  f.evalUI(`renderAll = () => {}; S.running = false; S.tokens = 999; S.foot = {total:99};
    onMsg({t:'boot',session:'new',settings:{},keys:{},timeline:[{kind:'cost',body:{usage:{prompt:3,completion:2}}}],snap:{turn:7,mode:'go',phase:'handoff',queued:1}});`);
  assert.equal(f.evalUI('S.running'), true); assert.equal(f.evalUI('S.turn'), 7);
  assert.equal(f.evalUI('S.tokens'), 5); assert.equal(f.evalUI('S.foot'), null);
});
test('snapshot replay deduplicates events and questions request a fresh snapshot', () => {
  const f = setup();
  f.evalUI(`renderStream = renderTop = () => {}; S.timeline = [{seq:1}];
    onMsg({t:'event',seq:1,kind:'said'}); onMsg({t:'event',seq:2,kind:'asked'});`);
  assert.equal(f.evalUI('S.timeline.length'), 2);
  assert.equal(f.evalUI('sent[0].op'), 'snap');
});
test('asked history keeps interactive option cards and marks the chosen answer', () => {
  const f = setup();
  f.evalUI(`S.timeline = [{seq:10,turn:3,kind:'asked',body:{question:'选哪个？',options:['甲','乙']},html:'<p>选哪个？</p>'}];
    renderStream();`);
  let cards = all(f.doc.querySelector('#stream')).filter(e => String(e.className || '').includes('ask-card'));
  assert.equal(cards.length, 2, '问题历史里直接显示全部选项');
  cards[1].fire('click');
  assert.equal(f.evalUI('sent.at(-1).op'), 'answer');
  assert.equal(f.evalUI('sent.at(-1).seq'), 10);
  assert.equal(f.evalUI('sent.at(-1).choice'), '乙');

  f.evalUI(`S.timeline.push({seq:11,kind:'answered',corr:10,body:{choice:'乙'},html:'<p>乙</p>'}); renderStream();`);
  cards = all(f.doc.querySelector('#stream')).filter(e => String(e.className || '').includes('ask-card'));
  assert.equal(cards.length, 2, '回答后历史选项仍保留');
  assert.ok(String(cards[1].className).includes('chosen'), '当时选中的一项可追溯');
  const before = f.evalUI('sent.length');
  cards[0].fire('click');
  assert.equal(f.evalUI('sent.length'), before, '回答后的历史卡片不重复提交');
});
test('floating ask row belongs only to the active turn and disappears when it closes', () => {
  const f = setup();
  f.evalUI(`S.timeline = [{seq:20,turn:7,kind:'asked',body:{question:'继续？',options:['是','否']}}];
    S.snap = {open_questions:[{seq:20,question:'继续？',options:['是','否']}]};`);
  f.evalUI(`S.turn = 7; S.running = true; renderAsk();`);
  assert.equal(f.doc.querySelector('#ask-row').hidden, false, '当前轮提问悬浮显示');
  f.evalUI(`S.turn = null; S.running = false; renderAsk();`);
  assert.equal(f.doc.querySelector('#ask-row').hidden, true, '本轮结束立刻消失');
  f.evalUI(`S.turn = 8; S.running = true; renderAsk();`);
  assert.equal(f.doc.querySelector('#ask-row').hidden, true, '旧问题不会在下一轮重新悬浮');
});
test('inference history distinguishes invalid ops from user-edit conflicts', () => {
  const f = setup();
  f.evalUI(`S.timeline = [{seq:30,turn:9,kind:'inferred',body:{ops:[],
    dropped:['node:bad','edge:e1'],invalid:['node:bad'],conflicts:['edge:e1']}}]; renderStream();`);
  const text = f.doc.querySelector('#stream').textContent;
  assert.ok(text.includes('无效 1 条') && text.includes('node:bad'), text);
  assert.ok(text.includes('冲突 1 条') && text.includes('edge:e1'), text);

  f.evalUI(`S.timeline = [{seq:31,turn:9,kind:'inferred',body:{ops:[],dropped:['old:path']}}]; renderStream();`);
  const oldText = f.doc.querySelector('#stream').textContent;
  assert.ok(oldText.includes('未生效 1 条'), oldText);
  assert.ok(!oldText.includes('你本轮改过'), oldText);
});
test('disconnected Enter keeps draft, stop names observed turn, roots keep extra grants', () => {
  const f = setup();
  f.evalUI(`connect = () => {}; boot(); S.session = 's'; S.turn = 7; ws.readyState = 3;`);
  const input = f.doc.querySelector('#input'); input.value = 'keep me';
  input.fire('keydown', { key:'Enter' }); assert.equal(input.value, 'keep me');
  f.evalUI(`ws.readyState = 1; S.settings = ${JSON.stringify(settings)};`);
  f.doc.querySelector('#stop').fire('click');
  assert.equal(f.evalUI('sent.at(-1).turn'), 7);
  // 「应用」**只发路径**。原来它发的是浏览器手上那份完整 settings ——
  // 而那是上一次 boot 的快照，用户在别处改过模型配置之后再点这里，
  // 就会把 roles 打回默认，下一次重启报「缺密钥」，看起来像密钥判断错了。
  f.doc.querySelector('#root-path').value = '/changed';
  f.doc.querySelector('#root-save').fire('click');
  assert.equal(f.evalUI('sent.at(-1).op'), 'roots_put');
  assert.equal(f.evalUI('sent.at(-1).path'), '/changed');
  assert.equal(f.evalUI('sent.at(-1).settings'), undefined, '不捎带任何别的配置');
  // 空输入不发。原来的 `|| '.'` 兜底会把可读目录悄悄改成当前目录，
  // 连点两次就把用户挑好的路径吃掉了。
  const n = f.evalUI('sent.length');
  f.doc.querySelector('#root-path').value = '   ';
  f.doc.querySelector('#root-save').fire('click');
  assert.equal(f.evalUI('sent.length'), n, '空目录一条都不发');
});
test('graph payload is handed to the renderer instead of being laid out in app.js', async () => {
  const f = setup();
  f.evalUI(`S.session='s'; S.snap={graph_render:{source:'flowchart TD',layout:'elk',bindings:[]},
    ws:{flow:{view:'built',nodes:{a:{id:'a',label:'A'}},edges:{}}}}; drawGraph(S.snap.ws.flow,S.snap.graph_render);`);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(f.graphCalls.length, 1);
  assert.equal(f.graphCalls[0].payload.layout, 'elk');
  assert.equal(f.graphCalls[0].context.renderId, 'graph-side');
});
test('expanded graph exposes explicit zoom and fit operations', async () => {
  const f = setup();
  assert.match(fs.readFileSync('ui/app.css', 'utf8'),
    /#graph-large \.mermaid-graph\{[^}]*min-width:0/,
    '大画布不能用 intrinsic min-width 顶住缩小');
  f.evalUI(`setLargeGraphZoom(1.4);`);
  await new Promise(resolve => setImmediate(resolve));
  const zoom = f.graphCalls.find(c => c.kind === 'zoom');
  assert.equal(zoom.value, 1.4);
  assert.equal(f.doc.querySelector('#graph-zoom-label').textContent, '140%');

  f.evalUI(`fitLargeGraph();`);
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(f.graphCalls.some(c => c.kind === 'fit'));
  assert.equal(f.doc.querySelector('#graph-zoom-label').textContent, '75%');
});

test('node editor preserves draft, patches changed fields only, and exposes same-field conflicts', () => {
  const f = setup();
  f.evalUI(`S.session='s'; S.snap={ws:{flow:{nodes:{a:{id:'a',label:'A',kind:'module',body:'old'}},edges:{}}}};
    editNode('a',S.snap.ws.flow.nodes.a);`);
  let fields = all(f.doc.querySelector('.sheet-body')).filter(e => e.tag === 'input' || e.tag === 'textarea');
  fields[0].value = '草稿';
  // 图刷新只换快照，不得回填或关闭正在编辑的表单。
  f.evalUI(`S.snap.ws.flow.nodes.a={id:'a',label:'A',kind:'loss',body:'new body'};`);
  assert.equal(fields[0].value, '草稿');
  const apply = all(f.doc.querySelector('.sheet-body')).find(e => e.tag === 'button' && e.textContent === '应用');
  apply.fire('click');
  const msg = f.evalUI('sent.at(-1)');
  assert.deepEqual(Object.keys(msg.ops[0]).sort(), ['id','label','op']);
  assert.equal(msg.ops[0].label, '草稿');

  // 同一字段变化时先停在冲突界面，展示当前值和草稿，再由用户决定覆盖。
  f.evalUI(`sent=[]; S.snap.ws.flow.nodes.a={id:'a',label:'当前值',kind:'loss',body:'new body'};
    editNode('a',{id:'a',label:'旧值',kind:'loss',body:'new body'});`);
  fields = all(f.doc.querySelector('.sheet-body')).filter(e => e.tag === 'input' || e.tag === 'textarea');
  fields[0].value = '我的草稿';
  all(f.doc.querySelector('.sheet-body')).find(e => e.tag === 'button' && e.textContent === '应用').fire('click');
  assert.equal(f.evalUI('sent.length'), 0);
  assert.match(f.doc.querySelector('.sheet-body').textContent, /当前值.*我的草稿/);
  all(f.doc.querySelector('.sheet-body')).find(e => e.tag === 'button' && e.textContent === '仍用我的草稿覆盖').fire('click');
  assert.equal(f.evalUI('sent[0].ops[0].label'), '我的草稿');
});
