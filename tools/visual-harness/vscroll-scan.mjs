/**
 * vscroll-scan.mjs — 在真机滚动链上做**穷举扫描**：候补数 × 外层滚动位置 × 条目下标，
 * 每格都记录：鼠标落点命中的元素（命中测试）、浮层能否滚、滚轮后的外层位移。
 *
 * 目的：找出「浮层里滚轮却带动外层」的**可复现条件**——如果某一格复现了，
 * 那就说明前两轮修的是别的元素/别的情形，真根因在那一格的特征里。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox"],
});
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const describe = (sel) => sel;

async function probe({ pool, fi, outerTop, compact }) {
  await page.goto(
    `${BASE}/src/vscroll.html?n=14&pool=${pool}&fi=${fi}${compact ? "&compact=1" : ""}`,
    { waitUntil: "load" }
  );
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 15000 });
  await page.waitForTimeout(500);
  await page.evaluate((t) => {
    document.querySelector('[data-virtuoso-scroller="true"]').scrollTop = t;
  }, outerTop);
  await page.waitForTimeout(350);

  const pre = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const pr = pop.getBoundingClientRect();
    const sr = sc.getBoundingClientRect();
    // 浮层可见部分（被 scroller 裁剪后）的中心点
    const visTop = Math.max(pr.top, sr.top);
    const visBottom = Math.min(pr.bottom, sr.bottom);
    const cx = pr.left + pr.width / 2;
    const cy = (visTop + visBottom) / 2;
    const hit = cy > visTop && cy < visBottom ? document.elementFromPoint(cx, cy) : null;
    return {
      outer: sc.scrollTop,
      inner: pop.scrollTop,
      innerMax: pop.scrollHeight - pop.clientHeight,
      innerCanScroll: pop.scrollHeight > pop.clientHeight + 1,
      outerMax: sc.scrollHeight - sc.clientHeight,
      popRect: { t: Math.round(pr.top), b: Math.round(pr.bottom), l: Math.round(pr.left), w: Math.round(pr.width) },
      scrRect: { t: Math.round(sr.top), b: Math.round(sr.bottom) },
      clipped: pr.top < sr.top || pr.bottom > sr.bottom,
      hitsPopover: !!(hit && (hit === pop || pop.contains(hit))),
      hitDesc: hit ? `${hit.tagName.toLowerCase()}.${typeof hit.className === "string" ? hit.className.split(/\s+/).filter(Boolean).join(".") : ""}` : "NONE",
      cx,
      cy,
      visible: visBottom - visTop,
    };
  });

  if (!(pre.cy > 0) || pre.visible <= 2) {
    return { ...pre, skipped: true };
  }

  await page.mouse.move(pre.cx, pre.cy);
  await page.waitForTimeout(150);
  const base = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    return { outer: sc.scrollTop, inner: pop.scrollTop };
  });
  for (let i = 0; i < 12; i++) {
    await page.mouse.wheel(0, 120);
    await page.waitForTimeout(25);
  }
  const post = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    return { outer: sc.scrollTop, inner: pop.scrollTop };
  });

  return {
    ...pre,
    outerDelta: post.outer - base.outer,
    innerDelta: post.inner - base.inner,
    skipped: false,
  };
}

const rows = [];
for (const compact of [false, true]) {
  for (const pool of [2, 3, 40]) {
    for (const fi of [1, 13]) {
      for (const outerTop of [0, 150, 250, 400, 999]) {
        const key = `compact=${compact ? 1 : 0} pool=${pool} fi=${fi} top=${outerTop}`;
        let r;
        try {
          r = await probe({ pool, fi, outerTop, compact });
        } catch (e) {
          console.log(`  ${key}  ERROR ${e.message.split("\n")[0]}`);
          continue;
        }
        if (r.skipped) {
          console.log(`  ${key}  SKIP(浮层不可见)`);
          continue;
        }
        const flag = r.outerDelta !== 0 ? " ★★★ 外层被带动" : "";
        console.log(
          `  ${key} | 命中=${r.hitsPopover ? "浮层" : r.hitDesc} 可滚=${r.innerCanScroll ? "是" : "否"} 被裁=${r.clipped ? "是" : "否"} 可见高=${Math.round(r.visible)} | ` +
            `内 Δ${r.innerDelta} (max ${r.innerMax}) | 外 Δ${r.outerDelta} (max ${r.outerMax})${flag}`
        );
        rows.push({ key, ...r });
      }
    }
  }
}

const bad = rows.filter((r) => r.outerDelta !== 0);
console.log(`\n扫描 ${rows.length} 格，外层被带动的 ${bad.length} 格`);
bad.forEach((b) => console.log("  ★", b.key, JSON.stringify({ hit: b.hitDesc, canScroll: b.innerCanScroll, clipped: b.clipped, outerDelta: b.outerDelta })));

await browser.close();
