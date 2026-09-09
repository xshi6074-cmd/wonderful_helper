import mermaid from '/vendor/mermaid.bundle.js';

// Mermaid 的配置是进程级的；即使侧栏与大画布是两个 host，也必须串行调用。
// host 自己只保留一个尚未开始的请求，所以连续快照会自然合并到最新一张。
const states = new WeakMap();
let engineTail = Promise.resolve();

function stateOf(host) {
  let state = states.get(host);
  if (!state) {
    state = { seq: 0, running: false, pending: null, renderedKey: '', displayedSession: null, context: null };
    states.set(host, state);
  }
  return state;
}

export function renderGraph(host, payload, context = {}) {
  if (!host) return Promise.resolve();
  const state = stateOf(host);
  state.context = context;
  const sessionId = context.sessionId ?? null;

  // 会话切换时先撤掉旧图；渲染失败也绝不能让上一会话继续占着画布。
  if (state.displayedSession !== null && state.displayedSession !== sessionId) {
    host.replaceChildren();
    state.renderedKey = '';
    state.displayedSession = null;
  }
  if (!payload?.source) {
    disposeGraph(host);
    return Promise.resolve();
  }

  const layout = context.layout || payload.layout || 'elk';
  const theme = context.theme || currentTheme();
  const key = `${sessionId}\u0000${layout}\u0000${theme}\u0000${payload.source}`;
  if (!context.force && key === state.renderedKey && state.displayedSession === sessionId) {
    report(state, { phase: 'ready', warnings: payload.warnings || [], layout });
    return Promise.resolve();
  }

  const req = { seq: ++state.seq, payload, sessionId, layout, theme, key };
  state.pending = req;
  if (!state.running) return pump(host, state);
  return Promise.resolve();
}

export function disposeGraph(host) {
  if (!host) return;
  const state = stateOf(host);
  state.seq++;
  state.pending = null;
  state.renderedKey = '';
  state.displayedSession = null;
  state.context = null;
  host.replaceChildren();
}

async function pump(host, state) {
  state.running = true;
  try {
    while (state.pending) {
      const req = state.pending;
      state.pending = null;
      await enqueue(() => perform(host, state, req));
    }
  } finally {
    state.running = false;
  }
}

function enqueue(task) {
  const run = engineTail.then(task, task);
  engineTail = run.catch(() => {});
  return run;
}

async function perform(host, state, req) {
  if (req.seq !== state.seq) return;
  report(state, { phase: 'rendering', warnings: req.payload.warnings || [], layout: req.layout });
  try {
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      suppressErrorRendering: true,
      layout: req.layout,
      theme: 'base',
      themeVariables: themeVariables(req.theme),
      flowchart: {
        htmlLabels: true,
        wrappingWidth: 220,
        nodeSpacing: 42,
        rankSpacing: 58,
        curve: req.layout === 'elk' ? 'linear' : 'basis',
      },
      elk: {
        mergeEdges: false,
        nodePlacementStrategy: 'NETWORK_SIMPLEX',
        cycleBreakingStrategy: 'GREEDY',
      },
    });

    const renderId = `${safeId(state.context?.renderId || 'graph')}_${req.seq}`;
    const result = await mermaid.render(renderId, req.payload.source);
    if (req.seq !== state.seq || state.pending) return;

    const template = document.createElement('template');
    template.innerHTML = result.svg;
    const svg = template.content.firstElementChild;
    if (!svg) throw new Error('Mermaid 没有返回 SVG');
    svg.removeAttribute('height');
    svg.removeAttribute('width');
    svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', '实验流程推断图');
    svg.classList.add('mermaid-graph');

    const missing = bind(svg, req.payload.bindings || [], state, renderId);
    if (req.seq !== state.seq || state.pending) return;
    host.replaceChildren(svg);
    state.renderedKey = req.key;
    state.displayedSession = req.sessionId;
    report(state, {
      phase: 'ready',
      layout: req.layout,
      warnings: [...(req.payload.warnings || []), ...missing.map(x => `无法映射 ${x.kind} ${x.id}`)],
      missing,
    });
  } catch (error) {
    if (req.seq !== state.seq || state.pending) return;
    const detail = error instanceof Error ? error.message : String(error);
    const prefix = req.layout === 'elk'
      ? 'ELK 图未更新（未自动降级为 Dagre）'
      : 'Dagre 图未更新';
    report(state, { phase: 'error', layout: req.layout, error: `${prefix}：${detail}` });
  }
}

function bind(svg, bindings, state, renderId) {
  const missing = [];
  for (const binding of bindings) {
    const matches = [...svg.querySelectorAll(
      `.${binding.marker}, [data-id="${binding.marker}"], [id="${renderId}-${binding.marker}"]`,
    )];
    if (!matches.length) {
      missing.push(binding);
      continue;
    }
    const roots = new Set();
    for (const match of matches) {
      const root = binding.kind === 'edge'
        ? closest(match, '.edgePath, .edgeLabel') || match
        : binding.kind === 'group'
          ? closest(match, '.cluster') || match
          : closest(match, '.node') || match;
      roots.add(root);
    }
    for (const root of roots) {
      root.dataset.pmId = binding.id;
      root.dataset.pmKind = binding.kind;
      root.classList.add('pm-selectable');
      for (const name of binding.classes || []) root.classList.add(name);
      if (binding.kind !== 'edge') {
        root.setAttribute('tabindex', '0');
        root.setAttribute('role', 'button');
        root.setAttribute('aria-label', `${binding.kind === 'group' ? '分组' : '节点'} ${binding.id}，按 Enter 编辑`);
      }
    }
  }

  svg.addEventListener('click', event => {
    const target = closest(event.target, '[data-pm-id]');
    if (!target) return;
    event.stopPropagation();
    select(state, target.dataset.pmKind, target.dataset.pmId);
  });
  svg.addEventListener('keydown', event => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    const target = closest(event.target, '[data-pm-id]');
    if (!target) return;
    event.preventDefault();
    event.stopPropagation();
    select(state, target.dataset.pmKind, target.dataset.pmId);
  });
  return missing;
}

function select(state, kind, id) {
  const context = state.context;
  if (!context || context.sessionId !== context.currentSession?.()) return;
  context.onSelect?.(kind, id);
}

function closest(node, selector) {
  return node && typeof node.closest === 'function' ? node.closest(selector) : null;
}

function report(state, detail) {
  state.context?.onState?.(detail);
}

function safeId(value) {
  return String(value).replace(/[^a-zA-Z0-9_-]/g, '_');
}

function currentTheme() {
  return globalThis.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

function themeVariables(theme) {
  if (theme === 'dark') {
    return {
      background: '#151822', primaryTextColor: '#edf0f7', lineColor: '#8790a3',
      clusterBkg: '#1a1e2a', clusterBorder: '#586174', edgeLabelBackground: '#151822',
      fontFamily: 'Inter, "Noto Sans SC", "Microsoft YaHei", sans-serif',
    };
  }
  return {
    background: '#f7f9fc', primaryTextColor: '#1b2433', lineColor: '#667085',
    clusterBkg: '#f4f6fb', clusterBorder: '#a5adbb', edgeLabelBackground: '#f7f9fc',
    fontFamily: 'Inter, "Noto Sans SC", "Microsoft YaHei", sans-serif',
  };
}
