#!/usr/bin/env node
/**
 * check-css-vars.mjs — 扫描「被 var() 引用但从未定义」的 CSS 变量。
 *
 * 为什么需要它：浏览器对 var() 解析失败是静默的 —— 取不到值时整条 CSS 声明被丢弃，
 * 构建、类型检查、运行时不报任何错，界面只是"少了边框/背景/阴影"。本项目曾因此
 * 积累 38 个未定义变量。本脚本让这类问题可复跑、可定位。
 *
 * 用法：
 *   node tools/check-css-vars.mjs            # 检查，缺失即 exit 1
 *   node tools/check-css-vars.mjs --list     # 只打印清单（含使用点）
 *   node tools/check-css-vars.mjs --json     # 机器可读输出
 *
 * 退出码：0 = 无缺失（或全部在白名单内）；1 = 存在未定义变量。
 *
 * 判定范围：
 *   - 定义：所有 src/**\/*.{css,ts,tsx} 中的 `--name:` 声明，以及 `.ts/.tsx` 中
 *     `setProperty("--name", ...)` / `style="--name: ..."` 这类运行时注入。
 *   - 使用：所有 `var(--name` 引用。
 *   - 白名单：tools/css-vars-allowlist.json，每条必须写 reason；到期即失效需重新论证。
 */

import fs from "node:fs";
import path from "node:path";
import url from "node:url";

const ROOT = path.resolve(path.dirname(url.fileURLToPath(import.meta.url)), "..");
const SRC = path.join(ROOT, "src");
const ALLOWLIST = path.join(ROOT, "tools", "css-vars-allowlist.json");

const EXTS = new Set([".css", ".ts", ".tsx"]);

function walk(dir, out = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "node_modules" || entry.name === "dist") continue;
      walk(full, out);
    } else if (EXTS.has(path.extname(entry.name))) {
      out.push(full);
    }
  }
  return out;
}

const DECL_RE = /(^|[\s;{'"`])(--[A-Za-z0-9_-]+)\s*:/g;
const USE_RE = /var\(\s*(--[A-Za-z0-9_-]+)/g;

/**
 * 运行时注入的三种真实写法，全部来自本仓库现有代码：
 *   a) `el.style.setProperty("--x", ...)`
 *   b) React 的 style 对象键：`["--x"]: value` 或 `["--x" as string]: value`
 *   c) 模板字符串里的 `--x: value`（组件内 `<style>{`...`}</style>`）
 * 识别到任意一种，就把该变量记为"已由运行时提供"，不再要求静态定义。
 * 这样做比手写白名单更强：新增一个注入点会被自动认出来，而白名单一旦写歪
 * （例如把 `--bg-main` 也豁免掉）就会掩盖真缺陷。
 */
const INJECT_RES = [
  /setProperty\(\s*["'`](--[A-Za-z0-9_-]+)["'`]/g,
  /\[\s*["'`](--[A-Za-z0-9_-]+)["'`]\s*(?:as\s+[A-Za-z<>\[\]\s|]+)?\]\s*:/g,
  /(?:^|[\s;{])["'`]?(--[A-Za-z0-9_-]+)["'`]?\s*:\s*[^;{}]*[`'"],\s*$/gm,
];

const defined = new Map(); // name -> [relpath:line]
const used = new Map(); // name -> [relpath:line]
const injected = new Map(); // name -> [relpath:line]

for (const file of walk(SRC)) {
  const rel = path.relative(ROOT, file);
  const text = fs.readFileSync(file, "utf8");
  for (const [i, line] of text.split(/\r?\n/).entries()) {
    for (const m of line.matchAll(DECL_RE)) {
      if (!defined.has(m[2])) defined.set(m[2], []);
      defined.get(m[2]).push(`${rel}:${i + 1}`);
    }
    for (const m of line.matchAll(USE_RE)) {
      if (!used.has(m[1])) used.set(m[1], []);
      used.get(m[1]).push(`${rel}:${i + 1}`);
    }
    for (const re of INJECT_RES) {
      re.lastIndex = 0;
      for (const m of line.matchAll(re)) {
        const name = m[1];
        if (!name) continue;
        if (!injected.has(name)) injected.set(name, []);
        injected.get(name).push(`${rel}:${i + 1}`);
      }
    }
  }
}

let allow = { vars: [] };
if (fs.existsSync(ALLOWLIST)) {
  allow = JSON.parse(fs.readFileSync(ALLOWLIST, "utf8"));
}
const allowByName = new Map((allow.vars || []).map((v) => [v.name, v]));

/**
 * 一条使用点是否"有 fallback"。这决定了缺陷的严重级别：
 *   `var(--x, fallback)`  → 变量缺失时用 fallback，样式仍生效（降级，有兜底）
 *   `var(--x)`            → 变量缺失时**整条声明被丢弃**（静默消失，最危险）
 *   `rgba(var(--x), a)`   → 即使写了 fallback 也救不了：整体是 `rgba(<非三元组>, a)`，
 *                           解析失败 → 整条声明被丢弃（本次 `--bg-panel-rgb` 的类型）
 * 所以"无 fallback"和"三元组位置"的缺失**不允许**被白名单豁免——那正是本次缺陷
 * 的形态。白名单只受理"确实有兜底、且兜底就是设计意图"的降级用法。
 */
const noFallbackSites = (sites) => sites.filter((s) => !hasFallback(s));
const FB_CACHE = new Map();
function hasFallback(site) {
  if (FB_CACHE.has(site)) return FB_CACHE.get(site);
  const [rel, lineStr] = site.split(":");
  const line = fs.readFileSync(path.join(ROOT, rel), "utf8").split(/\r?\n/)[Number(lineStr) - 1] ?? "";
  let v = false;
  for (const m of line.matchAll(/var\(\s*(--[A-Za-z0-9_-]+)([^)]*)\)/g)) {
    if (m[2].trimStart().startsWith(",")) v = true;
  }
  // `rgba(var(--x), a)` 形式：变量处在颜色函数里，fallback 救不回来
  if (/rgba?\(\s*var\(/.test(line)) v = false;
  FB_CACHE.set(site, v);
  return v;
}

const missing = [];
const allowed = [];
const illegalAllow = [];
for (const [name, sites] of [...used].sort((a, b) => a[0].localeCompare(b[0]))) {
  if (defined.has(name)) continue;
  if (injected.has(name)) continue;
  const entry = allowByName.get(name);
  const dangerous = noFallbackSites(sites);
  if (entry) {
    if (dangerous.length) {
      // 有站点会在变量缺失时静默丢掉整条声明 —— 这种豁免不成立
      illegalAllow.push({ name, reason: entry.reason, sites: dangerous });
    } else {
      allowed.push({ name, reason: entry.reason, sites });
    }
    continue;
  }
  missing.push({ name, sites: dangerous.length ? dangerous : sites, silent: dangerous.length > 0 });
}

// 反向检查：白名单里已经不再使用的项，属于过期豁免，应删掉。
const staleAllow = (allow.vars || []).filter((v) => !used.has(v.name)).map((v) => v.name);

const json = process.argv.includes("--json");
const listOnly = process.argv.includes("--list");

if (json) {
  console.log(JSON.stringify({ usedCount: used.size, definedCount: defined.size, injectedCount: injected.size, missing, allowed, illegalAllow, staleAllow }, null, 2));
} else {
  console.log(`[css-vars] 被引用变量 ${used.size} 个，静态定义 ${defined.size} 个，运行时注入 ${injected.size} 个`);
  if (missing.length) {
    console.error(`\n[css-vars] ✗ 发现 ${missing.length} 个未定义变量：`);
    for (const m of missing) {
      console.error(`  ${m.name}${m.silent ? "   ← 无 fallback：变量缺失时整条声明被丢弃" : ""}`);
      for (const s of m.sites) console.error(`      ${s}`);
    }
  } else {
    console.log("[css-vars] ✓ 无未定义变量");
  }
  if (illegalAllow.length) {
    console.error(`\n[css-vars] ✗ 以下 ${illegalAllow.length} 项被白名单豁免，但存在「无 fallback」的使用点：`);
    console.error("  这类用法在变量缺失时会静默丢弃整条样式声明，不允许豁免。");
    for (const a of illegalAllow) {
      console.error(`  ${a.name}`);
      for (const s of a.sites) console.error(`      ${s}`);
    }
  }
  if (allowed.length && !listOnly) {
    console.log(`\n[css-vars] 白名单豁免 ${allowed.length} 个（均有 fallback 兜底，且理由已写明）：`);
    for (const a of allowed) console.log(`  ${a.name} — ${a.reason}`);
  }
  if (staleAllow.length) {
    console.error(`\n[css-vars] ✗ 白名单中有 ${staleAllow.length} 项已不再被引用，请删除：${staleAllow.join(", ")}`);
  }
}

const failed = missing.length > 0 || illegalAllow.length > 0 || staleAllow.length > 0;
process.exit(failed ? 1 : 0);
