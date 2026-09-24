import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";

const ROOT = "tools/visual-harness/dist";
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" };
const server = http.createServer((req, res) => {
  const p = path.join(ROOT, decodeURIComponent(req.url.split("?")[0]) === "/" ? "index.html" : decodeURIComponent(req.url.split("?")[0]));
  fs.readFile(p, (e, b) => {
    if (e) { res.writeHead(404); res.end("nf"); return; }
    res.writeHead(200, { "Content-Type": MIME[path.extname(p)] ?? "application/octet-stream" });
    res.end(b);
  });
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

const browser = await chromium.launch({ executablePath: "/opt/google/chrome/chrome", args: ["--no-sandbox"] });
const shots = [
  { name: "groups-352-zh",        q: "lang=zh&mode=groups&theme=mica&colorMode=light",                w: 352,  h: 900 },
  { name: "groups-250-zh-min",    q: "lang=zh&mode=groups&theme=mica&colorMode=light",                w: 250,  h: 900 },
  { name: "groups-352-zh-dark",   q: "lang=zh&mode=groups&theme=mica&colorMode=dark",                 w: 352,  h: 900 },
  { name: "groups-352-en",        q: "lang=en&mode=groups&theme=mica&colorMode=light",                w: 352,  h: 900 },
  { name: "groups-352-tw",        q: "lang=tw&mode=groups&theme=mica&colorMode=light",                w: 352,  h: 900 },
  { name: "modal-352-zh",         q: "lang=zh&mode=modal&theme=mica&colorMode=light",                 w: 352,  h: 760 },
  { name: "modal-600-zh-wide",    q: "lang=zh&mode=modal&theme=mica&colorMode=light",                 w: 600,  h: 760 },
  { name: "menu-352-zh",          q: "lang=zh&mode=modal&theme=mica&colorMode=light&row=0",           w: 352,  h: 760 },
  { name: "menu-pinned-352-zh",   q: "lang=zh&mode=modal&theme=mica&colorMode=light&row=1",           w: 352,  h: 760 },
  { name: "confirm-delete-352-zh",q: "lang=zh&mode=modal&theme=mica&colorMode=light&row=0&click=delete",  w: 352, h: 760 },
  { name: "confirm-restore-352-zh",q:"lang=zh&mode=modal&theme=mica&colorMode=light&row=1&click=restore", w: 352, h: 760 },
  { name: "modal-352-zh-dark",    q: "lang=zh&mode=modal&theme=mica&colorMode=dark",                  w: 352,  h: 760 },
];

const out = [];
for (const s of shots) {
  const page = await browser.newPage({ viewport: { width: s.w, height: s.h }, deviceScaleFactor: 2 });
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
  await page.goto(`http://127.0.0.1:${port}/?${s.q}`, { waitUntil: "networkidle" });
  await page.waitForTimeout(900);

  // 采集结构化证据（类名是否存在、关键尺寸），不只看图
  const probe = await page.evaluate(() => {
    const q = (sel) => document.querySelector(sel);
    const rect = (el) => { if (!el) return null; const r = el.getBoundingClientRect(); return { w: +r.width.toFixed(1), h: +r.height.toFixed(1) }; };
    const groups = Array.from(document.querySelectorAll(".settings-group")).map((g) => ({
      title: g.querySelector("h3")?.textContent ?? "",
      rect: (() => { const r = g.getBoundingClientRect(); return { w: +r.width.toFixed(1), h: +r.height.toFixed(1) }; })(),
      items: g.querySelectorAll(".setting-item").length,
    }));
    const switches = Array.from(document.querySelectorAll(".switch")).map((s) => rect(s));
    const autoGroup = Array.from(document.querySelectorAll(".settings-group")).find((g) =>
      (g.querySelector("h3")?.textContent ?? "").includes("自动") || (g.querySelector("h3")?.textContent ?? "").toLowerCase().includes("auto")
    );
    const autoMaxKeep = autoGroup?.querySelector("[data-auto-backup-max-keep]");
    const dataGroup = Array.from(document.querySelectorAll(".settings-group")).find((g) =>
      (g.querySelector("h3")?.textContent ?? "").includes("数据") || (g.querySelector("h3")?.textContent ?? "").toLowerCase().includes("data")
    );
    const dataInput = dataGroup?.querySelector('input[type="number"]');
    return {
      viewport: { w: window.innerWidth, h: window.innerHeight },
      groups,
      switches,
      // 关键：样式是否真的生效（未被样式化的裸元素会是 0×0 或极窄）
      autoMaxKeepRect: rect(autoMaxKeep),
      autoMaxKeepBorder: autoMaxKeep ? getComputedStyle(autoMaxKeep).borderTopWidth : null,
      dataInputRect: rect(dataInput),
      dataInputBorder: dataInput ? getComputedStyle(dataInput).borderTopWidth : null,
      hasMenu: !!q(".tag-group-menu"),
      menuRect: rect(q(".tag-group-menu")),
      modalRect: rect(q("[data-backup-list-modal]")),
      rowCount: document.querySelectorAll("[data-backup-row]").length,
      untranslatedKeys: Array.from(document.querySelectorAll("*"))
        .filter((el) => el.children.length === 0)
        .map((el) => el.textContent ?? "")
        .filter((x) => /^[a-z][a-z0-9_]{6,}$/.test(x.trim())),
    };
  });

  const file = `tools/visual-harness/shots/${s.name}.png`;
  fs.mkdirSync("tools/visual-harness/shots", { recursive: true });
  await page.screenshot({ path: file });
  out.push({ name: s.name, file, errors, probe });
  await page.close();
}
await browser.close();
server.close();
fs.writeFileSync("tools/visual-harness/probe.json", JSON.stringify(out, null, 2));
console.log(JSON.stringify(out.map((o) => ({ name: o.name, errors: o.errors.length, groups: o.probe.groups.map((g) => `${g.title}:${g.rect.w}x${g.rect.h}/${g.items}`), sw: o.probe.switches[0], maxKeep: o.probe.autoMaxKeepRect, maxKeepBorder: o.probe.autoMaxKeepBorder, dataInput: o.probe.dataInputRect, dataBorder: o.probe.dataInputBorder, menu: o.probe.menuRect, modal: o.probe.modalRect, rows: o.probe.rowCount, untranslated: o.probe.untranslatedKeys })), null, 2));
