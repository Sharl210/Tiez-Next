/**
 * vscroll-probe.mjs — 在**复刻真机滚动容器链**的台页上量滚轮穿透。
 *
 * 用法：node tools/visual-harness/vscroll-probe.mjs
 * 需要先起 dev server：npx vite --config tools/visual-harness/vite.config.mjs --port 5199
 *
 * 量三件事：
 *   A. 真实滚动容器是谁（react-virtuoso 生成的 scroller）＋ 浮层到它的完整祖先链；
 *   B. 候补少（无溢出）与候补多（有溢出）两种情形下，浮层里的滚轮有没有带动外层；
 *   C. `overscroll-behavior: contain` 在两种情形下各自管不管用（改前 / 改后对照）。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;

const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });
console.log("chrome:", browser.version(), " viewport 352x380 (tauri.conf.json 主窗口)");

/** A. 祖先链 + scroller 识别。 */
async function inspect() {
  await page.goto(`${BASE}/src/vscroll.html?n=14&pool=40&fi=1`, { waitUntil: "load" });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 15000 });
  await page.waitForTimeout(600);
  return page.evaluate(() => {
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const chain = [];
    let el = pop;
    while (el && el !== document.documentElement) {
      const cs = getComputedStyle(el);
      chain.push({
        tag: el.tagName.toLowerCase(),
        cls: el.className && typeof el.className === "string" ? el.className : "",
        id: el.id || "",
        position: cs.position,
        overflowY: cs.overflowY,
        overflowX: cs.overflowX,
        overflow: cs.overflow,
        zIndex: cs.zIndex,
        overscrollY: cs.overscrollBehaviorY,
        scrollH: el.scrollHeight,
        clientH: el.clientHeight,
        canScrollY: el.scrollHeight > el.clientHeight,
        transform: cs.transform === "none" ? "none" : "set",
        isScrollable: el.scrollHeight > el.clientHeight,
      });
      el = el.parentElement;
    }
    // virtuoso 自己生成的 scroller
    const scroller = document.querySelector('[data-virtuoso-scroller="true"]');
    const scs = scroller ? getComputedStyle(scroller) : null;
    const itemList = document.querySelector('[data-testid="virtuoso-item-list"]');
    return {
      chain,
      scroller: scroller && {
        tag: scroller.tagName.toLowerCase(),
        cls: scroller.className,
        testid: scroller.getAttribute("data-testid"),
        attr: scroller.getAttribute("data-virtuoso-scroller"),
        inlineStyle: scroller.getAttribute("style"),
        position: scs.position,
        overflowY: scs.overflowY,
        overscrollY: scs.overscrollBehaviorY,
        scrollH: scroller.scrollHeight,
        clientH: scroller.clientHeight,
      },
      itemList: itemList && { testid: itemList.getAttribute("data-testid"), cls: itemList.className },
      virtualWrapper: !!document.querySelector(".virtual-list-wrapper"),
      hasHistoryList: !!document.querySelector(".history-list"),
    };
  });
}

/** 滚轮穿透测试。 */
async function wheel(poolSize, { fixture } = {}) {
  await page.goto(`${BASE}/src/vscroll.html?n=14&pool=${poolSize}&fi=1`, { waitUntil: "load" });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 15000 });
  await page.waitForTimeout(700);
  if (fixture === "wheelguard") {
    // 候选修复：在浮层上装 { passive:false } 的原生 wheel 监听器——自己不能滚就掐掉。
    // 这段**只在本实验台注入**，不写进产品代码。
    await page.evaluate(() => {
      const pop = document.querySelector(".tag-edit-suggestions-popover");
      pop.addEventListener(
        "wheel",
        (e) => {
          const canScroll = pop.scrollHeight > pop.clientHeight + 1;
          if (!canScroll) {
            e.preventDefault();
            e.stopPropagation();
            return;
          }
          const atTop = pop.scrollTop <= 0;
          const atBottom = pop.scrollTop + pop.clientHeight >= pop.scrollHeight - 1;
          if ((e.deltaY < 0 && atTop) || (e.deltaY > 0 && atBottom)) {
            e.preventDefault();
            e.stopPropagation();
          }
        },
        { passive: false, capture: true }
      );
    });
  }

  const pre = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const r = pop.getBoundingClientRect();
    return {
      outer: sc.scrollTop,
      inner: pop.scrollTop,
      innerMax: pop.scrollHeight - pop.clientHeight,
      innerCanScroll: pop.scrollHeight > pop.clientHeight + 1,
      overscrollY: getComputedStyle(pop).overscrollBehaviorY,
      box: { x: r.x, y: r.y, w: r.width, h: r.height },
      outerMax: sc.scrollHeight - sc.clientHeight,
    };
  });
  // 把浮层滚进视口中央，保证鼠标落在浮层上（而不是被视口裁掉）
  await page.evaluate(() => {
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    pop.scrollIntoView({ block: "center" });
  });
  await page.waitForTimeout(300);
  // scrollIntoView 可能已改变外层位置，重新取基线
  const base = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const r = pop.getBoundingClientRect();
    return { outer: sc.scrollTop, inner: pop.scrollTop, box: { x: r.x, y: r.y, w: r.width, h: r.height } };
  });

  await page.mouse.move(base.box.x + base.box.w / 2, base.box.y + base.box.h / 2);
  for (let i = 0; i < 20; i++) {
    await page.mouse.wheel(0, 120);
    await page.waitForTimeout(20);
  }
  const post = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    return { outer: sc.scrollTop, inner: pop.scrollTop };
  });
  const outerDelta = post.outer - base.outer;
  const innerDelta = post.inner - base.inner;
  console.log(
    `  pool=${String(poolSize).padStart(2)} 候补${String(poolSize).padStart(2)}条 | 浮层可滚=${pre.innerCanScroll} (${pre.innerMax > 0 ? "有溢出" : "无溢出"}) ob=${pre.overscrollY} | ` +
      `浮层内 ${base.inner}→${post.inner} (Δ${innerDelta}) | 外层 VIRTUOSO ${base.outer}→${post.outer} (Δ${outerDelta}) ` +
      `${outerDelta !== 0 ? "★外层被带动" : "外层未动"}`
  );
  return { outerDelta, innerDelta, pre, base, post };
}

console.log("\n[A] 真实滚动容器与祖先链");
const info = await inspect();
console.log("  scroller:", JSON.stringify(info.scroller, null, 2));
console.log("  virtuoso item list:", JSON.stringify(info.itemList));
console.log("  .virtual-list-wrapper 存在:", info.virtualWrapper, " .history-list 存在:", info.hasHistoryList);
console.log("  祖先链（浮层 → 上）:");
info.chain.forEach((c, i) => {
  console.log(
    `   ${String(i).padStart(2)}. <${c.tag}> ${c.cls ? "." + c.cls.split(/\s+/).join(".") : ""}${c.id ? "#" + c.id : ""}` +
      ` | position=${c.position} overflow-y=${c.overflowY} z=${c.zIndex} ob=${c.overscrollY}` +
      ` | scrollH=${c.scrollH} clientH=${c.clientH} 可滚=${c.canScrollY}`
  );
});

console.log("\n[B] 滚轮穿透（当前产品代码 = overscroll-behavior: contain）");
await wheel(2);
await wheel(40);

console.log("\n[C] 对照：注入「自己不能滚就 preventDefault+stopPropagation」的 wheel 守卫");
await wheel(2, { fixture: "wheelguard" });
await wheel(40, { fixture: "wheelguard" });

await page.screenshot({ path: "/tmp/vscroll-final.png" });
await browser.close();
