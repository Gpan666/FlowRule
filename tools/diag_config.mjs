/** 验证配置链路（v2 设计：总开关只管启停，绝不自动限整机流量）。
 *
 * 覆盖：
 *  - 只点总开关 → master=true 但「其他流量」保持不限速（不会偷偷限整机）
 *  - 只点挡位 → 自动开总开关且该挡位速率非零
 *  - 开上行 → 上行速率非零
 *  - 只限某程序 → 该程序桶生效、「其他流量」保持不限速（本任务的核心修复点）
 */
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'dist');
const PORT = 9366;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function findTarget() {
  for (let i = 0; i < 60; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const p = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl && (t.url || '').includes('tauri'));
      if (p) return p.webSocketDebuggerUrl;
    } catch {}
    await sleep(400);
  }
  return null;
}

let id = 1;
function ev(ws, expression, timeout = 15000) {
  const my = id++;
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('timeout')), timeout);
    const onMsg = (e) => {
      let m; try { m = JSON.parse(e.data); } catch { return; }
      if (m.id !== my) return;
      clearTimeout(t); ws.removeEventListener('message', onMsg);
      resolve(m.result?.result?.value ?? m.error);
    };
    ws.addEventListener('message', onMsg);
    ws.send(JSON.stringify({ id: my, method: 'Runtime.evaluate',
      params: { expression, awaitPromise: true, returnByValue: true } }));
  });
}

const child = spawn(join(DIST, 'rl-app.exe'), [], { cwd: DIST, stdio: 'ignore',
  env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}`,
    WEBVIEW2_USER_DATA_FOLDER: mkdtempSync(join(tmpdir(), 'rl-cdp-')) } });
let ws;
try {
  const url = await findTarget();
  ws = await new Promise((res, rej) => { const w = new WebSocket(url); w.onopen = () => res(w); w.onerror = rej; });
  ws.send(JSON.stringify({ id: id++, method: 'Runtime.enable', params: {} }));
  for (let i = 0; i < 30; i++) {
    if (await ev(ws, 'typeof window.__TAURI_INTERNALS__') === 'object') break;
    await sleep(400);
  }
  await sleep(1000);

  const out = await ev(ws, `(async () => {
    const inv = window.__TAURI_INTERNALS__.invoke;
    const snap = () => inv('get_snapshot');
    const res = {};
    const s0 = await snap();
    res.initial = { master: s0.master, other_down_bytes: s0.other.down.bytes, diag: s0.engine_diag };

    // 场景 A：只点总开关（不设任何挡位 / 不碰其他流量）
    // 期望：master=true，但「其他流量」保持不限速（bytes=0），绝不偷偷限整机
    await inv('set_master', { on: true });
    await new Promise(r => setTimeout(r, 700));
    const sA = await snap();
    res.onlyMaster = { master: sA.master, other_down_bytes: sA.other.down.bytes,
                       other_down_enabled: sA.other.down.enabled,
                       status: sA.status, diag: sA.engine_diag, warning: sA.warning };

    // 场景 B：把总开关关掉，只点「其他流量」下行挡位（profile 3 = 256KB/s）
    // 期望：自动开总开关 + 该挡位速率非零
    await inv('set_master', { on: false });
    await inv('set_rule', { edit: { target: 'other', direction: 'down', profile_id: 3 } });
    await new Promise(r => setTimeout(r, 700));
    const sB = await snap();
    res.onlyChip = { master: sB.master, other_down_bytes: sB.other.down.bytes,
                     other_down_short: sB.other.down.short, status: sB.status, diag: sB.engine_diag };

    // 场景 C：再开上行方向（profile 5 = 1MB/s）
    await inv('set_rule', { edit: { target: 'other', direction: 'up', enabled: true, profile_id: 5 } });
    await new Promise(r => setTimeout(r, 700));
    const sC = await snap();
    res.withUp = { master: sC.master, other_up_bytes: sC.other.up.bytes,
                   other_up_short: sC.other.up.short, status: sC.status, diag: sC.engine_diag };

    // 场景 D（核心修复）：只限某程序（ADU 下行 20MB/s），总开关不碰、其他流量不碰
    // 期望：该程序桶生效（detail.down_rule=20MB/s），「其他流量」保持不限速（bytes=0），整台电脑放行
    await inv('set_master', { on: false });
    await inv('set_rule', { edit: { target: 'other', direction: 'down', enabled: false } });
    await inv('set_rule', { edit: { target: 'other', direction: 'up', enabled: false } });
    await inv('set_unit', { unit: 'mb' });
    await new Promise(r => setTimeout(r, 300));
    await inv('select_program', { key: 'PROG::ADU' });
    await inv('set_rule', { edit: { target: 'program', key: 'PROG::ADU', direction: 'down', custom: true, custom_value: '20' } });
    await new Promise(r => setTimeout(r, 700));
    const sD = await snap();
    res.onlyProgram = {
      master: sD.master,
      unit: sD.unit,
      other_down_bytes: sD.other.down.bytes,
      other_down_enabled: sD.other.down.enabled,
      selected: sD.selected,
      program_down_bytes: sD.detail && sD.detail.down_rule ? sD.detail.down_rule.bytes : -1,
      program_down_short: sD.detail && sD.detail.down_rule ? sD.detail.down_rule.short : null,
      status: sD.status, diag: sD.engine_diag };
    return res;
  })()`, 30000);

  console.log(JSON.stringify(out, null, 2));
  const okA = out.onlyMaster.master && out.onlyMaster.other_down_bytes === 0 && !out.onlyMaster.other_down_enabled;
  const okB = out.onlyChip.master && out.onlyChip.other_down_bytes > 0;
  const okC = out.withUp.other_up_bytes > 0;
  const okD = out.onlyProgram.master && out.onlyProgram.program_down_bytes > 0 && out.onlyProgram.other_down_bytes === 0;
  const msg = (b, t) => console.log(`[${b ? 'PASS' : 'FAIL'}] ${t}`);
  msg(okA, '只点总开关 → 不会自动限整机（其他流量保持不限速）');
  msg(okB, '只点挡位 → 自动开启总开关且速率非零');
  msg(okC, '上行方向开关 → 速率非零');
  msg(okD, '只限某程序 → 该程序桶生效且其他流量保持不限速（核心修复）');
  console.log(okA && okB && okC && okD ? 'RESULT: PASS 配置链路符合 v2 设计' : 'RESULT: FAIL');
} catch (e) {
  console.log('ERROR:', e.message);
} finally {
  try { ws?.close(); } catch {}
  child.kill();
}
