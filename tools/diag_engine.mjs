/** 引擎错误诊断：设置一条真实程序规则 + 开启总开关，读取完整 status（含 Win32 错误码）。
 *  UI 上 status-pill 有 ellipsis 截断，用户截图看不到错误码，这里直接从 snapshot 拿全文。
 */
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'dist');
const PORT = 9355;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function findTarget() {
  for (let i = 0; i < 60; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl && (t.url || '').includes('tauri'));
      if (page) return page.webSocketDebuggerUrl;
    } catch {}
    await sleep(400);
  }
  return null;
}

let nextId = 1;
function evaluate(ws, expression, timeoutMs = 12000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('evaluate timeout')), timeoutMs);
    const onMsg = (ev) => {
      let m; try { m = JSON.parse(ev.data); } catch { return; }
      if (m.id !== id) return;
      clearTimeout(timer);
      ws.removeEventListener('message', onMsg);
      if (m.error) return reject(new Error(JSON.stringify(m.error)));
      resolve(m.result?.result?.value);
    };
    ws.addEventListener('message', onMsg);
    ws.send(JSON.stringify({ id, method: 'Runtime.evaluate',
      params: { expression, awaitPromise: true, returnByValue: true } }));
  });
}

const child = spawn(join(DIST, 'rl-app.exe'), [], {
  cwd: DIST, stdio: 'ignore',
  env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}`,
    WEBVIEW2_USER_DATA_FOLDER: mkdtempSync(join(tmpdir(), 'rl-cdp-')) },
});
let ws;
try {
  const url = await findTarget();
  if (!url) throw new Error('no CDP target');
  ws = await new Promise((res, rej) => { const w = new WebSocket(url); w.onopen = () => res(w); w.onerror = rej; });
  ws.send(JSON.stringify({ id: nextId++, method: 'Runtime.enable', params: {} }));
  for (let i = 0; i < 30; i++) {
    if (await evaluate(ws, 'typeof window.__TAURI_INTERNALS__', 5000).catch(() => null) === 'object') break;
    await sleep(400);
  }
  await sleep(1200);

  const result = await evaluate(ws, `(async () => {
    const inv = window.__TAURI_INTERNALS__.invoke;
    const out = {};
    const before = await inv('get_snapshot');
    out.elevated = before.elevated;
    out.program_count = before.program_count;
    // 找一个有 pid 的真实程序来设规则
    const prog = before.programs.find(p => p.key && p.key.length > 3) || before.programs[0];
    out.target_key = prog ? prog.key : null;
    out.pids = prog ? prog.pid_list : null;
    // 先设「其他流量」规则 + 程序规则，再开总开关
    out.set_other = await inv('set_rule', { edit: { target: 'other', direction: 'down', profile_id: 5 } }).then(()=> 'OK', e => 'ERR:'+e);
    if (prog) {
      out.set_prog = await inv('set_rule', { edit: { target: 'program', key: prog.key, direction: 'down', profile_id: 5 } }).then(()=> 'OK', e => 'ERR:'+e);
    }
    out.set_master = await inv('set_master', { on: true }).then(()=> 'OK', e => 'ERR:'+e);
    await new Promise(r => setTimeout(r, 2500));
    const after = await inv('get_snapshot');
    out.status = after.status;
    out.warning = after.warning;
    out.master = after.master;
    return out;
  })()`, 30000);

  console.log(JSON.stringify(result, null, 2));
} catch (e) {
  console.log('ERROR:', e.message);
} finally {
  try { ws?.close(); } catch {}
  child.kill();
}
