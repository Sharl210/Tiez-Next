/**
 * vscroll-bottom.mjs — 复现真机最常见的几何：**被编辑的条目位于列表可见区的下半部**，
 * 候补浮层（`top: calc(100% + 5px)`，高 80px）因此越过 virtuoso scroller 的下边界被裁。
 *
 * 这是本仓库前面几轮验证**都没做过**的条件：
 *   - `tagsuggest-probe.mjs` 与 `vscroll-*.mjs` 前面的用例，浮层都是**完整可见**的；
 *   - 真机上条目高得多（含预览、元信息、标签行），一屏 3~4 条，用户滚到要打标签的那条时，
 *     那条多半就在下半屏，浮层必然被裁。
 *
 * 被裁之后，产品代码 `ClipboardItem.tsx:893-896` 的
 *   `row.scrollIntoView({ block: "nearest" })`
 * 会为了把高亮行露出来而**滚动 scroller**（程序化滚动，`overscroll-behavior` 管不到）。
 *
 * 正确重放顺序：先导航 → 先滚外层（让目标条目被 Virtuoso 挂载）→ 再等浮层出现。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox", "--force-color-profile=srgb"] });
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const g = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    const sr = sc.getBoundingClientRect();
    const o = { outer: sc.scrollTop, outerMax: sc.scrollHeight - sc.clientHeight, sc: [Math.round(sr.top), Math.round(sr.bottom)], popAlive: !!pop };
    if (!pop) return o;
    const pr = pop.getBoundingClientRect();
    o.inner = pop.scrollTop;
    o.innerMax = pop.scrollHeight - pop.clientHeight;
    o.pop = [Math.round(pr.top), Math.round(pr.bottom)];
    o.cx = Math.round(pr.left + pr.width / 2);
    o.clipTop = Math.round(Math.max(0, sr.top - pr.top));
    o.clipBottom = Math.round(Math.max(0, pr.bottom - sr.bottom));
    o.vis = [Math.round(Math.max(pr.top, sr.top)), Math.round(Math.min(pr.bottom, sr.bottom))];
    o.rows = [...document.querySelectorAll(".tag-suggest-item")].map((el) => {
      const r = el.getBoundingClientRect();
      return { y: Math.round(r.y), b: Math.round(r.bottom), active: el.className.includes("--active") };
    });
    o.activeIdx = o.rows.findIndex((r) => r.active);
    return o;
  });

const hit = (x, y) =>
  page.evaluate((a) => {
    const el = document.elementFromPoint(a[0], a[1]);
    return el ? `${el.tagName.toLowerCase()}.${typeof el.className === "string" ? el.className.split(/\s+/).filter(Boolean).join(".") : ""}` : "NONE";
  }, [x, y]);

async function setup(n, pool, fi, outerTop) {
  await page.goto(`${BASE}/src/vscroll.html?n=${n}&pool=${pool}&fi=${fi}`, { waitUntil: "load" });
  await page.waitForTimeout(500);
  // 先滚外层 → Virtuoso 才会挂载 fi 对应的条目 → 浮层才出现
  await page.evaluate((t) => { document.querySelector('[data-virtuoso-scroller="true"]').scrollTop = t; }, outerTop);
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 20000 });
  await page.waitForTimeout(700);
}

console.log("############ 被编辑条目在列表下半部 → 浮层下缘被裁 ############");
const CASES = [
  { n: 20, pool: 3, fi: 15, top: 700 },
  { n: 20, pool: 40, fi: 15, top: 700 },
  { n: 20, pool: 3, fi: 15, top: 780 },
  { n: 20, pool: 40, fi: 15, top: 780 },
  { n: 20, pool: 3, fi: 18, top: 999 },
  { n: 20, pool: 40, fi: 18, top: 999 },
];

for (const c of CASES) {
  try {
    await setup(c.n, c.pool, c.fi, c.top);
  } catch (e) {
    console.log(`\n--- n=${c.n} pool=${c.pool} fi=${c.fi} top=${c.top}: 浮层未出现（${e.message.split("\n")[0]}）`);
    continue;
  }
  const s = await g();
  console.log(`\n--- n=${c.n} pool=${c.pool} fi=${c.fi} top=${c.top} ---`);
  console.log(`  外层=${s.outer}/${s.outerMax} scroller=${JSON.stringify(s.sc)} 浮层=${JSON.stringify(s.pop)} 上裁=${s.clipTop}px 下裁=${s.clipBottom}px 可见=${JSON.stringify(s.vis)}`);
  console.log(`  浮层内max=${s.innerMax} 高亮=#${s.activeIdx} 行y=${JSON.stringify(s.rows.map((r) => r.y))}`);
  if (s.clipBottom === 0 && s.clipTop === 0) { console.log("  浮层完整可见 —— 该用例不构成「被裁」条件"); }

  // ① 鼠标移到可见切片中部并等待（用户"把鼠标放进列表"）
  {
    const cy = Math.round((s.vis[0] + s.vis[1]) / 2);
    if (s.vis[1] - s.vis[0] > 3) {
      const h = await hit(s.cx, cy);
      await page.mouse.move(s.cx, cy);
      await page.waitForTimeout(300);
      const s1 = await g();
      console.log(`  ① 移入(${s.cx},${cy}) 命中=${h}: 外层 ${s.outer}→${s1.outer} Δ${s1.outer - s.outer}${s1.outer !== s.outer ? "  ★★鼠标一进去外层就动了" : ""}`);
    }
  }

  // ② 把鼠标移到"浮层可见下边缘附近"（真机最容易停在的位置）
  {
    const y = Math.max(s.sc[0] + 4, Math.min(s.vis[1] - 3, s.sc[1] - 3));
    const s0 = await g();
    const h = await hit(s0.cx, Math.round(y));
    await page.mouse.move(s0.cx, Math.round(y));
    await page.waitForTimeout(300);
    const s1 = await g();
    console.log(`  ② 移到下边缘 y=${Math.round(y)} 命中=${h}: 外层 ${s0.outer}→${s1.outer} Δ${s1.outer - s0.outer}${s1.outer !== s0.outer ? "  ★★外层被带动" : ""}`);
    // 滚轮
    const s2 = await g();
    for (let i = 0; i < 12; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const s3 = await g();
    console.log(`  ③ 下边缘滚轮×12: 外层 ${s2.outer}→${s3.outer} Δ${s3.outer - s2.outer} | 浮层内 ${s2.inner}→${s3.inner}(max ${s3.innerMax})${s3.outer !== s2.outer ? "  ★★★外层被带动" : ""}`);
  }

  // ④ hover 划过所有「在可见切片内」的行（含贴着下边缘的那行）
  {
    const s0 = await g();
    const ys = [];
    for (const r of s0.rows) {
      const y = r.y + 9;
      if (y > s0.sc[0] + 2 && y < s0.sc[1] - 2) ys.push(Math.round(y));
    }
    for (let round = 0; round < 2; round++) for (const y of ys) { await page.mouse.move(s0.cx, y); await page.waitForTimeout(70); }
    const s1 = await g();
    console.log(`  ④ hover 划过 ${ys.length} 行(y=${JSON.stringify(ys)}): 外层 ${s0.outer}→${s1.outer} Δ${s1.outer - s0.outer}${s1.outer !== s0.outer ? "  ★★外层被带动" : ""}`);
  }

  // ⑤ 直接调用产品代码那个 API：逐行 scrollIntoView
  {
    const s0 = await g();
    const res = await page.evaluate(() => {
      const sc = document.querySelector('[data-virtuoso-scroller="true"]');
      const pop = document.querySelector(".tag-edit-suggestions-popover");
      const out = [];
      const kids = [...pop.children];
      kids.forEach((row, i) => {
        const t0 = sc.scrollTop;
        row.scrollIntoView({ block: "nearest" });
        if (sc.scrollTop !== t0) out.push({ i, d: sc.scrollTop - t0 });
      });
      return out;
    });
    const s1 = await g();
    console.log(`  ⑤ 逐行 scrollIntoView: 外层 ${s0.outer}→${s1.outer} Δ${s1.outer - s0.outer} 带动外层的行=${JSON.stringify(res.slice(0, 6))}${s1.outer !== s0.outer ? "  ★★★产品代码自己滚了外层" : ""}`);
  }
}

await browser.close();
