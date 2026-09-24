/**
 * vscroll-user.mjs — **按用户的操作顺序**在真机结构上重放，找穿透的可复现条件。
 *
 * 为什么要按顺序重放：前几轮的验证都是"页面加载 → 直接滚轮"。而用户的实际顺序是
 *
 *   ① 在主列表里滚动，把要打标签的条目滚到视野里（往往是中段/末段）
 *   ② 点那条的「标签」按钮进入编辑态  →  输入框获得焦点、候补浮层弹出
 *   ③ 把鼠标移到候补浮层上，开始滚
 *
 * 第 ① 步留下的**外层滚动位置**正是浮层会被 scroller 裁掉的原因；第 ② 步会让
 * `scrollIntoView`（ClipboardItem.tsx:895）在候补变化时跑一次。这两件事叠在一起，
 * 就是"鼠标明明在列表里，外层却动了"的候选成因。
 *
 * 本脚本只用真实组件（真实 Virtuoso + 真实 ClipboardItem），不注入任何补丁。
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
    const out = { outer: sc.scrollTop, outerMax: sc.scrollHeight - sc.clientHeight, sc: [Math.round(sr.top), Math.round(sr.bottom)], popAlive: !!pop };
    if (!pop) return out;
    const pr = pop.getBoundingClientRect();
    out.inner = pop.scrollTop;
    out.innerMax = pop.scrollHeight - pop.clientHeight;
    out.pop = [Math.round(pr.top), Math.round(pr.bottom)];
    out.cx = Math.round(pr.left + pr.width / 2);
    out.clipped = pr.bottom > sr.bottom || pr.top < sr.top;
    out.clipBelow = Math.round(Math.max(0, pr.bottom - sr.bottom));
    out.vis = [Math.round(Math.max(pr.top, sr.top)), Math.round(Math.min(pr.bottom, sr.bottom))];
    out.items = [...document.querySelectorAll(".tag-suggest-item")].map((el) => Math.round(el.getBoundingClientRect().y));
    return out;
  });

const hit = (x, y) =>
  page.evaluate((a) => {
    const el = document.elementFromPoint(a[0], a[1]);
    return el ? `${el.tagName.toLowerCase()}.${typeof el.className === "string" ? el.className.split(/\s+/).filter(Boolean).join(".") : ""}` : "NONE";
  }, [x, y]);

async function scenario(label, { n, pool, fi, preScroll, hoverRows, wheels }) {
  await page.goto(`${BASE}/src/vscroll.html?n=${n}&pool=${pool}&fi=${fi}`, { waitUntil: "load" });
  await page.waitForSelector(".tag-edit-suggestions-popover", { timeout: 20000 });
  await page.waitForTimeout(800);

  // ① 模拟"用户在主列表里滚动"（外层定位）
  if (preScroll) {
    await page.evaluate((t) => { document.querySelector('[data-virtuoso-scroller="true"]').scrollTop = t; }, preScroll);
    await page.waitForTimeout(400);
  }

  const s0 = await g();
  console.log(`\n【${label}】`);
  console.log(`  起点: 外层=${s0.outer} 浮层=${JSON.stringify(s0.pop)} scroller=${JSON.stringify(s0.sc)} 被裁=${s0.clipped}(下 ${s0.clipBelow}px) 可见=${JSON.stringify(s0.vis)} 浮层内max=${s0.innerMax}`);

  // ② 鼠标移到浮层可见切片的中心并稍作停留（用户"把鼠标放到列表里"）
  if (s0.vis[1] > s0.vis[0] + 3) {
    const cy = Math.round((s0.vis[0] + s0.vis[1]) / 2);
    await page.mouse.move(s0.cx, cy);
    await page.waitForTimeout(250);
    const h = await hit(s0.cx, cy);
    const s1 = await g();
    console.log(`  鼠标→(${s0.cx},${cy}) 命中=${h} | 外层 ${s0.outer}→${s1.outer} Δ${s1.outer - s0.outer} ${s1.outer !== s0.outer ? "★移入就动了" : ""}`);
    s0.outer = s1.outer;
  } else {
    console.log(`  可见切片太小，跳过鼠标移入`);
  }

  // ③ 在浮层里逐项划过（用户视线/鼠标在列表里移动）
  if (hoverRows) {
    const s = await g();
    const ys = s.items.filter((y) => y > s.sc[0] + 4 && y < s.sc[1] - 4).map((y) => y + 9);
    for (let r = 0; r < 2; r++) for (const y of ys) { await page.mouse.move(s.cx, y); await page.waitForTimeout(70); }
    const s2 = await g();
    console.log(`  hover 划过 ${ys.length} 行: 外层 ${s.outer}→${s2.outer} Δ${s2.outer - s.outer} ${s2.outer !== s.outer ? "★外层被带动" : ""}`);
  }

  // ④ 滚轮
  if (wheels) {
    const s = await g();
    const cy = Math.round((s.vis[0] + s.vis[1]) / 2);
    await page.mouse.move(s.cx, cy);
    await page.waitForTimeout(150);
    const s3 = await g();
    for (let i = 0; i < wheels; i++) { await page.mouse.wheel(0, 120); await page.waitForTimeout(25); }
    const s4 = await g();
    console.log(`  滚轮×${wheels} @y=${cy}: 外层 ${s3.outer}→${s4.outer} Δ${s4.outer - s3.outer} | 浮层内 ${s3.inner}→${s4.inner}(max ${s4.innerMax}) ${s4.outer !== s3.outer ? "★★★外层被带动" : ""}`);
  }
}

console.log("======== 用户操作顺序重放（真机结构：真实 Virtuoso + 真实 ClipboardItem） ========");
for (const pool of [3, 40]) {
  for (const fi of [1, 2, 4]) {
    for (const pre of [0, 60, 120, 189, 189 + 40]) {
      await scenario(`pool=${pool} fi=${fi} pre=${pre}`, { n: 14, pool, fi, preScroll: pre, hoverRows: true, wheels: 12 });
    }
  }
}

await browser.close();
