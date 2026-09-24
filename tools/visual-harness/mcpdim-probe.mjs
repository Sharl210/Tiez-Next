/**
 * mcpdim-probe.mjs — 定位「MCP 压暗」到底来自哪一层。
 *
 * 用法：node tools/visual-harness/mcpdim-probe.mjs [输出目录]
 *
 * 方法：挂真实 McpSettingsGroup → 逐元素读 computed style →
 *       并在元素内部的几个采样点**取实际渲染像素**。
 * 两者结合才能区分「元素本身背景暗」与「元素内部有 inset 阴影」。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const OUT = process.argv[2] ?? "/tmp/mcpdim";
fs.mkdirSync(OUT, { recursive: true });

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

const THEMES = process.argv[3] ? [process.argv[3]] : ["retro", "mica"];
const results = [];

for (const theme of THEMES) {
  const page = await browser.newPage({ viewport: { width: 900, height: 1400 } });
  await page.goto(`http://127.0.0.1:${port}/src/mcpdim.html?theme=${theme}&colorMode=light`, { waitUntil: "load" });
  await page.waitForTimeout(700);

  // 逐元素读计算样式：只报"可能造成视觉变暗"的属性
  const rows = await page.evaluate(() => {
    const SELS = [".settings-group", ".group-header", ".group-content", ".data-panel", ".item-label", ".settings-subpage-note"];
    const out = [];
    for (const sel of SELS) {
      document.querySelectorAll(sel).forEach((el, i) => {
        const cs = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        out.push({
          sel: `${sel}#${i}`,
          w: Math.round(r.width), h: Math.round(r.height),
          bg: cs.backgroundColor,
          boxShadow: cs.boxShadow,
          opacity: cs.opacity,
          filter: cs.filter,
          backdrop: cs.backdropFilter,
          border: cs.borderTopWidth + " " + cs.borderTopColor,
        });
      });
    }
    return out;
  });

  await page.screenshot({ path: path.join(OUT, `${theme}.png`), fullPage: true });

  // 取色：在 .data-panel 的内部左上角区域采样（那里正是 inset 阴影覆盖处）
  const samples = await page.evaluate(async () => {
    // 用 canvas 从截不到的 DOM 取色不可行，改由外部截图取样；这里只回传坐标
    const res = [];
    document.querySelectorAll(".data-panel").forEach((el, i) => {
      const r = el.getBoundingClientRect();
      res.push({
        i,
        // inset 3px 3px 0 rgba(0,0,0,.05)：暗块在左上角 3px 内
        insetPoint: { x: Math.round(r.left + 2), y: Math.round(r.top + 2) },
        centerPoint: { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) },
      });
    });
    return res;
  });

  results.push({ theme, rows, samples, png: path.join(OUT, `${theme}.png`) });
  await page.close();
}

await browser.close();
server.close();

// 输出
for (const r of results) {
  console.log(`\n═══ ${r.theme} ═══`);
  for (const row of r.rows) {
    const flags = [];
    if (/inset/.test(row.boxShadow)) flags.push("⚠️INSET阴影");
    if (row.bg && /rgba\([^)]*,\s*0?\.\d+\)/.test(row.bg) && !/,\s*0\)/.test(row.bg)) flags.push("半透明背景");
    if (row.opacity !== "1") flags.push(`opacity=${row.opacity}`);
    if (row.filter !== "none") flags.push(`filter=${row.filter}`);
    if (row.backdrop !== "none") flags.push(`backdrop=${row.backdrop}`);
    console.log(`  ${row.sel.padEnd(24)} ${row.w}×${row.h}  bg=${row.bg}`);
    console.log(`    ${" ".repeat(24)} shadow=${row.boxShadow}  ${flags.join(" ") || ""}`);
  }
  console.log(`  截图: ${r.png}`);
  console.log(`  .data-panel 采样点: ${JSON.stringify(r.samples)}`);
}
