/**
 * tagsuggest-probe.mjs — 标签候补浮层的量测。
 *
 * 用法：node tools/visual-harness/tagsuggest-probe.mjs
 *
 * 用户的两条要求：
 *   1. 「不要填写标签还有这种快捷添加的遮挡栏」「不要这个上面弹出来的…区域」
 *      → 空输入时**不应存在**浮层
 *   2. 「我输了内容才进行补全列表的展示」「列表最多展示 4 行，超过可以用鼠标滚动查看」
 *      → 有输入时浮层存在、高度 ≤ 4 行、且可滚动
 *
 * 两条都必须量：只量第 2 条会让"空输入也弹出"的缺陷漏掉（它只是变矮了而已）。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" };
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

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});
const page = await browser.newPage({ viewport: { width: 380, height: 900 } });

const probe = async (query) => {
  await page.goto(`http://127.0.0.1:${port}/src/collapse.html?case=text&theme=retro&colorMode=light${query}`, { waitUntil: "load" });
  await page.waitForSelector("[data-test-clipboard-item]", { timeout: 15000 });
  await page.waitForTimeout(400);
  return page.evaluate(() => {
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    if (!pop) return { exists: false };
    const items = pop.querySelectorAll(".tag-suggest-item");
    const cs = getComputedStyle(pop);
    const firstItem = items[0];
    return {
      exists: true,
      itemCount: items.length,
      height: Math.round(pop.getBoundingClientRect().height * 100) / 100,
      rowHeight: firstItem ? Math.round(firstItem.getBoundingClientRect().height * 100) / 100 : null,
      overflowY: cs.overflowY,
      scrollHeight: pop.scrollHeight,
      clientHeight: pop.clientHeight,
      canScroll: pop.scrollHeight > pop.clientHeight + 1,
    };
  });
};

const findings = [];
const check = (label, ok, detail) => {
  findings.push({ label, ok, detail });
  console.log(`  ${ok ? "✓" : "✗"} ${label}${detail ? `  → ${detail}` : ""}`);
};

console.log("\n[空输入] 期望浮层不存在（用户说的遮挡栏）");
const empty = await probe("&tagopen=1&tagquery=");
check("空输入时浮层不存在", empty.exists === false, empty.exists ? `仍然存在，高 ${empty.height}px、${empty.itemCount} 项` : "");

console.log("\n[有输入] 期望最多 4 行、可滚动");
const typed = await probe("&tagopen=1&tagquery=i");
check("有输入时浮层存在", typed.exists === true);
if (typed.exists) {
  const maxH = typed.rowHeight ? typed.rowHeight * 4 + 6 + 2 : 0;
  check(
    `高度 ≤ 4 行（上限 ${Math.round(maxH * 100) / 100}px）`,
    typed.height <= maxH + 2,
    `实际 ${typed.height}px（行高 ${typed.rowHeight}px、${typed.itemCount} 项）`
  );
  check("内容多于可见区时可滚动", typed.canScroll === true, `scrollHeight=${typed.scrollHeight} > clientHeight=${typed.clientHeight}`);
  check("overflow-y 允许滚动", typed.overflowY === "auto" || typed.overflowY === "scroll", `overflowY=${typed.overflowY}`);
}

const failed = findings.filter((f) => !f.ok);
console.log(`\n断言 ${findings.length - failed.length} / ${findings.length} 通过`);
if (failed.length) {
  console.log("未通过：");
  failed.forEach((f) => console.log(`  ✗ ${f.label}  ${f.detail}`));
}

await page.screenshot({ path: "/tmp/tagsuggest-typed.png", fullPage: false });
await browser.close();
server.close();
process.exit(failed.length ? 1 : 0);
