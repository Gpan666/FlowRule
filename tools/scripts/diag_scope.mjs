/** 复现「单限某程序却整机限速 / 设 20MB 只有 1MB」。
 * 重点看设置程序规则后，other（其他流量）规则是否仍然 enabled 且速率为 1MB/s。
 */
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'dist');
const PORT = 9377;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function target() {
  for (let i = 0; i < 60; i++) {
    try {
      const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const p = l.find((t) => t.type === 'page' && t.webSocketDebuggerUrl && (t.url || '').includes('tauri'));
      if (p) return p.webSocketDebuggerUrl;
    } catch {}
    await sleep(400);
  }
  return null;
}
let id = 1;
function ev(ws, expression, timeout = 20000) {
  const my = id++;
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('timeout')), timeout);
    const f = (e) => {
      let m; try { m = JSON.parse(e.data); } catch { return; }
      if (m.id !== my) return;
      clearTimeout(t); ws.removeEventListener('message', f);
      resolve(m.result?.result?.value);
    };
    ws.addEventListener('message', f);
    ws.send(JSON.stringify({ id: my, method: 'Runtime.evaluate',
      params: { expression, awaitPromise: true, returnByValue: true } }));
  });
}
const child = spawn(join(DIST, 'rl-app.exe'), [], { cwd: DIST, stdio: 'ignore',
  env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}`,
    WEBVIEW2_USER_DATA_FOLDER: mkdtempSync(join(tmpdir(), 'rl-cdp-')) } });
let ws;
try {
  const url = await target();
  ws = await new Promise((res, rej) => { const w = new WebSocket(url); w.onopen = () => res(w); w.onerror = rej; });
  ws.send(JSON.stringify({ id: id++, method: 'Runtime.enable', params: {} }));
  for (let i = 0; i < 30; i++) { if (await ev(ws, 'typeof window.__TAURI_INTERNALS__') === 'object') break; await sleep(400); }
  await sleep(1200);
  const out = await ev(ws, `(async () => {
    const inv = window.__TAURI_INTERNALS__.invoke;
    const S = () => inv('get_snapshot');
    const r = {};
    // 场景：用户先点总开关，再切 MB/s，给某个程序设 20 MB/s 自定义
    await inv('set_master', { on: true });
    await inv('set_unit', { unit: 'mb' });
    const s0 = await S();
    r.afterMaster = { other_down: s0.other.down.short, other_down_bytes: s0.other.down.bytes,
                      other_down_enabled: s0.other.down.enabled, master: s0.master };
    const prog = s0.programs.find(p => p.key && p.pids > 0 && !p.key.startsWith('name::')) || s0.programs[0];
    await inv('select_program', { key: prog.key });
    await inv('set_rule', { edit: { target: 'program', key: prog.key, direction: 'down', custom: true } });
    await inv('set_rule', { edit: { target: 'program', key: prog.key, direction: 'down', custom_value: '20' } });
    await new Promise(r => setTimeout(r, 900));
    const s1 = await S();
    r.afterProgram20MB = {
      program: prog.name,
      program_down: s1.detail?.down_rule?.short, program_down_bytes: s1.detail?.down_rule?.bytes,
      other_down: s1.other.down.short, other_down_bytes: s1.other.down.bytes,
      other_down_enabled: s1.other.down.enabled,
      master: s1.master, status: s1.status, diag: s1.engine_diag,
    };
    return r;
  })()`, 30000);
  console.log(JSON.stringify(out, null, 2));
  const leak = out.afterProgram20MB.other_down_enabled && out.afterProgram20MB.other_down_bytes > 0;
  const progOk = out.afterProgram20MB.program_down_bytes === 20971520;
  console.log(`\n[${leak ? 'FAIL' : 'PASS'}] 设为程序 20MB/s 后，自动默认的其他流量兜底已移除`);
  console.log(`[${progOk ? 'PASS' : 'FAIL'}] 程序规则速率为 20MB/s (${out.afterProgram20MB.program_down})`);
  console.log(`[${out.afterProgram20MB.master ? 'PASS' : 'FAIL'}] 总开关已自动开启`);
  console.log(leak
    ? `  ⚠ 仍有全局兜底：${out.afterProgram20MB.other_down}（会把整机限到这个值）`
    : '  生效桶应只剩该程序的 20MB/s，其他程序流量完全放行');
  console.log(`  diag: ${out.afterProgram20MB.diag.replace(/\n/g, ' | ')}`);
  console.log(!leak && progOk && out.afterProgram20MB.master ? '\nRESULT: PASS' : '\nRESULT: FAIL');
} catch (e) { console.log('ERROR:', e.message); }
finally { try { ws?.close(); } catch {} child.kill(); }
