/**
 * vscroll-hover2.mjs — 决定性实验：**鼠标在候补列表里移动**（不是滚轮）会不会带动外层。
 *
 * 线索来自产品代码本身：`ClipboardItem.tsx:893-896`
 *
 *   useLayoutEffect(() => {
 *     if (tagSuggestIndex < 0 || !tagSuggestListRef.current) return;
 *     const row = tagSuggestListRef.current.children[tagSuggestIndex];
 *     row?.scrollIntoView({ block: "nearest" });
 *   }, [tagSuggestIndex, pickableTagSuggestions]);
 *
 * `tagSuggestIndex` 由候补项的 `onMouseEnter` 改变（ClipboardItem.tsx:1678）。
 * 也就是说：**鼠标在候补列表里划过一项，就调用一次 scrollIntoView**。
 * `scrollIntoView` 会滚动**所有**可滚动祖先（含 Virtuoso 生成的 scroller），
 * 而它属于**程序化滚动**，与 scroll chaining 无关 ⇒ `overscroll-behavior` 管不到。
 *
 * 本脚本把「浮层被 scroller 裁掉下半截」这个真机常态做出来（条目滚到列表靠下的位置时
 * 浮层必然被裁），然后在浮层内移动鼠标，量外层的 scrollTop。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const snap = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const sr = sc.getBoundingClientRect();
    const pr = pop.getBoundingClientRect();
    const rows = [...document.querySelectorAll(".tag-suggest-item")].map((el) => {
      const r = el.getBoundingClientRect();
      return { y: Math.round(r.y), h: Math.round(r.height), active: el.className.includes("--active") };
    });
    return {
      outer: sc.scrollTop,
      inner: pop.scrollTop,
      innerMax: pop.scrollHeight - pop.clientHeight,
      popRect: { t: Math.round(pr.top), b: Math.round(pr.bottom) },
      scrRect: { t: Math.round(sr.top), b: Math.round(sr.bottom) },
      clippedBottom: pr.bottom > sr.bottom,
      clippedTop: pr.top < sr.top,
      visibleBottom: Math.min(pr.bottom, sr.bottom),
      rows,
      activeIdx: rows.findIndex((r) => r.active),
      cx: pr.left + pr.width / 2,
    };
  });

async function open(pool) {
  await page.goto(`${BASE}/src/vscroll.html?n=14&pool=${pool}&fi=1`, { waitUntil: "load" });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 15000 });
  await page.waitForTimeout(700);
}

console.log("=== 情形 A：条目在列表靠下，浮层下半截被 scroller 裁掉（真机常态） ===");
await open(40);
// 让浮层越过 scroller 下边界：把外层继续往下滚
await page.evaluate(() => {
  const sc = document.querySelector('[data-virtuoso-scroller="true"]');
  sc.scrollTop += 40;
});
await page.waitForTimeout(400);
let a = await snap();
console.log(
  `  状态: 外层=${a.outer} 浮层=[${a.popRect.t},${a.popRect.b}] scroller=[${a.scrRect.t},${a.scrRect.b}] ` +
    `下截被裁=${a.clippedBottom} 浮层内可滚=${a.innerMax > 0} 当前高亮项=#${a.activeIdx}`
);

console.log("\n  [A1] 鼠标在**未被裁掉的那部分**浮层里逐行划过（模拟用户把鼠标放列表里）");
{
  const before = await snap();
  const ys = before.rows.filter((r) => r.y > before.scrRect.t + 2 && r.y < before.visibleBottom - 4).map((r) => r.y + r.h / 2);
  console.log(`     可在浮层内命中的行 y = ${JSON.stringify(ys.map(Math.round))}`);
  for (let round = 0; round < 3; round++) {
    for (const y of ys) {
      await page.mouse.move(before.cx, y);
      await page.waitForTimeout(70);
    }
  }
  const after = await snap();
  console.log(`     外层 ${before.outer} → ${after.outer} (Δ${after.outer - before.outer})  浮层内 ${before.inner} → ${after.inner} (Δ${after.inner - before.inner})`);
  console.log(`     ${after.outer !== before.outer ? "★★★ 外层被带动了 —— 与滚轮无关，是 hover→scrollIntoView" : "外层未动"}`);
}

console.log("\n  [A2] 鼠标静止在浮层里不动，只滚轮（对照：应无穿透）");
{
  await open(40);
  const s0 = await snap();
  const y = s0.scrRect.t + 30;
  await page.mouse.move(s0.cx, y);
  await page.waitForTimeout(150);
  const before = await snap();
  for (let i = 0; i < 10; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
  const after = await snap();
  console.log(`     外层 ${before.outer} → ${after.outer} (Δ${after.outer - before.outer}) 浮层内 ${before.inner} → ${after.inner}`);
}

console.log("\n=== 情形 B：候补只有 3 条（用户说的「只有 2–3 条」，浮层无溢出） ===");
await open(3);
await page.evaluate(() => {
  document.querySelector('[data-virtuoso-scroller="true"]').scrollTop += 40;
});
await page.waitForTimeout(400);
{
  const before = await snap();
  console.log(
    `  状态: 外层=${before.outer} 浮层=[${before.popRect.t},${before.popRect.b}] scroller=[${before.scrRect.t},${before.scrRect.b}] ` +
      `下截被裁=${before.clippedBottom} 浮层内可滚=${before.innerMax > 0}`
  );
  const ys = before.rows.filter((r) => r.y > before.scrRect.t + 2 && r.y < before.visibleBottom - 4).map((r) => r.y + r.h / 2);
  console.log(`     可在浮层内命中的行 y = ${JSON.stringify(ys.map(Math.round))}`);
  for (let round = 0; round < 3; round++) {
    for (const y of ys) {
      await page.mouse.move(before.cx, y);
      await page.waitForTimeout(70);
    }
  }
  const after = await snap();
  console.log(`     [B1] hover 划过: 外层 ${before.outer} → ${after.outer} (Δ${after.outer - before.outer})`);
  console.log(`     ${after.outer !== before.outer ? "★★★ 外层被带动了" : "外层未动"}`);
}

console.log("\n=== 情形 C：直接用真实产品代码的调用（scrollIntoView）逐行触发，看外层是否被带 ===");
await open(40);
await page.evaluate(() => {
  document.querySelector('[data-virtuoso-scroller="true"]').scrollTop += 40;
});
await page.waitForTimeout(400);
{
  const before = await snap();
  const res = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const out = [];
    const rows = [...pop.children];
    for (let i = 0; i < rows.length; i++) {
      const t0 = sc.scrollTop;
      rows[i].scrollIntoView({ block: "nearest" });
      out.push({ i, outerBefore: t0, outerAfter: sc.scrollTop, delta: sc.scrollTop - t0 });
    }
    return out;
  });
  const moved = res.filter((r) => r.delta !== 0);
  console.log(`     逐行 scrollIntoView 共 ${res.length} 行，其中 ${moved.length} 行带动了外层`);
  console.log(`     带动的行: ${JSON.stringify(moved.slice(0, 8))}`);
}

await browser.close();
