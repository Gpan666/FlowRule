/** 验证「设置限速 → 配置真的产生非零速率 → 引擎诊断」链路。
 *
 * 用户反馈「能切换但没效果」的根因候选：
 *  - master=false 时 current_config() 直接返回 unlimited()，引擎不开句柄
 *  - DirState 默认 enabled=false，界面高亮挡位却算不出速率
 *  - 只接管 IPv4，IPv6/QUIC 流量漏掉
 * 本脚本通过 snapshot 的 other/detail 字段验证速率真的非零，并读 engine_diag。
 */
import { spawn } from 'node:child_process';
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
  env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}` } });
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
    res.initial = { master: s0.master, other_down_bytes: s0.other.down.bytes,
                    other_down_short: s0.other.down.short, diag: s0.engine_diag };

    // 场景 A：只点总开关（不设任何挡位）
    await inv('set_master', { on: true });
    await new Promise(r => setTimeout(r, 700));
    const sA = await snap();
    res.onlyMaster = { master: sA.master, other_down_bytes: sA.other.down.bytes,
                       other_down_short: sA.other.down.short,
                       other_down_enabled: sA.other.down.enabled,
                       status: sA.status, diag: sA.engine_diag, warning: sA.warning };

    // 场景 B：只点挡位（总开关先关掉）
    await inv('set_master', { on: false });
    await inv('set_rule', { edit: { target: 'other', direction: 'down', profile_id: 3 } });
    await new Promise(r => setTimeout(r, 700));
    const sB = await snap();
    res.onlyChip = { master: sB.master, other_down_bytes: sB.other.down.bytes,
                     other_down_short: sB.other.down.short, status: sB.status, diag: sB.engine_diag };

    // 场景 C：开上行方向开关
    await inv('set_rule', { edit: { target: 'other', direction: 'up', enabled: true, profile_id: 5 } });
    await new Promise(r => setTimeout(r, 700));
    const sC = await snap();
    res.withUp = { master: sC.master, other_up_bytes: sC.other.up.bytes,
                   other_up_short: sC.other.up.short, status: sC.status, diag: sC.engine_diag };
    return res;
  })()`, 30000);

  console.log(JSON.stringify(out, null, 2));
  const okA = out.onlyMaster.master && out.onlyMaster.other_down_bytes > 0 && out.onlyMaster.other_down_enabled;
  const okB = out.onlyChip.master && out.onlyChip.other_down_bytes > 0;
  const okC = out.withUp.other_up_bytes > 0;
  console.log(`\n[${okA ? 'PASS' : 'FAIL'}] 只点总开关 → 自动产生生效速率`);
  console.log(`[${okB ? 'PASS' : 'FAIL'}] 只点挡位 → 自动开启总开关且速率非零`);
  console.log(`[${okC ? 'PASS' : 'FAIL'}] 上行方向开关 → 速率非零`);
  console.log(okA && okB && okC ? 'RESULT: PASS 配置链路已打通' : 'RESULT: FAIL');
} catch (e) {
  console.log('ERROR:', e.message);
} finally {
  try { ws?.close(); } catch {}
  child.kill();
}
