/**
 * vscroll-clip.mjs — 复现真机**必然发生**的那个条件：浮层被 virtioso scroller 裁剪。
 *
 * 真机几何（tauri 主窗口 352×380）：
 *   - 条目行高约 80–110px，一屏只放得下 3–4 条；
 *   - 用户要"给某条加标签"，往往会滚到列表中段/末段；
 *   - 此时 `.tag-edit-suggestions-popover`（`top: calc(100% + 5px)`，高 80px）
 *     会越过 `.virtuoso-scroller` 的下边界 —— scroller 是 `overflow-y: auto`，
 *     于是浮层的下半截**被裁掉**。
 *
 * 被裁掉之后有两件事和台页里"浮层完整可见"时不同：
 *   1. 鼠标落在"看着像列表、其实已经被裁掉"的那块区域上时，命中的是**别的元素**
 *      （外层条目 / 容器），滚轮自然由外层处理 —— 这不是"穿透"，而是"没打中"；
 *   2. 浮层内的高亮行若落在 scroller 可视区之外，产品代码的
 *      `row.scrollIntoView({block:'nearest'})`（ClipboardItem.tsx:895）
 *      必须滚动 scroller 才能把它显出来 —— 这会**主动滚动外层**。
 *      `overscroll-behavior` 管不到程序化滚动。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
const BASE = process.env.HARNESS_URL ?? "http://127.0.0.1:5199";

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
const page = await browser.newPage({ viewport: { width: 352, height: 380 } });

const geom = () =>
  page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    if (!pop) return { popAlive: false };
    const pr = pop.getBoundingClientRect();
    const sr = sc.getBoundingClientRect();
    const rows = [...document.querySelectorAll(".tag-suggest-item")].map((el) => {
      const r = el.getBoundingClientRect();
      return { y: Math.round(r.y), h: Math.round(r.height), active: el.className.includes("--active") };
    });
    return {
      popAlive: true,
      outer: sc.scrollTop, outerMax: sc.scrollHeight - sc.clientHeight,
      inner: pop.scrollTop, innerMax: pop.scrollHeight - pop.clientHeight,
      sc: { t: Math.round(sr.top), b: Math.round(sr.bottom) },
      pop: { t: Math.round(pr.top), b: Math.round(pr.bottom), l: Math.round(pr.left), w: Math.round(pr.width) },
      popClippedBelow: pr.bottom > sr.bottom,
      clipBelowPx: Math.round(Math.max(0, pr.bottom - sr.bottom)),
      visTop: Math.round(Math.max(pr.top, sr.top)),
      visBottom: Math.round(Math.min(pr.bottom, sr.bottom)),
      cx: Math.round(pr.left + pr.width / 2),
      rows,
      activeIdx: rows.findIndex((r) => r.active),
    };
  });

async function openAndScrollEditedItemToBottom(pool, fi) {
  await page.goto(`${BASE}/src/vscroll.html?n=14&pool=${pool}&fi=${fi}`, { waitUntil: "load" });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 15000 });
  await page.waitForTimeout(700);
  // 把外层滚到"被编辑条目的浮层下缘刚好越过 scroller 下边界 40px"的位置
  const need = await page.evaluate(() => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    const pop = document.querySelector(".tag-edit-suggestions-popover");
    return { delta: pop.getBoundingClientRect().bottom - sc.getBoundingClientRect().bottom + 40, outerMax: sc.scrollHeight - sc.clientHeight };
  });
  await page.evaluate((d) => {
    const sc = document.querySelector('[data-virtuoso-scroller="true"]');
    sc.scrollTop = Math.min(sc.scrollTop + d, sc.scrollHeight - sc.clientHeight);
  }, need.delta);
  await page.waitForTimeout(500);
  return need;
}

console.log("########## 条件：浮层下缘被 scroller 裁剪 ##########\n");

for (const [pool, fi] of [[40, 1], [40, 13], [3, 1], [3, 13]]) {
  const need = await openAndScrollEditedItemToBottom(pool, fi);
  const g = await geom();
  console.log(`--- pool=${pool} fi=${fi} (需要滚动 ${Math.round(need.delta)}px，外层上限 ${need.outerMax}) ---`);
  if (!g.popAlive) { console.log("  浮层不存在（该条目不在可视区）\n"); continue; }
  console.log(`  浮层=[${g.pop.t},${g.pop.b}] scroller=[${g.sc.t},${g.sc.b}] 下缘被裁=${g.popClippedBelow}(${g.clipBelowPx}px) 可见切片=[${g.visTop},${g.visBottom}]`);
  console.log(`  外层=${g.outer}/${g.outerMax} 浮层内=${g.inner}/${g.innerMax} 高亮=#${g.activeIdx} 行y=${JSON.stringify(g.rows.slice(0, 3).map((r) => r.y))}`);

  // ① 鼠标在浮层可见切片内滚轮
  if (g.visBottom - g.visTop > 4) {
    const y = (g.visTop + g.visBottom) / 2;
    const b0 = await geom();
    await page.mouse.move(g.cx, y);
    await page.waitForTimeout(120);
    for (let i = 0; i < 10; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const b1 = await geom();
    const hit = await page.evaluate((args) => {
      const el = document.elementFromPoint(args[0], args[1]);
      return el ? `${el.tagName.toLowerCase()}.${typeof el.className === "string" ? el.className.split(/\s+/).filter(Boolean).join(".") : ""}` : "NONE";
    }, [g.cx, Math.round(y)]);
    console.log(`  ① 可见切片内滚轮 (y=${Math.round(y)}, 命中=${hit}): 外层 ${b0.outer}→${b1.outer} Δ${b1.outer - b0.outer} | 浮层内 ${b0.inner}→${b1.inner}${b1.outer !== b0.outer ? "  ★外层被带动" : ""}`);
  }

  // ② 鼠标"看着在列表里、实际被裁掉"的区域滚轮
  if (g.popClippedBelow && g.pop.b > g.sc.b + 6) {
    const y = Math.min(g.pop.b - 6, 374);
    await page.mouse.move(g.cx, y).catch(() => {});
    const hit = await page.evaluate((args) => {
      const el = document.elementFromPoint(args[0], args[1]);
      return el ? `${el.tagName.toLowerCase()}.${typeof el.className === "string" ? el.className.split(/\s+/).filter(Boolean).join(".") : ""}` : "NONE";
    }, [g.cx, Math.round(y)]);
    const b0 = await geom();
    await page.mouse.move(g.cx, y);
    await page.waitForTimeout(120);
    for (let i = 0; i < 6; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const b1 = await geom();
    console.log(`  ② 被裁区域 (y=${Math.round(y)}) 命中=${hit}: 外层 ${b0.outer}→${b1.outer} Δ${b1.outer - b0.outer}${b1.outer !== b0.outer ? "  ★外层被带动（但这是没打中，不是穿透）" : ""}`);
  }

  // ③ 高亮追进被裁区域：逐行 scrollIntoView（产品代码同一个 API）
  {
    const b0 = await geom();
    const res = await page.evaluate(() => {
      const sc = document.querySelector('[data-virtuoso-scroller="true"]');
      const pop = document.querySelector(".tag-edit-suggestions-popover");
      const out = [];
      for (const row of [...pop.children]) {
        const t0 = sc.scrollTop;
        row.scrollIntoView({ block: "nearest" });
        if (sc.scrollTop !== t0) out.push({ i: [...pop.children].indexOf(row), d: sc.scrollTop - t0 });
      }
      return out;
    });
    const b1 = await geom();
    console.log(`  ③ 逐行 scrollIntoView: 外层 ${b0.outer}→${b1.outer} Δ${b1.outer - b0.outer} | 带动外层的行=${JSON.stringify(res.slice(0, 5))}${b1.outer !== b0.outer ? "  ★★ 产品代码自己滚了外层" : ""}`);
  }

  // ④ 鼠标划过被裁区域边缘的行（真机用户把鼠标放在列表里）
  {
    const b0 = await geom();
    const ys = b0.rows.filter((r) => r.y + r.h > b0.sc.t + 2 && r.y < b0.sc.b - 2).map((r) => Math.max(b0.sc.t + 3, Math.min(r.y + r.h / 2, b0.sc.b - 3)));
    for (let round = 0; round < 2; round++) {
      for (const y of ys) { await page.mouse.move(b0.cx, y); await page.waitForTimeout(60); }
    }
    const b1 = await geom();
    console.log(`  ④ hover 划过可命中的 ${ys.length} 行: 外层 ${b0.outer}→${b1.outer} Δ${b1.outer - b0.outer}${b1.outer !== b0.outer ? "  ★外层被带动" : ""}`);
  }
  console.log("");
}

await browser.close();
