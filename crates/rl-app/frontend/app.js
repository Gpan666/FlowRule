/* 限速器 · Miuix 前端
 * 纯渲染层：所有业务状态在 Rust 端，页面每 220ms 拉一次快照，
 * 交互只通过 invoke 下发意图。静态 DOM 骨架不重建，避免输入框丢焦点。
 */

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));

const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
}[c]));

function call(cmd, args = {}) {
  const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (!inv) return Promise.resolve(undefined);
  return inv(cmd, args).catch((e) => { console.error(`[${cmd}]`, e); });
}

let S = null;
const iconStore = {};
let searchText = '';
let listSig = '';
let customTimer = null;
let busy = false;

// ── 初始化：为每个方向规则块生成固定 chips ──
function buildBlocks() {
  $$('.dir-block').forEach((block) => {
    const chips = $('[data-role="chips"]', block);
    for (let i = 0; i < 9; i++) {
      const b = document.createElement('button');
      b.className = 'chip';
      b.dataset.role = 'profileChip';
      b.dataset.profileId = String(i);
      b.innerHTML = '<span>—</span>';
      chips.appendChild(b);
    }
    const c = document.createElement('button');
    c.className = 'chip custom-label';
    c.dataset.role = 'customChip';
    c.textContent = '自定义';
    chips.appendChild(c);
  });
}

function hueFromName(s) {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return h % 360;
}

// ── 曲线：Catmull-Rom 转三次贝塞尔，得到 Miuix 那种顺滑的面积图 ──
function smoothPath(vals, w, h, max) {
  let v = vals.length >= 2 ? vals : (vals.length === 1 ? [0, vals[0]] : [0, 0]);
  const pts = v.map((n, i) => [
    (i / (v.length - 1)) * w,
    h - 8 - (Math.max(0, n) / max) * (h - 20)
  ]);
  let d = `M ${pts[0][0].toFixed(1)},${pts[0][1].toFixed(1)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[Math.max(0, i - 1)], p1 = pts[i];
    const p2 = pts[i + 1], p3 = pts[Math.min(pts.length - 1, i + 2)];
    const c1x = p1[0] + (p2[0] - p0[0]) / 6, c1y = p1[1] + (p2[1] - p0[1]) / 6;
    const c2x = p2[0] - (p3[0] - p1[0]) / 6, c2y = p2[1] - (p3[1] - p1[1]) / 6;
    d += ` C ${c1x.toFixed(1)},${c1y.toFixed(1)} ${c2x.toFixed(1)},${c2y.toFixed(1)} ${p2[0].toFixed(1)},${p2[1].toFixed(1)}`;
  }
  return d;
}

function renderGraph(g) {
  const empty = !g || g.peak <= 0 || (!g.down.length && !g.up.length);
  $('#graphEmpty').hidden = !empty;
  $('#peakText').textContent = empty ? '峰值 —' : `峰值 ${g.peak_text}`;
  if (empty) {
    ['#pathDown', '#pathUp', '#fillDown', '#fillUp'].forEach((s) => $(s).setAttribute('d', ''));
    return;
  }
  const max = Math.max(g.peak * 1.12, 1);
  const dDown = smoothPath(g.down, 1000, 180, max);
  const dUp = smoothPath(g.up, 1000, 180, max);
  $('#pathDown').setAttribute('d', dDown);
  $('#pathUp').setAttribute('d', dUp);
  $('#fillDown').setAttribute('d', `${dDown} L 1000,180 L 0,180 Z`);
  $('#fillUp').setAttribute('d', `${dUp} L 1000,180 L 0,180 Z`);
}

// ── 方向规则块（静态 DOM，只更新文本与 class） ──
function renderDirBlock(block, dto) {
  if (!block || !dto) return;
  $('[data-role="switch"]', block).classList.toggle('on', dto.enabled);
  const val = $('[data-role="value"]', block);
  val.textContent = dto.short;
  val.classList.toggle('muted', dto.bytes === 0);
  const explain = $('[data-role="explain"]', block);
  if (explain) explain.textContent = dto.explain;

  $$('[data-role="profileChip"]', block).forEach((b, i) => {
    b.classList.toggle('active', dto.sel === `p${i}`);
  });
  $('[data-role="customChip"]', block).classList.toggle('active', dto.sel === 'custom');
  $('[data-role="customRow"]', block).hidden = dto.sel !== 'custom';

  const input = $('[data-role="customInput"]', block);
  if (document.activeElement !== input && input.value !== dto.custom) input.value = dto.custom;
  const unit = $('[data-role="unitLabel"]', block);
  if (unit) unit.textContent = S.unit_label;
}

function renderList(s) {
  const needle = searchText.trim().toLowerCase();
  const items = s.programs.filter((p) => !needle
    || p.name.toLowerCase().includes(needle)
    || p.path.toLowerCase().includes(needle)
    || p.pid_list.some((id) => String(id).includes(needle)));

  const sig = `${needle}|${s.selected}|${items.map((p) =>
    `${p.key}:${p.pids}:${p.icon ? 1 : 0}:${p.down_text}:${p.up_text}:${p.limited ? 1 : 0}`).join('|')}`;
  $('#countChip').textContent = s.scanning
    ? '扫描中…'
    : `${s.program_count} 个程序 · ${s.process_count} 个进程`;
  if (sig === listSig) return;
  listSig = sig;

  const list = $('#programList');
  const top = list.scrollTop;
  list.innerHTML = items.length ? items.map((p) => {
    const sel = p.key === s.selected;
    if (p.icon) iconStore[p.key] = p.icon;
    const iconData = p.icon || iconStore[p.key] || null;
    return `<div class="prow${sel ? ' selected' : ''}" data-key="${esc(p.key)}" title="${esc(p.path)}">
      <div class="pname${iconData ? '' : ' no-icon'}">
        ${iconData ? `<img class="picon-img" src="data:image/png;base64,${iconData}" alt="">` : ''}
        <div class="pmeta"><b>${esc(p.name)}</b><small>${p.pids > 1 ? p.pids + ' 个进程' : '单进程'}</small></div>
      </div>
      <span class="rate down${p.down ? ' hot' : ''}">${esc(p.down_text)}</span>
      <span class="rate up${p.up ? ' hot' : ''}">${esc(p.up_text)}</span>
      <span class="ptag">${p.limited ? '<span class="badge">限速中</span>' : ''}</span>
    </div>`;
  }).join('') : '<div class="empty-list">没有匹配的程序</div>';
  list.scrollTop = top;
}

function renderDetail(s) {
  const has = !!s.detail;
  $('#detailEmpty').hidden = has;
  $('#detailBody').hidden = !has;
  if (!has) return;
  const d = s.detail;
  $('#detailName').textContent = d.name;
  $('#detailPath').textContent = d.path || '无法获取可执行文件路径';
  $('#detailPath').title = d.path || '';
  if (d.icon) iconStore[d.key] = d.icon;
  const iconData = d.icon || iconStore[d.key] || null;
  const icon = $('#detailIcon');
  const iconSig = iconData ? `${d.key}:${iconData.length}` : 'none';
  if (icon.dataset.sig !== iconSig) {
    icon.dataset.sig = iconSig;
    if (iconData) {
      icon.hidden = false;
      icon.classList.add('has-img');
      icon.innerHTML = `<img src="data:image/png;base64,${iconData}" alt="">`;
    } else {
      icon.hidden = true;
      icon.classList.remove('has-img');
      icon.innerHTML = '';
    }
  }
  $('#detailDown').textContent = d.down_text;
  $('#detailUp').textContent = d.up_text;
  const pids = $('#detailPids');
  pids.textContent = d.running ? `${d.pids} 个进程 · 运行中` : '当前未运行';
  pids.classList.toggle('off', !d.running);
  renderDirBlock($('[data-target="program"][data-dir="down"]'), d.down_rule);
  renderDirBlock($('[data-target="program"][data-dir="up"]'), d.up_rule);
}

function render(s) {
  S = s;
  $('#downNum').textContent = s.totals.down_num;
  $('#downUnit').textContent = s.totals.down_unit;
  $('#upNum').textContent = s.totals.up_num;
  $('#upUnit').textContent = s.totals.up_unit;
  $('#masterSwitch').classList.toggle('on', s.master);
  $('#masterLabel').textContent = s.master ? '限速已开启' : '限速已暂停';
  $('#unitSeg').dataset.active = s.unit;
  $$('.seg-item').forEach((b) => b.classList.toggle('active', b.dataset.unit === s.unit));

  $('#statusText').textContent = s.status;
  $('#statusPill').classList.toggle('error', s.status.startsWith('引擎错误'));
  $('#elevateBanner').hidden = s.elevated;
  const warn = $('#warningPill');
  warn.hidden = !s.warning;
  if (s.warning) warn.textContent = s.warning;

  $$('[data-role="profileChip"]').forEach((b, i) => {
    const p = s.profiles[i % s.profiles.length];
    if (p) {
      const span = $('span', b);
      if (span.textContent !== p.label) span.textContent = p.label;
      b.title = p.explain;
    }
  });

  renderGraph(s.graph);
  renderList(s);
  renderDetail(s);
  renderDirBlock($('[data-target="other"][data-dir="down"]'), s.other.down);
  renderDirBlock($('[data-target="other"][data-dir="up"]'), s.other.up);
}

async function tick() {
  if (busy) return;
  busy = true;
  try {
    const s = await call('get_snapshot');
    if (s) render(s);
  } finally {
    busy = false;
  }
}

async function act(cmd, args) {
  await call(cmd, args);
  await tick();
}

// ── 事件 ──
function bindEvents() {
  $('#themeBtn').addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    try { localStorage.setItem('rl-theme', next); } catch (e) { /* noop */ }
  });

  $$('.tb-btn.win').forEach((b) => b.addEventListener('click', () => call('win_action', { action: b.dataset.win })));

  $('#masterSwitch').addEventListener('click', () => act('set_master', { on: !S.master }));
  $('#unitSeg').addEventListener('click', (e) => {
    const b = e.target.closest('.seg-item');
    if (b) act('set_unit', { unit: b.dataset.unit });
  });
  $('#refreshBtn').addEventListener('click', () => act('refresh', {}));
  $('#elevateBtn').addEventListener('click', () => call('relaunch_admin'));
  $('#clearBtn').addEventListener('click', () => {
    if (S && S.selected) act('clear_rule', { key: S.selected });
  });

  $('#search').addEventListener('input', (e) => {
    searchText = e.target.value;
    if (S) renderList(S);
  });

  document.addEventListener('input', (e) => {
    if (!e.target.matches('[data-role="customInput"]')) return;
    const block = e.target.closest('.dir-block');
    const key = block.dataset.target === 'program' ? (S && S.selected) : null;
    if (block.dataset.target === 'program' && !key) return;
    const val = e.target.value;
    clearTimeout(customTimer);
    customTimer = setTimeout(() => act('set_rule', {
      target: block.dataset.target,
      key,
      direction: block.dataset.dir,
      custom_value: val
    }), 280);
  });

  document.addEventListener('click', (e) => {
    const chip = e.target.closest('.chip');
    if (chip) {
      const block = chip.closest('.dir-block');
      const key = block.dataset.target === 'program' ? (S && S.selected) : null;
      if (block.dataset.target === 'program' && !key) return;
      const args = { target: block.dataset.target, key, direction: block.dataset.dir };
      if (chip.dataset.role === 'customChip') args.custom = true;
      else args.profile_id = Number(chip.dataset.profileId);
      act('set_rule', args);
      return;
    }

    const sw = e.target.closest('.switch[data-role="switch"]');
    if (sw) {
      const block = sw.closest('.dir-block');
      const key = block.dataset.target === 'program' ? (S && S.selected) : null;
      if (block.dataset.target === 'program' && !key) return;
      act('set_rule', {
        target: block.dataset.target,
        key,
        direction: block.dataset.dir,
        enabled: !sw.classList.contains('on')
      });
      return;
    }

    const row = e.target.closest('.prow');
    if (row) act('select_program', { key: row.dataset.key });
  });
}

// ── 启动 ──
try { document.documentElement.dataset.theme = localStorage.getItem('rl-theme') || 'light'; } catch (e) { /* noop */ }
buildBlocks();
bindEvents();
tick();
setInterval(tick, 220);
