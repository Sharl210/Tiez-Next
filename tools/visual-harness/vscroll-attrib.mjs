/**
 * vscroll-attrib.mjs — **决定性归因**：在被裁条件下复现出的"外层跟着动"，到底是
 * ①scroll chaining（`overscroll-behavior` 该管的）还是
 * ②`scrollIntoView` 程序化滚动（`overscroll-behavior` 管不到的）
 *
 * 三条对照，同一格条件（浮层下缘被 scroller 裁掉 56px）：
 *   A. 原样                      → 量基线
 *   B. no-op `scrollIntoView`    → 若外层不动 ⇒ 归因②成立
 *   C. 只加 wheel 守卫           → 若外层仍动 ⇒ 说明候选方案①（preventDefault）治不了这个根因
 *   D. wheel 守卫 + no-op SIV    → 双保险下的结果
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const g = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const sr = sc.getBoundingClientRect();
    const o = { outer: sc.scrollTop, sc: [Math.round(sr.top), Math.round(sr.bottom)], popAlive: !!pop };
    if (!pop) return o;
    const pr = pop.getBoundingClientRect();
    o.inner = pop.scrollTop;
    o.innerMax = pop.scrollHeight - pop.clientHeight;
    o.pop = [Math.round(pr.top), Math.round(pr.bottom)];
    o.cx = Math.round(pr.left + pr.width / 2);
    o.clipBottom = Math.round(Math.max(0, pr.bottom - sr.bottom));
    o.vis = [Math.round(Math.max(pr.top, sr.top)), Math.round(Math.min(pr.bottom, sr.bottom))];
    return o;
  });

async function run(pool, mode) {
  await page.goto(`${BASE}/src/vscroll.html?n=20&pool=${pool}&fi=15`, { waitUntil: "load" });
  await page.waitForTimeout(500);
  await page.evaluate(() => { document.querySelector('[data-virtuoso-scroller="true"]').scrollTop = 700; });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 20000 });
  await page.waitForTimeout(700);

  if (mode.includes("nosiv")) {
    // B/D：把 scrollIntoView 变成 no-op —— 精确切断"程序化滚动"这条路径
    await page.evaluate(() => {
      window.__sivCalls = 0;
      const orig = Element.prototype.scrollIntoView;
      Element.prototype.scrollIntoView = function (...a) { window.__sivCalls++; return orig.apply(this, a); };
      window.__sivNoop = () => { Element.prototype.scrollIntoView = function () {}; };
    });
  }
  if (mode.includes("guard")) {
    // C/D：候选方案① —— 浮层上的 wheel 守卫（自己不能滚 / 到边界就掐掉）
    await page.evaluate(() => {
      const pop = document.querySelector(".tag-edit-suggestions-popover");
      window.__guardHits = 0;
      pop.addEventListener("wheel", (e) => {
        const canScroll = pop.scrollHeight > pop.clientHeight + 1;
        const atTop = pop.scrollTop <= 0;
        const atBottom = pop.scrollTop + pop.clientHeight >= pop.scrollHeight - 1;
        if (!canScroll || (e.deltaY < 0 && atTop) || (e.deltaY > 0 && atBottom)) {
          window.__guardHits++; e.preventDefault(); e.stopPropagation();
        }
      }, { passive: false, capture: true });
    });
  }

  const s0 = await g();
  const cy = Math.round((s0.vis[0] + s0.vis[1]) / 2);
  await page.mouse.move(s0.cx, cy);
  await page.waitForTimeout(250);
  const s1 = await g();

  if (mode.includes("nosiv")) await page.evaluate(() => window.__sivNoop());

  // 滚轮前先记录，再滚
  const s2 = await g();
  for (let i = 0; i < 12; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
  const s3 = await g();

  const info = await page.evaluate(() => ({ sivCalls: window.__sivCalls ?? null, guardHits: window.__guardHits ?? null }));

  console.log(
    `pool=${String(pool).padStart(2)} mode=${mode.padEnd(16)} 裁下=${s0.clipBottom}px 浮层内max=${s0.innerMax} | ` +
      `移入 外${s0.outer}→${s1.outer}(Δ${s1.outer - s0.outer}) | 滚轮 外${s2.outer}→${s3.outer}(Δ${s3.outer - s2.outer}) 内${s2.inner}→${s3.inner} | ` +
      `scrollIntoView 调用=${info.sivCalls ?? "-"} 守卫拦截=${info.guardHits ?? "-"}`
  );
}

console.log("########## 归因实验（浮层下缘被裁 56px 的真机几何）##########\n");
for (const pool of [40, 3]) {
  await run(pool, "asis");
  await run(pool, "nosiv");
  await run(pool, "guard");
  await run(pool, "guard+nosiv");
  console.log("");
}
await browser.close();
