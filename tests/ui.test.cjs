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
  append(...children) { for (const c of children) { this.children.push(c); c.parent = this; } }
  setAttribute(k, v) { this.attrs[k] = v; if (k === 'value') this.value = v; }
  addEventListener(k, fn) { (this.listeners[k] ||= []).push(fn); }
  fire(k, props = {}) {
    const e = { target: this, preventDefault() {}, ...props };
    for (let el = this; el; el = el.parent) for (const fn of el.listeners[k] || []) fn(e);
    this['on' + k]?.(e);
  }
  focus() {}
  select() {}
}
function setup() {
  const elements = new Map();
  const doc = {
    querySelector(s) { if (!elements.has(s)) elements.set(s, new Element()); return elements.get(s); },
    querySelectorAll() { return []; },
    createElement: tag => new Element(tag),
    createElementNS: (_, tag) => new Element(tag),
    createTextNode: text => { const e = new Element('#text'); e.textContent = text; return e; },
    body: new Element(), documentElement: new Element(),
  };
  const ctx = vm.createContext({ document: doc, Node: Element, window: { innerHeight: 900 },
    addEventListener() {}, setTimeout() {}, clearTimeout() {}, console });
  const source = fs.readFileSync('ui/app.js', 'utf8').replace(/boot\(\);\s*$/, '');
  vm.runInContext(source, ctx);
  const evalUI = s => vm.runInContext(s, ctx);
  evalUI(`globalThis.sent = []; ws = {readyState:1, send: s => sent.push(JSON.parse(s))};`);
  return { doc, evalUI, ctx, elements };
}
function all(e) { return [e, ...e.children.flatMap(all)]; }
const settings = {
  providers: { local: {api:'open_ai_compat',base_url:'http://localhost:1234',key_env:'LOCAL_KEY'} },
  roles: Object.fromEntries(['judge','answer','subagent'].map(r => [r,{provider:'local',model:r,temperature:0,max_tokens:200}])),
  web: {fetch:'http',fetch_base:'',search:'none',search_base:''},
  tools: {roots:['/first','/second'],net:false,allow_hosts:[],deny_hosts:[],deny_names:[],exec_allow:[],max_bytes:200,max_lines:20,max_matches:10,max_depth:4,max_line_len:100,max_entries:20,max_file_bytes:1000},
};
function form() {
  const f = setup(); f.evalUI(`S.settings = ${JSON.stringify(settings)}; renderConfig();`);
  const nodes = all(f.doc.querySelector('#config-form'));
  return {...f, nodes, save:nodes.find(e => e.tag === 'button' && e.textContent === '保存并重启会话')};
}
test('role and permission form edits reach settings_put', () => {
  const f = form();
  const model = f.nodes.find(e => e.tag === 'input' && e.value === 'answer');
  model.value = 'new-model'; model.fire('input');
  const net = f.nodes.find(e => e.attrs.type === 'checkbox');
  net.checked = true; net.fire('change');
  f.save.fire('click');
  assert.equal(f.evalUI('sent.at(-1).settings.roles.answer.model'), 'new-model');
  assert.equal(f.evalUI('sent.at(-1).settings.tools.net'), true);
});
// 原来这条测的是左栏那个 rows=10 的 raw 框。整份 JSON 现在改在右栏编辑区里
// （侧栏小框读不了也改不动），断言跟着搬过去，强度不变：合法的存得进，坏的一条都发不出去。
test('config editor saves valid JSON and refuses malformed', () => {
  const f = form();
  f.evalUI(`S.saved = ${JSON.stringify(settings)}; openEditor('config');`);
  const ed = f.doc.querySelector('#ed-text');
  const next = structuredClone(settings); next.roles.judge.model = 'raw-model';
  ed.value = JSON.stringify(next);
  f.evalUI('saveEditor()');
  assert.equal(f.evalUI('sent.at(-1).settings.roles.judge.model'), 'raw-model');
  const before = f.evalUI('sent.length');
  ed.value = '{bad';
  f.evalUI('saveEditor()');
  assert.equal(f.evalUI('sent.length'), before);
});
test('memory files open in the right pane instead of an inline box', () => {
  const f = setup();
  f.evalUI(`S.memory = [{file:'prompts.toml', text:'a\\nb'}]; renderMemory();`);
  const rows = all(f.doc.querySelector('#memory-list'));
  assert.equal(rows.some(e => e.tag === 'textarea'), false);
  // h() 把 class 写在 className 上（不是 setAttribute），所以查这里
  rows.find(e => String(e.className || '').includes('frow')).fire('click');
  assert.equal(f.evalUI('S.editor.file'), 'prompts.toml');
  assert.equal(f.doc.querySelector('#ed-text').value, 'a\nb');
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
    S.settings = ${JSON.stringify(settings)}; renderConfig();`);
  const nodes = all(f.doc.querySelector('#config-form'));
  nodes.find(e => e.tag === 'button' && e.textContent === '加进花名册').fire('click');
  assert.equal(f.evalUI("S.settings.providers.deepseek.base_url"), 'https://api.deepseek.com/v1');
  assert.equal(f.evalUI("S.settings.providers.deepseek.key_env"), 'DEEPSEEK_API_KEY');
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
test('disconnected Enter keeps draft, stop names observed turn, roots keep extra grants', () => {
  const f = setup();
  f.evalUI(`connect = () => {}; boot(); S.session = 's'; S.turn = 7; ws.readyState = 3;`);
  const input = f.doc.querySelector('#input'); input.value = 'keep me';
  input.fire('keydown', { key:'Enter' }); assert.equal(input.value, 'keep me');
  f.evalUI(`ws.readyState = 1; S.settings = ${JSON.stringify(settings)};`);
  f.doc.querySelector('#stop').fire('click');
  assert.equal(f.evalUI('sent.at(-1).turn'), 7);
  f.doc.querySelector('#root-path').value = '/changed';
  f.doc.querySelector('#root-save').fire('click');
  assert.equal(f.evalUI('sent.at(-1).settings.tools.roots[1]'), '/second');
});
test('cyclic graph nodes stay inside the SVG viewport', () => {
  const f = setup();
  f.evalUI(`drawGraph({nodes:{a:{label:'A'},b:{label:'B'}},edges:{ab:{from:'a',to:'b'},ba:{from:'b',to:'a'}}});`);
  const svg = f.doc.querySelector('#graph').children[0];
  for (const rect of all(svg).filter(e => e.tag === 'rect')) {
    assert.ok(Number(rect.attrs.y) + Number(rect.attrs.height) <= Number(svg.attrs.height));
  }
});
