import pw from "/root/Tiez-Next/node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";

const ROOT = "tools/visual-harness/dist";
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" };
const server = http.createServer((req, res) => {
  const u = decodeURIComponent(req.url.split("?")[0]);
  const p = path.join(ROOT, u === "/" ? "index.html" : u);
  fs.readFile(p, (e, b) => {
    if (e) { res.writeHead(404); res.end("nf"); return; }
    res.writeHead(200, { "Content-Type": MIME[path.extname(p)] ?? "application/octet-stream" });
    res.end(b);
  });
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;
const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
fs.mkdirSync("tools/visual-harness/elements", { recursive: true });

const jobs = [
  { name: "group-auto-zh",      q: "lang=zh&mode=groups&theme=mica&colorMode=light", sel: "auto",  w: 352 },
  { name: "group-data-zh",      q: "lang=zh&mode=groups&theme=mica&colorMode=light", sel: "data",  w: 352 },
  { name: "group-auto-en",      q: "lang=en&mode=groups&theme=mica&colorMode=light", sel: "auto",  w: 352 },
  { name: "group-auto-tw",      q: "lang=tw&mode=groups&theme=mica&colorMode=light", sel: "auto",  w: 352 },
  { name: "group-auto-dark",    q: "lang=zh&mode=groups&theme=mica&colorMode=dark",  sel: "auto",  w: 352 },
  { name: "modal-full-zh",      q: "lang=zh&mode=modal&theme=mica&colorMode=light",  sel: "modal", w: 352 },
];

const out = [];
for (const j of jobs) {
  const page = await browser.newPage({ viewport: { width: j.w, height: 900 }, deviceScaleFactor: 2 });
  await page.goto(`http://127.0.0.1:${port}/?${j.q}`, { waitUntil: "networkidle" });
  await page.waitForTimeout(800);
  // 用 evaluate 在页面里挑元素（与 probe 用的是同一套选择逻辑），避免
  // Playwright filter 的 hasText 与 lucide 图标节点交互时的匹配问题。
  const handle = await page.evaluateHandle((sel) => {
    if (sel === "modal") return document.querySelector("[data-backup-list-modal]");
    const groups = Array.from(document.querySelectorAll(".settings-group"));
    const find = (needles) =>
      groups.find((g) => needles.some((n) => (g.querySelector("h3")?.textContent ?? "").includes(n)));
    return sel === "auto" ? find(["自动", "Automatic", "自動"]) : find(["数据管理", "Data Management", "資料管理"]);
  }, j.sel);
  const target = handle.asElement();
  if (!target) throw new Error(`找不到元素：${j.sel}`);
  const target_ = { screenshot: (o) => target.screenshot(o), boundingBox: () => target.boundingBox(), evaluate: (fn) => target.evaluate(fn) };
  const target2 = target_;
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.waitForTimeout(200);
  const file = `tools/visual-harness/elements/${j.name}.png`;
  await target2.screenshot({ path: file });
  const box = await target2.boundingBox();
  // 逐行量测：每个 setting-item 的行高与左右边界，用于"与邻居一致"的数值比对
  const rows = await target2.evaluate((el) =>
    Array.from(el.querySelectorAll(".setting-item")).map((r) => {
      const b = r.getBoundingClientRect();
      const label = r.querySelector(".item-label")?.textContent ?? "";
      const ctrl = r.querySelector("input, button, select");
      const cb = ctrl?.getBoundingClientRect();
      return {
        label: label.slice(0, 18),
        h: +b.height.toFixed(1),
        left: +b.left.toFixed(1),
        right: +b.right.toFixed(1),
        ctrl: cb ? { w: +cb.width.toFixed(1), h: +cb.height.toFixed(1), right: +cb.right.toFixed(1) } : null,
      };
    })
  );
  const pad = await target2.evaluate((el) => {
    const cs = getComputedStyle(el.querySelector(".group-content") ?? el);
    return { padding: cs.padding, gap: cs.gap };
  });
  out.push({ name: j.name, file, box, rows, pad });
  await page.close();
}
await browser.close(); server.close();
fs.writeFileSync("tools/visual-harness/elements.json", JSON.stringify(out, null, 2));
for (const o of out) {
  console.log(`\n=== ${o.name}  box=${JSON.stringify(o.box)}  group-content padding=${o.pad.padding}`);
  for (const r of o.rows) console.log(`   h=${String(r.h).padStart(5)} right=${String(r.right).padStart(6)} ctrl=${JSON.stringify(r.ctrl)} ${r.label}`);
}
