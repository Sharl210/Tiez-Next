/**
 * migmeasure.mjs — 量测「迁移进度条 + 迁移结果卡片」的真实渲染几何与配色。
 *
 * # 为什么需要单独量
 *
 * 组件测试断言的是**类名与文案**；类名对不代表颜色对、更不代表真的画得出来。
 * 本仓库已有同类教训：`var()` 取不到值时整条声明被静默丢弃，DOM 断言全绿而界面上
 * 什么都没有。所以这里读 `getComputedStyle` 的真实值：
 *   - 轨道高度/背景、填充宽度是不是真按百分比算出来的像素；
 *   - 成败边框色的**实际 RGB** 是不是绿色系（不是红色系）；
 *   - `total === 0` 时填充有没有可见宽度（必须能看到"在动"，而不是一根 0px 空轨）。
 *
 * # 边界
 *
 * 验证台的 `invoke` 是 mock，`listen` 是 no-op —— **只证明渲染与样式，
 * 不证明真实事件时序**。事件时序由组件测试的模拟序列覆盖，真机时序两端都未验证。
 *
 * 用法：node tools/visual-harness/migmeasure.mjs
 */

import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".map": "application/json" };

const server = http.createServer((q, r) => {
  const u = decodeURIComponent(q.url.split("?")[0]);
  const p = path.join(ROOT, u === "/" ? "index.html" : u);
  fs.readFile(p, (e, b) => {
    if (e) { r.writeHead(404); r.end("nf"); return; }
    r.writeHead(200, { "Content-Type": MIME[path.extname(p)] ?? "application/octet-stream" });
    r.end(b);
  });
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
const page = await browser.newPage({ viewport: { width: 352, height: 900 } });

const measure = async (query) => {
  await page.goto(`http://127.0.0.1:${port}/index.html${query}`, { waitUntil: "networkidle" });
  await page.waitForTimeout(500);
  return page.evaluate(() => {
    const round = (n) => Math.round(n * 100) / 100;
    const out = { block: null, track: null, bar: null, label: null, value: null, meta: null, card: null };

    const q = (s) => document.querySelector(s);
    const block = q(".migration-progress");
    if (block) {
      out.block = { present: true, stage: block.dataset.stage ?? null };
      out.label = q(".migration-progress-label")?.textContent ?? null;
      out.value = q(".migration-progress-value")?.textContent ?? null;
      out.meta = q(".migration-progress-meta")?.textContent ?? null;

      const track = q(".migration-progress-track");
      if (track) {
        const cs = getComputedStyle(track);
        const box = track.getBoundingClientRect();
        out.track = {
          height: round(box.height),
          width: round(box.width),
          background: cs.backgroundColor,
          radius: cs.borderRadius,
        };
      }
      const bar = q(".migration-progress-bar");
      if (bar) {
        const cs = getComputedStyle(bar);
        const box = bar.getBoundingClientRect();
        out.bar = {
          width: round(box.width),
          inlineWidth: bar.style.width || "(none)",
          background: cs.backgroundColor,
          className: bar.className,
          animationName: cs.animationName,
        };
      }
    } else {
      out.block = { present: false };
    }

    const card = q(".migration-result");
    if (card) {
      const cs = getComputedStyle(card);
      out.card = {
        className: card.className,
        borderColor: cs.borderTopColor,
        borderWidth: cs.borderTopWidth,
        radius: cs.borderRadius,
        textPreview: (card.textContent ?? "").slice(0, 90),
      };
    }
    return out;
  });
};

const results = {};
results["determinate (?mig=determinate, 128/512)"] = await measure("?mig=determinate");
results["indeterminate (?mig=indeterminate)"] = await measure("?mig=indeterminate");

// 主题遍历：进度条只用语义令牌，六套主题下都应可见（不全为透明）。
const THEMES = ["mica", "paper", "acrylic", "sakura", "retro", "sticky-note"];
const themeRows = [];
for (const theme of THEMES) {
  const r = await measure(`?theme=${theme}&colorMode=light&mig=determinate`);
  themeRows.push({
    theme,
    trackBg: r.track?.background ?? "-",
    barBg: r.bar?.background ?? "-",
    barWidth: r.bar?.width ?? "-",
  });
}

// browser 在追加段落之后才关闭
// 清理放到文件末尾（追加的结果卡片量测还需要 browser 与 server）

console.log(JSON.stringify({ results, themeRows }, null, 2));

/**
 * 结果卡片量测：点一次「从此目录迁移」，等到结果卡片出现，读它**真实的边框色**。
 *
 * 这是"deferred 不用错误样式"的可视化证据：类名断言证明不了颜色，
 * 只有 computed RGB 能证明用户真的看到的是绿色而不是红色。
 */
const cardProbe = async (result) => {
  await page.goto(`http://127.0.0.1:${port}/index.html?result=${result}&ask=yes`, { waitUntil: "networkidle" });
  await page.waitForTimeout(400);
  const clicked = await page.evaluate(() => {
    const btn = Array.from(document.querySelectorAll("button")).find((b) =>
      (b.textContent ?? "").includes("从此目录迁移")
    );
    if (!btn) return false;
    btn.click();
    return true;
  });
  if (!clicked) return { error: "找不到迁移按钮" };
  await page.waitForTimeout(700);
  return page.evaluate(() => {
    const card = document.querySelector(".migration-result");
    if (!card) return { error: "结果卡片未出现" };
    const cs = getComputedStyle(card);
    return {
      className: card.className,
      borderColor: cs.borderTopColor,
      borderWidth: cs.borderTopWidth,
      radius: cs.borderRadius,
      title: card.querySelector(".migration-result-title")?.textContent ?? null,
      hasRestartBtn: Array.from(card.querySelectorAll("button")).some((b) =>
        (b.textContent ?? "").includes("重启")
      ),
      text: (card.textContent ?? "").replace(/\s+/g, " ").slice(0, 160),
    };
  });
};

const cards = {};
for (const kind of ["deferred", "done", "failed"]) cards[kind] = await cardProbe(kind);
console.log("\n=== RESULT CARDS ===");
console.log(JSON.stringify(cards, null, 2));

await browser.close();
server.close();
