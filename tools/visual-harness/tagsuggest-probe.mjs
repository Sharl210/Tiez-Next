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

// ===========================================================================
// 删除叉的可点区域 + 滚轮不穿透
// ===========================================================================
//
// 这两条是用户直接反馈的交互问题：
//   ① 「剪切板条目里面取消标签的那个叉叉大一点，现在很难点击」
//   ② 「联想列表鼠标放在列表里面滚动时不要影响外面的剪切板条目一起滚动」
//
// 它们都**必须量**，因为都不可见地坏：① 视觉上叉叉看起来"有"，但可点区只有 8px；
// ② 滚动在浮层没超出时看不出问题，只有滚到底之后继续滚才会暴露。

console.log("\n[删除叉的可点区域] 期望 >= 18x18，且不把标签撑大");
{
  await page.goto(`http://127.0.0.1:${port}/src/collapse.html?case=with_tags&tagopen=1&theme=retro&colorMode=light`, { waitUntil: "load" });
  await page.waitForTimeout(700);
  const rm = await page.evaluate(() => {
    const b = document.querySelector(".tag-chip-remove");
    if (!b) return null;
    const r = b.getBoundingClientRect();
    const svg = b.querySelector("svg");
    const sr = svg ? svg.getBoundingClientRect() : null;
    const chip = b.closest(".tag-chip");
    const cr = chip.getBoundingClientRect();
    return {
      btnW: +r.width.toFixed(1), btnH: +r.height.toFixed(1),
      iconW: sr ? +sr.width.toFixed(1) : null,
      chipH: +cr.height.toFixed(1),
    };
  });
  check("删除叉存在", !!rm);
  if (rm) {
    check("可点区域 >= 18x18", rm.btnW >= 18 && rm.btnH >= 18, `实测 ${rm.btnW}x${rm.btnH}`);
    check("图标 >= 12px", rm.iconW >= 12, `实测 ${rm.iconW}px`);
    // 加宽点击区若把芯片也撑大，一行就放不下几个标签 —— 那是另一个问题。
    check("标签芯片未被撑大（高 <= 22）", rm.chipH <= 22, `实测 ${rm.chipH}px`);
  }
}

console.log("\n[滚轮不穿透] 期望：浮层内滚动到底后，外层条目列表纹丝不动");
{
  // `tagfirst=1`：只让**第一个**用例进编辑态，但整页仍渲染 ——
  // 必须有可滚的外层，否则"是否被带动"这条没有判别力。
  await page.setViewportSize({ width: 352, height: 300 });
  await page.goto(`http://127.0.0.1:${port}/src/collapse.html?tagopen=1&tagfirst=1&tagquery=i&theme=retro&colorMode=light`, { waitUntil: "load" });
  await page.waitForTimeout(900);
  await page.evaluate(() => {
    const p = document.querySelector(".tag-edit-suggestions-popover");
    if (p) p.scrollIntoView({ block: "center" });
  });
  await page.waitForTimeout(300);

  const pre = await page.evaluate(() => {
    const l = document.querySelector(".history-list");
    const p = document.querySelector(".tag-edit-suggestions-popover");
    if (!p) return null;
    return { outer: l.scrollTop, max: p.scrollHeight - p.clientHeight,
             y: Math.round(p.getBoundingClientRect().y),
             overscroll: getComputedStyle(p).overscrollBehaviorY };
  });
  check("浮层存在且已滚入视口", !!pre && pre.y >= 0, pre ? `y=${pre.y}` : "无浮层");
  check("overscroll-behavior-y = contain", !!pre && pre.overscroll === "contain", pre ? `实测 ${pre.overscroll}` : "");

  if (pre) {
    const box = await page.locator(".tag-edit-suggestions-popover").first().boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    // 滚到底**之后继续滚** —— 这才是考验"链断没断"的时刻
    for (let i = 0; i < 30; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const post = await page.evaluate(() => {
      const l = document.querySelector(".history-list");
      const p = document.querySelector(".tag-edit-suggestions-popover");
      return { outer: l.scrollTop, inner: p.scrollTop };
    });
    check("浮层内部滚到底", post.inner >= pre.max - 1, `inner ${post.inner}/${pre.max}`);
    check("外层条目列表未被动", post.outer === pre.outer, `outer ${pre.outer} → ${post.outer}`);
  }
  await page.setViewportSize({ width: 380, height: 900 });
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
