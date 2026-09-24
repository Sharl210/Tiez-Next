/**
 * cssvars-shots.mjs — 缺陷修复前后的对照截图。
 *
 * 用法：node tools/visual-harness/cssvars-shots.mjs <输出目录>
 *
 * 截图本身不判定缺陷（边框缺失在缩略图上可能看不出来），但 `measure.mjs` 的
 * 数字需要一处可肉眼核对的落点：当量测说"更新确认框修前无背景"时，
 * 对应的 PNG 应当能直接看出那块面板是透的。
 */
import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const OUT = process.argv[2] ?? "/tmp/cssfix/shots";
fs.mkdirSync(OUT, { recursive: true });

const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" };
const server = http.createServer((q, r) => {
  const u = decodeURIComponent(q.url.split("?")[0]);
  const p = path.join(ROOT, u === "/" ? "index.html" : u);
  fs.readFile(p, (e, b) => {
    if (e) { r.writeHead(404); r.end("nf"); return; }
    r.writeHead(200, { "Content-Type": MIME[path.extname(p)] ?? "application/octet-stream" });
    r.end(b);
  });
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});

const shots = [
  { name: "clipboard", stage: "clipboard", theme: "mica", colorMode: "light", w: 352, h: 900 },
  { name: "clipboard-dark", stage: "clipboard", theme: "mica", colorMode: "dark", w: 352, h: 900 },
  { name: "footer", stage: "footer", theme: "mica", colorMode: "light", w: 352, h: 620 },
  { name: "chat", stage: "chat", theme: "mica", colorMode: "light", w: 400, h: 640 },
  { name: "clipboard-retro", stage: "clipboard", theme: "retro", colorMode: "light", w: 352, h: 900 },
  { name: "clipboard-sakura", stage: "clipboard", theme: "sakura", colorMode: "light", w: 352, h: 900 },
];

for (const s of shots) {
  const page = await browser.newPage({ viewport: { width: s.w, height: s.h }, deviceScaleFactor: 2 });
  await page.goto(
    `http://127.0.0.1:${port}/src/cssvars.html?stage=${s.stage}&theme=${s.theme}&colorMode=${s.colorMode}`,
    { waitUntil: "networkidle" }
  );
  await page.waitForTimeout(s.stage === "footer" ? 1000 : 500);
  await page.screenshot({ path: path.join(OUT, `${s.name}.png`), fullPage: true });
  console.log(`wrote ${path.join(OUT, `${s.name}.png`)}`);
  await page.close();
}

await browser.close();
server.close();
