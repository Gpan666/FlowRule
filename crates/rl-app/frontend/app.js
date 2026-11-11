/* 流律 · Throttle NetLimiter 形态前端
 * 纯渲染层：所有业务状态在 Rust 引擎端，页面每 220ms 拉一次快照，
 * 交互只通过 invoke 下发意图（set_rule / clear_rule / set_master /
 * set_other_limits）。表格行做 keyed 更新，避免重建导致失焦与滚动跳动。
 */

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));

const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
}[c]));

function call(cmd, args = {}) {
  const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (!inv) return Promise.resolve(undefined);
  return inv(cmd, args).catch((e) => console.error(`[${cmd}]`, e));
}

// ── 全局状态 ──
let unit = localStorage.getItem('rl-unit') || 'kb';       // kb | mb
let searchText = '';
let sortKey = null;                                       // null | 'down' | 'up'（点击表头按当前速率降序）
let snap = null;                                          // 最近一次快照
const rows = new Map();                                   // exe → 行元素引用
const iconStore = new Map();                              // exe → base64 png | null
const hist = { down: [], up: [], lastSec: 0 };            // 60s 总速率环形历史
let modalCtx = null;                                      // {kind, exe}

const KB = 1024, MB = 1024 * 1024;
const unitFactor = () => (unit === 'mb' ? MB : KB);
const unitLabel = () => (unit === 'mb' ? 'MB/s' : 'KB/s');

function fmtRate(bps) {
  if (unit === 'mb') return (bps / MB).toFixed(2);
  return (bps / KB).toFixed(1);
}
function fmtRateText(bps) {
  if (!bps) return '0';
  return `${fmtRate(bps)} ${unit === 'mb' ? 'MB' : 'KB'}`;
}
function fmtLimit(bps) {
  if (bps == null) return null;
  return fmtRateText(bps);
}

// ── 曲线：Catmull-Rom 转三次贝塞尔的平滑面积图 ──
function smoothPath(vals, w, h, max) {
  const v = vals.length >= 2 ? vals : [0, 0];
  const pts = v.map((n, i) => [
    (i / (v.length - 1)) * w,
    h - 2 - (Math.max(0, n) / max) * (h - 6),
  ]);
  if (pts.length < 2) return '';
  let d = `M ${pts[0][0].toFixed(2)} ${pts[0][1].toFixed(2)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[Math.max(0, i - 1)], p1 = pts[i], p2 = pts[i + 1], p3 = pts[Math.min(pts.length - 1, i + 2)];
    const c1 = [p1[0] + (p2[0] - p0[0]) / 6, p1[1] + (p2[1] - p0[1]) / 6];
    const c2 = [p2[0] - (p3[0] - p1[0]) / 6, p2[1] - (p3[1] - p1[1]) / 6];
    d += ` C ${c1[0].toFixed(2)} ${c1[1].toFixed(2)}, ${c2[0].toFixed(2)} ${c2[1].toFixed(2)}, ${p2[0].toFixed(2)} ${p2[1].toFixed(2)}`;
  }
  return d;
}

function drawGraph() {
  const w = 1000, h = 150;
  const peak = Math.max(64 * KB, ...hist.down, ...hist.up);
  const dLine = smoothPath(hist.down, w, h, peak);
  const uLine = smoothPath(hist.up, w, h, peak);
  $('#pathDown').setAttribute('d', dLine);
  $('#pathUp').setAttribute('d', uLine);
  $('#fillDown').setAttribute('d', dLine ? `${dLine} L ${w} ${h} L 0 ${h} Z` : '');
  $('#fillUp').setAttribute('d', uLine ? `${uLine} L ${w} ${h} L 0 ${h} Z` : '');
  $('#peakText').textContent = `峰值 ${fmtRateText(peak)}`;
  $('#graphEmpty').hidden = hist.down.length > 1;
}

// ── 表格行 ──
function ruleOf(exe) {
  return snap && snap.rules ? snap.rules[exe] : null;
}

function ruleChips(exe, isOther) {
  let down, up, blocked = false;
  if (isOther) {
    down = snap.other_down_bps; up = snap.other_up_bps;
  } else {
    const r = ruleOf(exe);
    if (r) { down = r.down_limit; up = r.up_limit; blocked = r.blocked; }
  }
  if (blocked) return `<span class="rule-chip blocked">已阻断</span>`;
  const d = fmtLimit(down), u = fmtLimit(up);
  if (d == null && u == null) return `<span class="rule-chip none">未限速</span>`;
  return (d != null ? `<span class="rule-chip">↓ ${esc(d)}/s</span>` : '') +
         (u != null ? `<span class="rule-chip up">↑ ${esc(u)}/s</span>` : '');
}

function ensureRow(exe, name, path, isOther) {
  let row = rows.get(exe);
  if (row) return row;
  const el = document.createElement('div');
  el.className = 'proc-row' + (isOther ? ' other' : '');
  el.innerHTML = `
    <div class="p-app">
      <div class="app-icon"><img alt="" hidden /></div>
      <div class="app-text">
        <div class="p-name">${esc(name)}</div>
        <div class="p-path">${esc(path || exe)}</div>
      </div>
    </div>
    <div class="p-rate down"><span data-r="down">0</span><small>下行</small></div>
    <div class="p-rate up"><span data-r="up">0</span><small>上行</small></div>
    <div class="p-rule" data-r="rule"></div>
    <div class="p-ops">
      <button class="row-btn" data-act="limit">限速</button>
      <button class="row-btn ghost danger" data-act="clear" title="清除限速">✕</button>
    </div>`;
  el.querySelector('[data-act="limit"]').addEventListener('click', () => openModal(exe, isOther));
  el.querySelector('[data-act="clear"]').addEventListener('click', () => clearLimits(exe, isOther));
  $('#procList').appendChild(el);
  row = {
    el,
    down: el.querySelector('[data-r="down"]'),
    up: el.querySelector('[data-r="up"]'),
    rule: el.querySelector('[data-r="rule"]'),
    icon: el.querySelector('.app-icon img'),
    iconLoaded: false,
  };
  rows.set(exe, row);
  return row;
}

function loadIcon(exe, row) {
  if (row.iconLoaded) return;
  if (!iconStore.has(exe)) {
    iconStore.set(exe, null); // 占位防止重复请求
    call('get_icon', { exePath: exe }).then((b64) => {
      iconStore.set(exe, b64 || null);
      const r = rows.get(exe);
      if (r && b64) { r.icon.src = `data:image/png;base64,${b64}`; r.icon.hidden = false; }
    });
    return;
  }
  const b64 = iconStore.get(exe);
  if (b64) { row.icon.src = `data:image/png;base64,${b64}`; row.icon.hidden = false; }
  row.iconLoaded = true;
}

function renderTable() {
  if (!snap) return;
  const q = searchText.trim().toLowerCase();
  const list = snap.processes.filter((p) =>
    !q || p.name.toLowerCase().includes(q) || p.exe_path.toLowerCase().includes(q) || String(p.pid).includes(q));

  // 表头排序：按当前速率降序；每次渲染都重排，随实时速率动态变化
  if (sortKey) {
    list.sort((a, b) => b[sortKey + '_rate'] - a[sortKey + '_rate']);
  }

  const wanted = list.map((p) => p.exe_path);
  // 删除已消失的行
  for (const [exe, row] of rows) {
    if (exe !== '__other__' && !wanted.includes(exe)) { row.el.remove(); rows.delete(exe); }
  }
  // 进程行：仅在实际需要时移动/重建 DOM（每 220ms 重排会把鼠标下的节点
  // 拔出重插，打断 :hover 造成行背景闪烁）
  const listEl = $('#procList');
  let prevEl = null;
  for (const p of list) {
    const row = ensureRow(p.exe_path, p.name, p.exe_path, false);
    row.down.textContent = fmtRate(p.down_rate);
    row.up.textContent = fmtRate(p.up_rate);
    const chips = ruleChips(p.exe_path, false);
    if (row.ruleDataset !== chips) { row.rule.innerHTML = chips; row.ruleDataset = chips; }
    loadIcon(p.exe_path, row);
    if (row.el.previousElementSibling !== prevEl) {
      listEl.insertBefore(row.el, prevEl ? prevEl.nextSibling : listEl.firstChild);
    }
    prevEl = row.el;
  }
  // 「其他流量」兜底行固定在最后
  const other = ensureRow('__other__', '其他流量', '所有未被单独限速的程序', true);
  other.down.textContent = fmtRate(otherTraffic().down_rate);
  other.up.textContent = fmtRate(otherTraffic().up_rate);
  const otherChips = ruleChips(null, true);
  if (other.ruleDataset !== otherChips) { other.rule.innerHTML = otherChips; other.ruleDataset = otherChips; }
  if (listEl.lastElementChild !== other.el) listEl.appendChild(other.el);

  $('#countChip').textContent = `${snap.processes.length} 个程序`;
}

function otherTraffic() {
  // 其他流量速率 = 总速率 − 已有规则进程的速率（仅展示用）
  let d = snap.total_down_rate, u = snap.total_up_rate;
  for (const p of snap.processes) {
    if (snap.rules[p.exe_path]) { d -= p.down_rate; u -= p.up_rate; }
  }
  return { down_rate: Math.max(0, d), up_rate: Math.max(0, u) };
}

function clearLimits(exe, isOther) {
  if (isOther) {
    call('set_other_limits', { downBps: null, upBps: null });
  } else {
    call('clear_rule', { exe });
  }
  // 快照会带回新状态；本地先乐观刷新一次
  setTimeout(poll, 120);
}

// ── 限速弹窗 ──
function openModal(exe, isOther) {
  modalCtx = { exe, isOther };
  let name, path, down, up, blocked = false;
  if (isOther) {
    name = '其他流量'; path = '所有未被单独限速的程序';
    down = snap.other_down_bps; up = snap.other_up_bps;
  } else {
    const p = snap.processes.find((x) => x.exe_path === exe);
    name = p ? p.name : exe; path = exe;
    const r = ruleOf(exe);
    down = r ? r.down_limit : null; up = r ? r.up_limit : null; blocked = r ? !!r.blocked : false;
  }
  $('#modalTitle').textContent = name;
  $('#modalPath').textContent = path;
  $('#blockRow').hidden = isOther;
  setSwitch($('#blockSwitch'), blocked);
  fillLimitInput($('#modalDown'), down);
  fillLimitInput($('#modalUp'), up);
  $('#modalDownUnit').textContent = unitLabel();
  $('#modalUpUnit').textContent = unitLabel();
  $('#modalMask').hidden = false;
  $('#modalDown').focus();
}

function fillLimitInput(input, bps) {
  input.value = bps == null ? '' : String(+(bps / unitFactor()).toFixed(unit === 'mb' ? 2 : 1));
}

function setSwitch(el, on) { el.classList.toggle('on', !!on); el.setAttribute('aria-checked', on ? 'true' : 'false'); }

function parseLimitInput(input) {
  const s = input.value.trim();
  if (s === '') return null;                 // 不限
  const v = parseFloat(s);
  if (!isFinite(v) || v < 0) return null;
  return Math.round(v * unitFactor());       // 0 → Some(0) = 阻断该方向
}

function saveModal() {
  if (!modalCtx) return closeModal();
  const down = parseLimitInput($('#modalDown'));
  const up = parseLimitInput($('#modalUp'));
  if (modalCtx.isOther) {
    call('set_other_limits', { downBps: down, upBps: up });
  } else {
    call('set_rule', { rule: {
      exe_path: modalCtx.exe,
      down_limit: down,
      up_limit: up,
      blocked: $('#blockSwitch').classList.contains('on'),
      disabled: false,
    } });
  }
  closeModal();
  setTimeout(poll, 120);
}

function closeModal() {
  modalCtx = null;
  $('#modalMask').hidden = true;
}

// ── 轮询 ──
async function poll() {
  const dto = await call('get_snapshot');
  if (!dto) return;
  snap = dto;

  // 大数字 + 单位
  $('#downNum').textContent = fmtRate(dto.total_down_rate);
  $('#upNum').textContent = fmtRate(dto.total_up_rate);
  $('#downUnit').textContent = unitLabel();
  $('#upUnit').textContent = unitLabel();

  // 总开关
  setSwitch($('#masterSwitch'), dto.master);
  $('#masterLabel').textContent = dto.master ? '限速运行中' : '限速已暂停';
  const pill = $('#statusPill');
  const st = $('#statusText');
  if (dto.engine_error) {
    pill.classList.add('off'); st.textContent = '引擎未运行';
    $('#engineBanner').hidden = false;
    $('#engineErrorText').textContent = dto.engine_error;
  } else if (!dto.master) {
    pill.classList.add('off'); st.textContent = '限速已暂停（流量直通）';
  } else {
    pill.classList.remove('off'); st.textContent = '限速运行中';
  }

  // 提权横幅
  $('#elevateBanner').hidden = !!dto.elevated;

  // 每秒采样一次总速率进曲线
  const sec = Math.floor(Date.now() / 1000);
  if (sec !== hist.lastSec) {
    hist.lastSec = sec;
    hist.down.push(dto.total_down_rate);
    hist.up.push(dto.total_up_rate);
    if (hist.down.length > 60) { hist.down.shift(); hist.up.shift(); }
    drawGraph();
  }

  renderTable();

  // 诊断栏
  const g = dto.diagnostics || {};
  const pct = g.packets_recv ? Math.round((g.attributed / g.packets_recv) * 100) : 0;
  $('#diagBar').innerHTML =
    `<b>引擎诊断</b><span>包 <b>${g.packets_recv ?? 0}</b></span>` +
    `<span>转发 <b>${g.shaped ?? 0}</b></span>` +
    `<span>丢弃 <b>${g.dropped ?? 0}</b></span>` +
    `<span>进程识别 <b>${pct}%</b></span>` +
    `<span>TCP4 <b>${g.tcp4 ?? 0}</b></span><span>UDP4 <b>${g.udp4 ?? 0}</b></span>` +
    `<span>TCP6 <b>${g.tcp6 ?? 0}</b></span><span>UDP6 <b>${g.udp6 ?? 0}</b></span>`;
}

// ── 事件绑定 ──
$('#unitSeg').addEventListener('click', (e) => {
  const b = e.target.closest('[data-unit]');
  if (!b) return;
  unit = b.dataset.unit;
  localStorage.setItem('rl-unit', unit);
  $('#unitSeg').dataset.active = unit;
  $$('#unitSeg .seg-item').forEach((x) => x.classList.toggle('active', x === b));
  if (snap) { poll(); drawGraph(); }
});
$$('#unitSeg .seg-item').forEach((x) => x.classList.toggle('active', x.dataset.unit === unit));
$('#unitSeg').dataset.active = unit;

$('#masterSwitch').addEventListener('click', () => {
  const on = !$('#masterSwitch').classList.contains('on');
  setSwitch($('#masterSwitch'), on); // 乐观更新，快照会校正
  call('set_master', { on });
  setTimeout(poll, 150);
});

$('#search').addEventListener('input', (e) => { searchText = e.target.value; renderTable(); });

// 表头点击排序：下载/上传按当前速率降序；再点同一列取消，恢复默认（总速率）排序
$$('.proc-head .sortable').forEach((h) => h.addEventListener('click', () => {
  const key = h.dataset.sort;
  sortKey = sortKey === key ? null : key;
  $$('.proc-head .sortable').forEach((x) => x.classList.toggle('active', x.dataset.sort === sortKey));
  renderTable();
}));

$('#elevateBtn').addEventListener('click', () => call('relaunch_admin'));

$('#themeBtn').addEventListener('click', () => {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('rl-theme', next); } catch (e) {}
});

$('[data-win="min"]').addEventListener('click', () => call('win_action', { action: 'min' }));
$('[data-win="max"]').addEventListener('click', () => call('win_action', { action: 'max' }));
$('[data-win="close"]').addEventListener('click', () => call('win_action', { action: 'close' }));

$('#modalSave').addEventListener('click', saveModal);
$('#modalCancel').addEventListener('click', closeModal);
$('#modalClear').addEventListener('click', () => {
  if (!modalCtx) return;
  clearLimits(modalCtx.exe, modalCtx.isOther);
  closeModal();
});
$('#blockSwitch').addEventListener('click', () =>
  setSwitch($('#blockSwitch'), !$('#blockSwitch').classList.contains('on')));
$('#modalMask').addEventListener('mousedown', (e) => { if (e.target === e.currentTarget) closeModal(); });
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') closeModal();
  if (e.key === 'Enter' && !$('#modalMask').hidden) saveModal();
});

// 启动
poll();
setInterval(poll, 220);
