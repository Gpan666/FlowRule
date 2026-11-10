/** 卡死回归验证：直接对 Tauri 命令计时，检测任何命令是否会挂起。
 *
 * 原理：WebView2 的 renderer 是独立进程，Rust 主线程被死锁卡住时 JS 定时器仍能跑，
 * 所以用 Promise.race 给每个 invoke 套 3 秒超时，能精确区分「返回」与「永久挂起」。
 * 同时校验 config/settings.json 是否落盘，确认 persist() 真的执行到了。
 *
 * 坑：CDP 首个 target 是 about:blank，随后会导航到 tauri.localhost 并清空执行上下文。
 * 必须等导航完成、Runtime.enable、并确认 __TAURI_INTERNALS__ 就绪后再发 evaluate。
 *
 * 用法：node tools/verify_no_freeze.mjs
 */
import { spawn } from 'node:child_process';
import { existsSync, readFileSync, unlinkSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'dist');
const EXE = join(DIST, 'rl-app.exe');
const SETTINGS = join(DIST, 'config', 'settings.json');
const PORT = 9333;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function findTarget() {
  for (let i = 0; i < 60; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${PORT}/json/list`);
      const list = await res.json();
      // 只认已导航到 tauri 页面的 target，避免连到随即被销毁的 about:blank 上下文
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl
        && (t.url || '').includes('tauri'));
      if (page) return page.webSocketDebuggerUrl;
    } catch { /* not up yet */ }
    await sleep(400);
  }
  return null;
}

function connect(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    ws.onopen = () => resolve(ws);
    ws.onerror = (e) => reject(new Error('websocket failed: ' + e.message));
  });
}

let nextId = 1;
async function evaluate(ws, expression, timeoutMs = 12000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('CDP evaluate timeout (Rust side hung)')), timeoutMs);
    const onMsg = (ev) => {
      let m;
      try { m = JSON.parse(ev.data); } catch { return; }
      if (m.id !== id) return;
      clearTimeout(timer);
      ws.removeEventListener('message', onMsg);
      if (m.error) return reject(new Error(JSON.stringify(m.error)));
      const r = m.result;
      if (r?.exceptionDetails) return resolve({ exception: r.exceptionDetails.text });
      resolve(r?.result?.value);
    };
    ws.addEventListener('message', onMsg);
    ws.send(JSON.stringify({
      id, method: 'Runtime.evaluate',
      params: { expression, awaitPromise: true, returnByValue: true },
    }));
  });
}

async function main() {
  if (existsSync(SETTINGS)) unlinkSync(SETTINGS);
  console.log('launching', EXE, 'with CDP port', PORT);
  const child = spawn(EXE, [], {
    cwd: DIST,
    stdio: 'ignore',
    env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}` },
  });

  let ws;
  try {
    const url = await findTarget();
    if (!url) throw new Error('CDP target not found (page never navigated to tauri.localhost)');
    console.log('CDP target found (tauri page ready)');
    ws = await connect(url);
    // 显式发 Runtime.enable 并忽略响应（它带的是 method 而非表达式）
    ws.send(JSON.stringify({ id: nextId++, method: 'Runtime.enable', params: {} }));

    let ready = false;
    for (let i = 0; i < 30; i++) {
      const v = await evaluate(ws, 'typeof window.__TAURI_INTERNALS__', 5000).catch(() => null);
      if (v === 'object') { ready = true; break; }
      await sleep(400);
    }
    if (!ready) throw new Error('__TAURI_INTERNALS__ never became ready');
    console.log('Tauri API ready');
    await sleep(800);

    // 每个命令独立计时：Rust 主线程死锁时 invoke 的 promise 永不 settle，
    // 但 renderer 独立进程里的 setTimeout 仍会触发 → 得到 TIMEOUT 而非整体卡住。
    const tests = [
      ['get_snapshot', 'get_snapshot', {}],
      ['set_unit(MB/s)', 'set_unit', { unit: 'mb' }],   // 曾经自死锁的元凶
      ['get_snapshot#2', 'get_snapshot', {}],
      ['set_rule(chip)', 'set_rule', { edit: { target: 'other', direction: 'down', profile_id: 3 } }],
      ['set_master(on)', 'set_master', { on: true }],
      ['set_unit(KB/s)', 'set_unit', { unit: 'kb' }],
      ['get_snapshot#3', 'get_snapshot', {}],
      ['set_unit(MB/s)#2', 'set_unit', { unit: 'mb' }],
      ['get_snapshot#4', 'get_snapshot', {}],
    ];
    const expr = `(async () => {
      const tests = ${JSON.stringify(tests)};
      const out = [];
      for (const [name, cmd, args] of tests) {
        const t0 = performance.now();
        const r = await Promise.race([
          window.__TAURI_INTERNALS__.invoke(cmd, args).then(
            (v) => 'OK', (e) => 'ERR:' + e),
          new Promise((res) => setTimeout(() => res('TIMEOUT'), 3000)),
        ]);
        out.push({ name, result: r, ms: Math.round(performance.now() - t0) });
      }
      return out;
    })()`;

    const rows = await evaluate(ws, expr, 30000);
    if (!Array.isArray(rows)) {
      console.log('BAD RESULT:', JSON.stringify(rows));
      return 2;
    }
    for (const r of rows) {
      const flag = r.result === 'OK' ? 'PASS' : 'FAIL';
      console.log(`  [${flag}] ${r.name.padEnd(20)} ${String(r.result).padEnd(10)} ${r.ms}ms`);
    }
    const hung = rows.filter((r) => r.result !== 'OK');

    await sleep(600);
    let persisted = null;
    try { persisted = JSON.parse(readFileSync(SETTINGS, 'utf8')); } catch { /* missing */ }
    const persistOk = persisted && persisted.unit === 'mb' && persisted.master === true
      && persisted.other?.down?.sel === 'p3' && persisted.other?.down?.enabled === true;
    console.log(`  [${persistOk ? 'PASS' : 'FAIL'}] settings.json 落盘: ${persisted ? JSON.stringify({ unit: persisted.unit, master: persisted.master, other_down: persisted.other?.down?.sel, down_enabled: persisted.other?.down?.enabled }) : 'NOT FOUND'}`);

    const ok = hung.length === 0 && persistOk;
    console.log(ok ? '\nRESULT: PASS — 所有限速相关命令均正常返回，无卡死' : '\nRESULT: FAIL — 存在挂起命令或持久化失败');
    return ok ? 0 : 1;
  } catch (e) {
    console.log('ERROR:', e.message);
    return 2;
  } finally {
    try { ws?.close(); } catch { /* noop */ }
    child.kill();
  }
}

process.exit(await main());
