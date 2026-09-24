/**
 * measure.mjs — 把「CSS 变量是否生效」量成数字。
 *
 * # 为什么需要它
 *
 * 浏览器对 `var()` 解析失败是静默的：变量没定义 → 整条声明被丢弃 → 构建通过、
 * 类型检查通过、DOM 结构正常，只有 `getComputedStyle` 看得见（例如 border 变成
 * `0px`、background 变成 `rgba(0, 0, 0, 0)`）。既有验证台只看截图和 DOM，
 * 所以这批缺陷在它眼皮下存在了很久。
 *
 * 本脚本做三件事：
 *   1. 变量解析探针：对每个受关注的变量，在文档里造一个 `var(--x, SENTINEL)`
 *      的探针元素；computed 值等于 SENTINEL 就说明该变量在本轮未定义。
 *      这一层直接对应缺陷的根因，不依赖任何具体选择器。
 *   2. 字段量测：对真实组件渲染出的真实节点读 border/background/radius 等，
 *      证明"本该生效的样式生效了"。
 *   3. 跨主题 × 明暗遍历：同一个变量在 8 套主题下取值不同，只测一套会漏。
 *
 * 用法：node tools/visual-harness/measure.mjs [--json]
 */

import pw from "/root/Tiez-Next/node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const REPO = path.resolve(HERE, "../..");
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".map": "application/json" };

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

/**
 * 受关注的变量 = 本次缺陷清单。其中 `--scroll-dist` / `--marquee-duration` /
 * `--preview-*` / `--range-progress` / `--tm-sidebar-height` / `--advanced-sidebar-*` /
 * `--tag-sidebar-width` 由 TSX 在运行时写到元素上，所以它们在本探针里"未命中"
 * 是预期的——脚本把它们与"静态 CSS 应定义却缺失"的那批分开报告，避免把
 * 运行时注入的存在性误判成缺陷。
 */
const RUNTIME_INJECTED = new Set([
  "--scroll-dist",
  "--marquee-duration",
  "--preview-max-width",
  "--preview-max-height",
  "--preview-media-max-width",
  "--preview-media-max-height",
  "--preview-min-width",
  "--range-progress",
  "--tm-sidebar-width",
  "--tm-sidebar-height",
  "--advanced-sidebar-width",
  "--advanced-sidebar-height",
  "--custom-bg-image",
  "--custom-bg-opacity",
]);

const WATCHED = [
  "--accent-color-dark", "--accent-light", "--bg-hover", "--bg-main", "--bg-panel-rgb",
  "--bg-secondary", "--border", "--border-color", "--danger-color", "--input-bg",
  "--radius-lg", "--radius-md", "--radius-sm", "--shadow", "--tags-border-color",
  "--text-color", "--tag-sidebar-width", "--advanced-sidebar-width", "--advanced-sidebar-height",
  "--scrollbar-overlay-width", "--tm-sidebar-height",
  "--wt-peer-gradient-1", "--wt-peer-gradient-8",
  "--custom-bg-image", "--custom-bg-opacity",
  "--range-progress", "--marquee-duration", "--scroll-dist",
  "--preview-max-width", "--preview-max-height", "--preview-media-max-width",
  "--preview-media-max-height", "--preview-min-width",
];

/**
 * TagManager 的样式在组件内的 `<style>{...}` 字符串里，React 原样注入文档。
 * 想量它的规则不需要挂载组件（它依赖 12 个宿主命令与标签数据），只需把那段
 * 字符串原样取出来注入——与真实渲染逐字节一致。这里直接从源码抽取，
 * 不做任何改写，因此量到的就是线上规则。
 */
const tmSrc = fs.readFileSync(path.join(REPO, "src/features/tag/components/TagManager.tsx"), "utf8");
const tmStart = tmSrc.indexOf("<style>{`");
const tmEnd = tmSrc.indexOf("`}</style>", tmStart);
if (tmStart < 0 || tmEnd < 0) throw new Error("无法从 TagManager.tsx 抽出样式块");
const TAGMANAGER_CSS = tmSrc.slice(tmStart + "<style>{`".length, tmEnd);

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});

const THEMES = ["mica", "acrylic", "paper", "retro", "sakura", "sticky-note"];

/** 在页面里跑的采集函数（字符串化后注入，避免闭包捕获）。 */
const collect = ({ watched, tmCss, stage }) => {
  const out = { vars: {}, probes: {}, missing: [], present: [] };

  // ---- 1. 变量解析探针 ----
  const host = document.createElement("div");
  host.style.cssText = "position:absolute;left:-9999px;top:0";
  document.body.appendChild(host);
  for (const name of watched) {
    const el = document.createElement("div");
    const sentinel = `SENTINEL_${name.replace(/[^a-z0-9]/gi, "_")}`;
    el.style.setProperty("--probe-out", `var(${name}, ${sentinel})`);
    el.style.setProperty("color", `var(${name}, red)`);
    host.appendChild(el);
    const cs = getComputedStyle(el).getPropertyValue("--probe-out").trim();
    out.vars[name] = cs;
    if (cs === sentinel) out.missing.push(name); else out.present.push(name);
  }
  host.remove();

  // ---- 2. TagManager 样式块（与源码逐字节一致）----
  if (tmCss) {
    const s = document.createElement("style");
    s.textContent = tmCss;
    document.head.appendChild(s);
  }

  // ---- 3. 字段量测 ----
  const read = (sel, props, root = document) => {
    const el = root.querySelector(sel);
    if (!el) return { __missing: sel };
    const cs = getComputedStyle(el);
    const o = {};
    for (const p of props) o[p] = cs.getPropertyValue(p);
    return o;
  };
  const BG = ["background-color", "background-image"];
  const BORDER = ["border-top-width", "border-top-style", "border-top-color"];
  const RADIUS = ["border-top-left-radius"];
  const SHADOW = ["box-shadow"];
  const COLOR = ["color"];

  if (stage === "clipboard") {
    // 真实输入框：持久化上限的数值输入。修复前 border-top-width 应为 0px。
    out.probes.clipboardLimitInput = read('.settings-view input[type="number"]', [...BORDER, ...BG, ...COLOR]);
    // 内联选择行：用法是 rgba(var(--bg-panel-rgb), .5)，变量缺失时整条声明失效。
    out.probes.inlineChoiceRow = read(".settings-inline-choice-row", BG);
    out.probes.inlineChoiceBtn = read(".settings-inline-choice-btn", [...RADIUS, ...BG]);
    out.probes.rangeInput = read('.settings-view input[type="range"]', BG);
  } else if (stage === "footer") {
    // 更新确认框：修复前应无背景、无边框。
    const modal = document.querySelector(".settings-group");
    out.probes.updateModal = modal
      ? (() => {
          const cs = getComputedStyle(modal);
          return {
            "background-color": cs.getPropertyValue("background-color"),
            "background-image": cs.getPropertyValue("background-image"),
            "border-top-width": cs.getPropertyValue("border-top-width"),
            "border-top-color": cs.getPropertyValue("border-top-color"),
            "border-top-left-radius": cs.getPropertyValue("border-top-left-radius"),
          };
        })()
      : { __missing: "更新确认框未渲染（check() 未返回 Update）" };
  }

  // TagManager 规则量测：造一个带真实类名的骨架，用真实规则命中它。
  const skel = document.createElement("div");
  skel.className = "themed-tag-manager theme-mica";
  skel.innerHTML = `
    <div class="tag-sidebar"><div class="tag-search-box"><input /></div>
      <div class="tag-item"><span class="tag-name">A</span><span class="tag-badge">3</span></div>
      <div class="tag-item active"><span class="tag-name">B</span><span class="tag-badge">2</span></div>
      <div class="sort-btn"></div><div class="collapse-toggle"></div>
      <div class="view-toggle"><button class="toggle-btn"></button></div>
    </div>
    <div class="tag-content"><div class="items-grid">
      <div class="themed-card"><div class="card-top-row">
        <div class="card-actions-left"><button class="card-action-btn"></button></div></div>
        <div class="card-media"></div><div class="card-note"></div></div>
      </div>
      <div class="manage-mode"><div class="themed-card"><div class="selection-indicator"></div></div></div>
    </div>
    <div class="modal-overlay"><div class="confirm-dialog">
      <div class="modal-input-field"><input /></div>
      <div class="modal-buttons"><button class="confirm-dialog-button">取消</button>
      <button class="btn-save">保存</button></div>
    </div></div>`;
  document.body.appendChild(skel);

  if (stage === "clipboard" || stage === "footer") {
    out.probes.tm = {
      searchInput: read(".tag-search-box input", [...BORDER, ...BG, ...RADIUS], skel),
      tagItem: read(".tag-item", [...RADIUS, ...BG], skel),
      tagItemActive: read(".tag-item.active", BG, skel),
      tagBadge: read(".tag-badge", [...RADIUS, ...BG], skel),
      sortBtn: read(".sort-btn", [...RADIUS, ...BG], skel),
      viewToggle: read(".view-toggle", [...RADIUS, ...BG], skel),
      themedCard: read(".themed-card", [...RADIUS, ...BORDER, ...BG, ...SHADOW], skel),
      cardMedia: read(".card-media", [...RADIUS, ...BG], skel),
      cardNote: read(".card-note", [...RADIUS, ...BG], skel),
      cardActionBtn: read(".card-action-btn", [...RADIUS], skel),
      confirmDialog: read(".confirm-dialog", [...RADIUS, ...BORDER, ...BG], skel),
      confirmBtn: read(".confirm-dialog-button", [...RADIUS, ...BORDER, ...BG], skel),
      saveBtn: read(".btn-save", BG, skel),
      selectionIndicator: read(".manage-mode .selection-indicator", [...RADIUS, ...BORDER], skel),
      modalInput: read(".modal-input-field input", [...RADIUS, ...BORDER, ...BG], skel),
    };
  }
  skel.remove();
  return out;
};

const results = [];
for (const theme of THEMES) {
  for (const colorMode of ["light", "dark"]) {
    for (const stage of ["clipboard", "footer", "chat"]) {
      const page = await browser.newPage({
        viewport: { width: 352, height: 900 },
        deviceScaleFactor: 1,
      });
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e).slice(0, 300)));
      await page.goto(`http://127.0.0.1:${port}/src/cssvars.html?stage=${stage}&theme=${theme}&colorMode=${colorMode}`, {
        waitUntil: "networkidle",
      });
      await page.waitForTimeout(stage === "footer" ? 900 : 500);
      const r = await page.evaluate(collect, { watched: WATCHED, tmCss: TAGMANAGER_CSS, stage });
      if (stage === "chat") {
        r.probes.chatAvatars = await page.evaluate(() =>
          Array.from(document.querySelectorAll(".wt-avatar")).map((el) => {
            const cs = getComputedStyle(el);
            return {
              text: (el.textContent ?? "").trim().slice(0, 3),
              backgroundImage: cs.getPropertyValue("background-image"),
              backgroundColor: cs.getPropertyValue("background-color"),
            };
          })
        );
      }
      results.push({ theme, colorMode, stage, errors, ...r });
      await page.close();
    }
  }
}

await browser.close();
server.close();

// ---- 汇总 ----
const json = process.argv.includes("--json");
if (json) {
  console.log(JSON.stringify(results, null, 1));
} else {
  const staticMissing = new Set();
  for (const r of results) {
    for (const m of r.missing) if (!RUNTIME_INJECTED.has(m)) staticMissing.add(m);
  }
  console.log("=== 变量解析探针：静态 CSS 应定义却缺失的变量 ===");
  console.log(staticMissing.size ? [...staticMissing].sort().join("\n") : "（无）");

  const runtimeMissing = new Set();
  for (const r of results) for (const m of r.missing) if (RUNTIME_INJECTED.has(m)) runtimeMissing.add(m);
  console.log("\n=== 运行时注入变量（本页未触发注入，故缺失属预期）===");
  console.log([...runtimeMissing].sort().join(", ") || "（无）");

  console.log("\n=== mica/light 关键字段 ===");
  const pick = results.find((r) => r.theme === "mica" && r.colorMode === "light" && r.stage === "clipboard");
  if (pick) {
    console.log(JSON.stringify({ clipboard: pick.probes.clipboardLimitInput, inlineChoiceRow: pick.probes.inlineChoiceRow, tm: pick.probes.tm }, null, 1));
  }
  const foot = results.find((r) => r.theme === "mica" && r.colorMode === "light" && r.stage === "footer");
  console.log("\n=== footer 更新确认框 ===");
  console.log(JSON.stringify(foot?.probes?.updateModal, null, 1));

  console.log("\n=== 对端头像渐变（mica/light/chat）===");
  const chat = results.find((r) => r.theme === "mica" && r.colorMode === "light" && r.stage === "chat");
  console.log(JSON.stringify(chat?.probes?.chatAvatars, null, 1));

  const errs = results.filter((r) => r.errors.length);
  console.log(`\n=== 页面错误：${errs.length} 个页面有报错 ===`);
  for (const e of errs.slice(0, 4)) console.log(`  ${e.theme}/${e.colorMode}/${e.stage}: ${e.errors[0]}`);
}
