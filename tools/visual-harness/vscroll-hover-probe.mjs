/**
 * vscroll-hover-probe.mjs — 检验「外层跟着滚」是不是被 `scrollIntoView` 引起的（而不是滚轮链）。
 *
 * 线索：`ClipboardItem.tsx:893-896` 有一个 `useLayoutEffect`，在 `tagSuggestIndex` 或
 * `pickableTagSuggestions` 变化时执行 `row.scrollIntoView({ block: "nearest" })`。
 * `scrollIntoView` 会滚动**所有**可滚动祖先（不只是最近的滚动口），因此只要那个候补行
 * 在 Virtuoso 的可见区之外，外层 scroller 就会被它滚动 —— 这条路径完全绕开
 * `overscroll-behavior`（后者只管滚轮/触摸的链式滚动）。
 *
 * 而 `tagSuggestIndex` 由候补项的 `onMouseEnter` 改变 ⇒ 鼠标停在列表上时，
 * 任何让光标下的项发生变化的动作（含"滚动后浏览器重发 hover"）都会走到这条路径。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const state = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const items = [...document.querySelectorAll(".tag-suggest-item")];
    const pr = pop.getBoundingClientRect();
    const sr = sc.getBoundingClientRect();
    return {
      outer: sc.scrollTop,
      inner: pop.scrollTop,
      outerMax: sc.scrollHeight - sc.clientHeight,
      popRect: { y: Math.round(pr.y), h: Math.round(pr.height), bottom: Math.round(pr.bottom) },
      scrollerRect: { y: Math.round(sr.y), h: Math.round(sr.height), bottom: Math.round(sr.bottom) },
      popBottomBeyondScroller: pr.bottom > sr.bottom,
      itemRects: items.slice(0, 4).map((el) => {
        const r = el.getBoundingClientRect();
        return { y: Math.round(r.y), h: Math.round(r.height), active: el.className.includes("--active") };
      }),
    };
  });

async function open(pool) {
  await page.goto(`${BASE}/src/vscroll.html?n=14&pool=${pool}&fi=1`, { waitUntil: "load" });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 15000 });
  await page.waitForTimeout(700);
}

console.log("\n=== 场景 1：候补少（3 条，浮层无溢出），把浮层拖到外层可见区底部附近 ===");
await open(3);
// 让浮层下半部分越过外层 scroller 的下边界（真实使用中，条目滚到列表下半部就是这个状态）
await page.evaluate(() => {
  const sc = document.querySelector('[data-virtuoso-scroller="true"]');
  const pop = document.querySelector(".tag-edit-suggestions-popover");
  const delta = sc.getBoundingClientRect().bottom - pop.getBoundingClientRect().bottom + 20;
  sc.scrollTop += delta;
});
await page.waitForTimeout(400);
let a = await state();
console.log("  初始:", JSON.stringify({ outer: a.outer, inner: a.inner, outerMax: a.outerMax, pop: a.popRect, scroller: a.scrollerRect, 浮层越过外层下边界: a.popBottomBeyondScroller }));
console.log("  候补项位置:", JSON.stringify(a.itemRects));

console.log("\n  [1a] 光标静止 + 滚轮（不注入 hover）");
{
  const before = await state();
  await page.mouse.move(180, 200); // 先移到别处
  const box = await page.locator(".tag-edit-suggestions-popover").boundingBox();
  await page.mouse.move(box.x + box.width / 2, Math.min(box.y + box.height - 4, 370));
  const b2 = await state();
  for (let i = 0; i < 10; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(20); }
  const after = await state();
  console.log(`     外层 ${b2.outer}→${after.outer} (Δ${after.outer - b2.outer}) | 浮层内 ${b2.inner}→${after.inner}`);
}

console.log("\n  [1b] 光标在候补项之间移动（模拟真实浏览器滚动后重发 hover）");
await open(3);
await page.evaluate(() => {
  const sc = document.querySelector('[data-virtuoso-scroller="true"]');
  const pop = document.querySelector(".tag-edit-suggestions-popover");
  sc.scrollTop += sc.getBoundingClientRect().bottom - pop.getBoundingClientRect().bottom + 20;
});
await page.waitForTimeout(400);
{
  const before = await state();
  const scRect = before.scrollerRect;
  const maxY = Math.min(scRect.bottom - 4, 374);
  for (let k = 0; k < 14; k++) {
    const y = Math.min(before.itemRects[0].y + (k % 3) * 18 + 8, maxY);
    await page.mouse.move(160, y);
    await page.waitForTimeout(60);
  }
  const after = await state();
  console.log(`     外层 ${before.outer}→${after.outer} (Δ${after.outer - before.outer})`);
  console.log(`     浮层内 ${before.inner}→${after.inner}`);
}

console.log("\n  [1c] 直接用 scrollIntoView 复现（不改产品代码，只在台页里调用同一个 API）");
await open(3);
await page.evaluate(() => {
  const sc = document.querySelector('[data-virtuoso-scroller="true"]');
  const pop = document.querySelector(".tag-edit-suggestions-popover");
  sc.scrollTop += sc.getBoundingClientRect().bottom - pop.getBoundingClientRect().bottom + 20;
});
await page.waitForTimeout(400);
{
  const before = await state();
  await page.evaluate(() => {
    const rows = [...document.querySelectorAll(".tag-suggest-item")];
    rows[rows.length - 1].scrollIntoView({ block: "nearest" });
  });
  await page.waitForTimeout(300);
  const after = await state();
  console.log(`     外层 ${before.outer}→${after.outer} (Δ${after.outer - before.outer})  ← scrollIntoView 一个候补行`);
}

console.log("\n=== 场景 2：候补多（40 条，浮层有溢出），浮层同样越过外层下边界 ===");
await open(40);
await page.evaluate(() => {
  const sc = document.querySelector('[data-virtuoso-scroller="true"]');
  const pop = document.querySelector(".tag-edit-suggestions-popover");
  sc.scrollTop += sc.getBoundingClientRect().bottom - pop.getBoundingClientRect().bottom + 20;
});
await page.waitForTimeout(400);
{
  const before = await state();
  console.log("  初始:", JSON.stringify({ outer: before.outer, 浮层越过外层下边界: before.popBottomBeyondScroller }));
  const box = await page.locator(".tag-edit-suggestions-popover").boundingBox();
  await page.mouse.move(box.x + box.width / 2, Math.min(box.y + box.height - 4, 374));
  for (let i = 0; i < 10; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(20); }
  const after = await state();
  console.log(`  [2a] 滚轮: 外层 ${before.outer}→${after.outer} (Δ${after.outer - before.outer}) | 浮层内 ${before.inner}→${after.inner}`);
}

await browser.close();
