/**
 * vscroll-fix-validate.mjs — 对**推荐修复**做双向验证，确保它既治好穿透、
 * 又不破坏"高亮行始终可见"（鼠标 hover 与键盘方向键两条路径都要覆盖）。
 *
 * 推荐修复＝把 `ClipboardItem.tsx:895` 的
 *   `row.scrollIntoView({ block: "nearest" })`
 * 换成**只在浮层容器内**滚动（手算 `pop.scrollTop`），不碰任何祖先。
 *
 * 本脚本在台页里以 `Element.prototype.scrollIntoView` 拦截的方式**等价实现**这条修复
 * （产品源码在别人手上，这里不改），然后量：
 *   - 已复现的穿透格是否归零；
 *   - 鼠标 hover 到被裁区的行时，高亮行是否仍被露出来（在浮层内可见）；
 *   - 键盘 ArrowDown 逐项下移时，高亮是否跟着滚动、外层是否不动。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

/** 等价实现推荐修复：只在浮层容器内滚动。 */
const APPLY_FIX = () => {
  const pop = document.querySelector(".tag-edit-suggestions-popover");
  const orig = Element.prototype.scrollIntoView;
  window.__fix = { containerScoped: 0, passthrough: 0 };
  Element.prototype.scrollIntoView = function (opts) {
    if (this.parentElement === pop) {
      window.__fix.containerScoped++;
      const top = this.offsetTop;
      const bottom = top + this.offsetHeight;
      if (top < pop.scrollTop) pop.scrollTop = top;
      else if (bottom > pop.scrollTop + pop.clientHeight) pop.scrollTop = bottom - pop.clientHeight;
      return;
    }
    window.__fix.passthrough++;
    return orig.apply(this, [opts]);
  };
};

const snap = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    if (!pop) return { alive: false, outer: sc.scrollTop };
    const sr = sc.getBoundingClientRect(), pr = pop.getBoundingClientRect();
    const rows = [...pop.children].map((el) => {
      const r = el.getBoundingClientRect();
      return { t: Math.round(r.top), b: Math.round(r.bottom), active: el.className.includes("--active") };
    });
    const ai = rows.findIndex((r) => r.active);
    const pr2 = pop.getBoundingClientRect();
    return {
      alive: true,
      outer: sc.scrollTop,
      inner: pop.scrollTop,
      innerMax: pop.scrollHeight - pop.clientHeight,
      clipBottom: Math.round(Math.max(0, pr.bottom - sr.bottom)),
      scBottom: Math.round(sr.bottom),
      popBox: [Math.round(pr2.top), Math.round(pr2.bottom)],
      cx: Math.round(pr.left + pr.width / 2),
      rows: rows.length,
      activeIdx: ai,
      // 高亮行是否落在浮层自己的可视区内
      activeVisibleInPopover:
        ai >= 0 && rows[ai].t >= pr2.top - 1 && rows[ai].b <= pr2.bottom + 1,
      log: window.__fix,
    };
  });

async function open(top, pool) {
  await page.goto(`${BASE}/src/vscroll.html?n=20&pool=${pool}&fi=15`, { waitUntil: "load" });
  await page.waitForTimeout(500);
  await page.evaluate((t) => { document.querySelector('[data-virtuoso-scroller="true"]').scrollTop = t; }, top);
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 20000 });
  await page.waitForTimeout(700);
}

console.log("═════════ 修复前后对照：已复现的穿透格（浮层被裁 + 候补溢出）═════════\n");
for (const pool of [40, 3, 2]) {
  for (const top of [700, 748, 780]) {
    // 修复前
    await open(top, pool);
    let s = await snap();
    if (!s.alive) { console.log(`  pool=${pool} top=${top}: 无浮层`); continue; }
    const cy = Math.round((Math.max(s.popBox[0], 8) + Math.min(s.popBox[1], s.scBottom)) / 2);
    const hitOk = await page.evaluate((a) => {
      const pop = document.querySelector(".tag-edit-suggestions-popover");
      const el = document.elementFromPoint(a[0], a[1]);
      return !!(el && (el === pop || pop.contains(el)));
    }, [s.cx, cy]);
    const o0 = s.outer;
    await page.mouse.move(s.cx, cy);
    await page.waitForTimeout(200);
    for (let i = 0; i < 12; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const o1 = (await snap()).outer;

    // 修复后
    await open(top, pool);
    await page.evaluate(APPLY_FIX);
    let f = await snap();
    const cy2 = Math.round((Math.max(f.popBox[0], 8) + Math.min(f.popBox[1], f.scBottom)) / 2);
    const fo0 = f.outer;
    await page.mouse.move(f.cx, cy2);
    await page.waitForTimeout(200);
    for (let i = 0; i < 12; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const fa = await snap();

    console.log(
      `  pool=${String(pool).padStart(2)} top=${String(top).padStart(3)} 内max=${String(s.innerMax).padStart(3)} 裁下=${String(s.clipBottom).padStart(2)}px 打中浮层=${hitOk ? "是" : "否"} | ` +
        `修复前 外 ${o0}→${o1} Δ${o1 - o0} ${o1 - o0 ? "★穿透" : "OK"} ‖ 修复后 外 ${fo0}→${fa.outer} Δ${fa.outer - fo0} ${fa.outer - fo0 ? "★仍穿透" : "✅未动"} ` +
        `| 修复后浮层内 ${fa.inner}（改滚容器 ${fa.log.containerScoped} 次）`
    );
  }
}

console.log("\n═════════ 键盘导航：修复后 ArrowDown 逐项下移，高亮是否仍被露出、外层是否不动 ═════════\n");
for (const top of [700, 760]) {
  await open(top, 40);
  await page.evaluate(APPLY_FIX);
  const inp = await page.$("input.tag-input");
  await inp.focus();
  await page.waitForTimeout(150);
  const s0 = await snap();
  let outerMoved = false;
  const trace = [];
  for (let i = 0; i < 26; i++) {
    await page.keyboard.press("ArrowDown");
    await page.waitForTimeout(45);
    const s = await snap();
    if (s.outer !== s0.outer) outerMoved = true;
    if (i % 6 === 0 || i === 25) {
      trace.push(`#${s.activeIdx}${s.activeVisibleInPopover ? "✓可见" : "✗不可见"} 内${s.inner}/外${s.outer}`);
    }
  }
  const sf = await snap();
  console.log(`  top=${top}: 起点 高亮#${s0.activeIdx} 裁下=${s0.clipBottom}px 内max=${s0.innerMax}`);
  console.log(`    ArrowDown×26: ${trace.join(" | ")}`);
  console.log(`    末态 高亮#${sf.activeIdx} 外层 ${s0.outer}→${sf.outer} Δ${sf.outer - s0.outer} ${outerMoved ? "★外层被动过" : "✅外层全程未动"} | 改滚容器 ${sf.log.containerScoped} 次`);
}

console.log("\n═════════ 未修复时的键盘导航（对照：外层是否会动）═════════\n");
for (const top of [700, 760]) {
  await open(top, 40);
  const inp = await page.$("input.tag-input");
  await inp.focus();
  await page.waitForTimeout(150);
  const s0 = await snap();
  for (let i = 0; i < 26; i++) { await page.keyboard.press("ArrowDown"); await page.waitForTimeout(45); }
  const sf = await snap();
  console.log(`  top=${top}: 外层 ${s0.outer}→${sf.outer} Δ${sf.outer - s0.outer} ${sf.outer !== s0.outer ? "★外层被带动" : "未动"} | 高亮#${sf.activeIdx}`);
}

await browser.close();
