/**
 * 回归验证：旧版本遗留的「其他流量」自动默认限速，升级后必须被清除。
 *
 * 背景（用户报告的 bug）：
 *   旧版（v1/v2/v3）在「点总开关」或「单限某个程序」时，会自动给「其他流量」套 1 MB/s
 *   默认挡位。这个兜底桶把**所有没被识别归属的流量**（含 IPv6、连接表刷新窗口、
 *   本机其它程序）全压到 1 MB/s，于是：
 *     - 单限某个程序 → 整台电脑的网络被限速
 *     - 设 20 MB/s    → 实测只有 1 MB/s
 *
 *   旧迁移逻辑带 `rules.is_empty()` 守卫，只在「没有任何程序规则」时才清除，
 *   漏掉了用户真实场景（既有程序规则、又有遗留「其他流量」）→ 这个 1 MB 兜底桶
 *   被原样存成 v3 保留下来，bug 一直复现。
 *
 * 本脚本模拟真实落盘配置 → 启动 exe → 检查迁移结果。
 */

import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, copyFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = dirname(fileURLToPath(import.meta.url)) + '/..';
const DIST = join(ROOT, 'dist');
const WORK = mkdtempSync(join(tmpdir(), 'rl-mig-'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function fail(msg) {
  console.log(`[FAIL] ${msg}`);
  process.exitCode = 1;
}
function pass(msg) {
  console.log(`[PASS] ${msg}`);
}

// ── 准备隔离运行目录（exe 同目录 config/settings.json 才是程序真正读的路径）──
mkdirSync(join(WORK, 'config'), { recursive: true });
for (const f of readdirSync(DIST)) {
  if (/\.(exe|dll|sys)$/i.test(f)) copyFileSync(join(DIST, f), join(WORK, f));
}

const legacy = {
  version: 2,
  unit: 'mb',
  master: true,
  // 旧版自动塞的 1 MB/s 兜底（p5 = 1MB），没有 custom 文本 → 属遗留自动默认
  other: {
    down: { enabled: true, sel: 'p5', custom: '' },
    up: { enabled: false, sel: '', custom: '' },
  },
  // 用户自己设的程序规则：20 MB/s 自定义
  rules: [
    ['C:/ADU.exe', {
      down: { enabled: true, sel: 'custom', custom: '20' },
      up: { enabled: false, sel: '', custom: '' },
    }],
  ],
  selected: null,
};
writeFileSync(join(WORK, 'config', 'settings.json'), JSON.stringify(legacy, null, 2), 'utf8');

console.log('─ 遗留配置（v2）：其他流量 p5 已启用 + 1 条程序规则 ─');

// ── 启动 exe：迁移发生在 main() 里（Tauri 之前），不依赖 UI 是否起得来 ──
const udf = mkdtempSync(join(tmpdir(), 'rl-cdp-'));
const child = spawn(join(WORK, 'rl-app.exe'), [], {
  cwd: WORK,
  stdio: 'ignore',
  env: {
    ...process.env,
    WEBVIEW2_USER_DATA_FOLDER: udf,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: '--remote-debugging-port=9399',
  },
});
child.on('error', (e) => console.log('spawn error', e.message));

try {
  await sleep(6000);
} finally {
  try { child.kill(); } catch { /* ignore */ }
}

// ── 检查迁移结果 ──
const cfgPath = join(WORK, 'config', 'settings.json');
let cfg;
try {
  cfg = JSON.parse(readFileSync(cfgPath, 'utf8'));
} catch (e) {
  fail(`无法读取迁移后的配置：${e.message}`);
  try { rmSync(WORK, { recursive: true, force: true }); } catch { /* ignore */ }
  process.exit(1);
}

console.log('迁移后：', JSON.stringify(cfg));

if (cfg.version === 4) pass('版本已升级到 v4');
else fail(`版本未升级（仍是 v${cfg.version}）`);

if (cfg.other.down.enabled || cfg.other.up.enabled) {
  fail(`「其他流量」仍在限速（down=${cfg.other.down.enabled} up=${cfg.other.up.enabled}）` +
       ' → 这就是「单限某程序却整台被限」的元凶');
} else {
  pass('「其他流量」遗留的 1 MB/s 兜底已被清除');
}

if (Array.isArray(cfg.rules) && cfg.rules.length === 1
    && cfg.rules[0][1].down.custom === '20' && cfg.rules[0][1].down.enabled) {
  pass('用户的程序规则（20 MB/s）原样保留');
} else {
  fail('程序规则被错误地清除了');
}

// 清理隔离目录：exe 刚退出可能仍占用句柄，等一会儿再删；删不掉也不影响结论
await sleep(1200);
try {
  rmSync(WORK, { recursive: true, force: true, maxRetries: 5, retryDelay: 300 });
} catch {
  console.log(`(提示) 临时目录未能清理，可手动删除：${WORK})`);
}
console.log(process.exitCode ? '\nRESULT: FAIL' : '\nRESULT: PASS 遗留「其他流量」限速已清除');
