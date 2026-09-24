/**
 * vscroll-causal.mjs — 完整因果链插桩 + 候选修复验证。
 *
 * 已由 `vscroll-attrib.mjs` 归因：外层被带动**不是** scroll chaining（滚轮守卫拦住 8 次
 * 边界滚轮后外层仍动 6px），而是 `ClipboardItem.tsx:895` 的
 * `row.scrollIntoView({ block: "nearest" })` —— 程序化滚动，`overscroll-behavior` 管不到。
 *
 * 本脚本把整条链插桩证实：
 *   wheel(浮层内) → 浮层 scrollTop 变 → 光标下换了一行 → onMouseEnter → setTagSuggestIndex
 *   → useLayoutEffect → row.scrollIntoView({block:'nearest'}) → 滚动 virtuoso scroller
 *
 * 然后验证两种修复：
 *   F1. 把 `scrollIntoView` 换成**只滚浮层自己**（container-scoped，不碰祖先）
 *   F2. F1 + 保留现有 `overscroll-behavior: contain`
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const INSTRUMENT = () => {
  window.__log = { wheel: 0, enter: 0, siv: [], sivScrolled: 0 };
  const pop = document.querySelector(".tag-edit-suggestions-popover");
  pop.addEventListener("wheel", () => { window.__log.wheel++; }, { passive: true, capture: true });
  for (const el of pop.children) {
    el.addEventListener("mouseenter", () => { window.__log.enter++; });
  }
  // 监听冒泡到 document 的 scroll，看是谁在滚
  document.addEventListener(
    "scroll",
    (e) => {
      const t = e.target;
      if (!t || !(t instanceof Element)) return;
      if (t.getAttribute && t.getAttribute("data-virtuoso-scroller") === "true") window.__log.sivScrolled++;
    },
    true
  );
  const orig = Element.prototype.scrollIntoView;
  Element.prototype.scrollIntoView = function (...a) {
    const tag = this.className && typeof this.className === "string" ? this.className : this.tagName;
    window.__log.siv.push(String(tag).slice(0, 40));
    return orig.apply(this, a);
  };
};

const state = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    if (!pop) return { popAlive: false, outer: sc.scrollTop };
    const sr = sc.getBoundingClientRect(), pr = pop.getBoundingClientRect();
    const rows = [...pop.children].map((el) => {
      const r = el.getBoundingClientRect();
      return { y: Math.round(r.y), b: Math.round(r.bottom), active: el.className.includes("--active") };
    });
    return {
      outer: sc.scrollTop, inner: pop.scrollTop,
      innerMax: pop.scrollHeight - pop.clientHeight,
      scBottom: Math.round(sr.bottom),
      clipBottom: Math.round(Math.max(0, pr.bottom - sr.bottom)),
      cx: Math.round(pr.left + pr.width / 2),
      activeIdx: rows.findIndex((r) => r.active),
      lastVisibleRow: rows.filter((r) => r.y < sr.bottom - 2).length - 1,
      rows: rows.length,
      log: window.__log,
    };
  });

async function open(top = 700, pool = 40) {
  await page.goto(`${BASE}/src/vscroll.html?n=20&pool=${pool}&fi=15`, { waitUntil: "load" });
  await page.waitForTimeout(500);
  await page.evaluate((t) => { document.querySelector('[data-virtuoso-scroller="true"]').scrollTop = t; }, top);
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 20000 });
  await page.waitForTimeout(700);
}

async function wheelInside(steps = 12) {
  const s = await state();
  const cy = Math.min(s.scBottom - 3, s.visibleY ?? s.scBottom - 3);
  await page.mouse.move(s.cx, cy);
  await page.waitForTimeout(200);
  const b = await state();
  for (let i = 0; i < steps; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(30); }
  const a = await state();
  return { b, a, cy };
}

console.log("════════ ① 因果链插桩（浮层下缘被裁 = 真机几何）════════");
await open(700, 40);
await page.evaluate(INSTRUMENT);
{
  const s0 = await state();
  console.log(`  起点: 外层=${s0.outer} 浮层内max=${s0.innerMax} 裁下=${s0.clipBottom}px 高亮=#${s0.activeIdx} 可见到第${s0.lastVisibleRow}行/共${s0.rows - 1}行`);
  const { b, a, cy } = await wheelInside(12);
  console.log(`  滚轮×12 @y=${cy}`);
  console.log(`    wheel 事件=${a.log.wheel}  mouseenter=${a.log.enter}`);
  console.log(`    scrollIntoView 调用=${a.log.siv.length} 次 → ${JSON.stringify(a.log.siv.slice(0, 6))}`);
  console.log(`    virtuoso scroller 发生滚动=${a.log.sivScrolled} 次`);
  console.log(`    外层 ${b.outer} → ${a.outer}  Δ${a.outer - b.outer}  ${a.outer !== b.outer ? "★★★外层被带动" : ""}`);
  console.log(`    浮层内 ${b.inner} → ${a.inner}（max ${b.innerMax}）；高亮 #${b.activeIdx} → #${a.activeIdx}`);
}

console.log("\n════════ ② 修复 F1：把 scrollIntoView 换成只滚浮层自己 ════════");
await open(700, 40);
{
  // 只滚容器自身（不碰祖先）—— 这正是推荐修复的核心
  await page.evaluate(() => {
    window.__fix = { calls: 0 };
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const origSiv = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = function (opts) {
      if (this.parentElement === pop) {
        window.__fix.calls++;
        const rowTop = this.offsetTop;
        const rowBottom = rowTop + this.offsetHeight;
        if (rowTop < pop.scrollTop) pop.scrollTop = rowTop;
        else if (rowBottom > pop.scrollTop + pop.clientHeight) pop.scrollTop = rowBottom - pop.clientHeight;
        return;
      }
      return origSiv.apply(this, [opts]);
    };
  });
  const s0 = await state();
  const { b, a, cy } = await wheelInside(12);
  const fixCalls = await page.evaluate(() => window.__fix.calls);
  console.log(`  起点: 外层=${s0.outer} 裁下=${s0.clipBottom}px`);
  console.log(`  滚轮×12 @y=${cy} → 外层 ${b.outer} → ${a.outer}  Δ${a.outer - b.outer}  ${a.outer !== b.outer ? "★仍被带动" : "✅外层未动"}`);
  console.log(`  修复版 scrollIntoView 生效次数=${fixCalls}；浮层内 ${b.inner} → ${a.inner}（浮层仍能滚：${a.inner > 0}）`);
}

console.log("\n════════ ③ 修复 F1 在「候补少、浮层无溢出」下的行为 ════════");
for (const pool of [3, 2]) {
  await open(760, pool);
  const s0 = await state();
  const { b, a } = await wheelInside(12);
  console.log(`  pool=${pool}: 浮层内max=${s0.innerMax} 裁下=${s0.clipBottom}px → 外层 ${b.outer}→${a.outer} Δ${a.outer - b.outer} ${a.outer !== b.outer ? "★被带动" : "✅未动"}`);
}

console.log("\n════════ ④ 反向对照：不修 scrollIntoView，只保留 contain（= 现状）════════");
await open(700, 40);
{
  const { b, a } = await wheelInside(20);
  console.log(`  外层 ${b.outer} → ${a.outer}  Δ${a.outer - b.outer}  ${a.outer !== b.outer ? "★★★ 现状：外层确实被带动（复现用户反馈）" : "外层未动"}`);
}

console.log("\n════════ ⑤ 再对照：去掉 contain（模拟 v0.5.8 之前）════════");
await open(700, 40);
await page.addStyleTag({ content: `.tag-edit-suggestions-popover{overscroll-behavior:auto !important}` });
{
  const { b, a } = await wheelInside(20);
  console.log(`  外层 ${b.outer} → ${a.outer}  Δ${a.outer - b.outer}  ← 这就是 v0.5.8 修掉的那部分（scroll chaining）`);
}

await page.screenshot({ path: "/tmp/vscroll-causal.png" });
await browser.close();
