/**
 * backup-measure.mjs — 把「备份列表」弹窗的样式一致性量成数字。
 *
 * # 为什么不能只看截图
 *
 * 本次要修的缺陷属于「DOM 与几何都不变、只有 computed style 不对」那一类：
 * 三个按钮看起来"一个没边框一个有边框"、来源标签看起来"像按钮"、底部提示看起来
 * "几乎透明"——这些印象在截图上会随缩放、主题、屏幕而变，只有 `getComputedStyle`
 * 是稳定可复现的。因此本脚本读的是逐字段的 computed 值，并按 WCAG 计算对比度。
 *
 * # 用法
 *
 *   npm run harness:build && node tools/visual-harness/backup-measure.mjs
 *   node tools/visual-harness/backup-measure.mjs --json      # 机器可读
 *   node tools/visual-harness/backup-measure.mjs --theme mica --color-mode light
 *
 * 默认遍历 6 套主题 × 明/暗两态（仓库的 6 套主题都在 tauri.conf 的可选范围内）。
 */

import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".map": "application/json" };

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : fallback;
};
const THEMES = arg("theme") ? [arg("theme")] : ["mica", "acrylic", "paper", "retro", "sakura", "sticky-note"];
const MODES = arg("color-mode") ? [arg("color-mode")] : ["light", "dark"];
const WIDTHS = (arg("widths") ?? "352,720").split(",").map(Number);

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
 * 页面内采集函数（须自包含：会被序列化后注入）。
 *
 * 定位策略刻意**不依赖本次将要新增的类名**——否则"修前基线"就只能靠另一套选择器，
 * 两套选择器量出来的数不可比。这里用 data-* 锚点（弹窗自带）＋文本匹配（来自真实
 * locale）＋结构回退，修前修后是同一套。
 */
const collect = () => {
  const px = (v) => {
    const n = parseFloat(v);
    return Number.isFinite(n) ? +n.toFixed(2) : v;
  };
  const rgb = (s) => {
    const m = /rgba?\(([^)]+)\)/.exec(s ?? "");
    if (!m) return null;
    const parts = m[1].split(/[,/]/).map((x) => parseFloat(x.trim()));
    return [parts[0] ?? 0, parts[1] ?? 0, parts[2] ?? 0, parts.length > 3 && Number.isFinite(parts[3]) ? parts[3] : 1];
  };
  const lin = (c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  const lum = ([r, g, b]) => 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
  const contrast = (a, b) => {
    const la = lum(a), lb = lum(b);
    const [hi, lo] = la > lb ? [la, lb] : [lb, la];
    return +((hi + 0.05) / (lo + 0.05)).toFixed(2);
  };
  /** 沿祖先链把半透明背景逐层合成，得到该元素真正的视觉背景色。 */
  const effectiveBg = (el) => {
    const layers = [];
    let cur = el;
    while (cur && cur !== document.documentElement) {
      const c = rgb(getComputedStyle(cur).backgroundColor);
      if (c && c[3] > 0) layers.push(c);
      if (c && c[3] >= 1) break;
      cur = cur.parentElement;
    }
    const base = [255, 255, 255];
    let acc = base;
    // 从最外层往里合成
    for (let i = layers.length - 1; i >= 0; i--) {
      const [r, g, b, a] = layers[i];
      acc = [acc[0] * (1 - a) + r * a, acc[1] * (1 - a) + g * a, acc[2] * (1 - a) + b * a];
    }
    return acc.map((x) => Math.round(x));
  };
  const props = (el, list) => {
    if (!el) return null;
    const cs = getComputedStyle(el);
    const out = {};
    for (const p of list) {
      const v = cs.getPropertyValue(p);
      out[p] = /width|radius|size|height|padding|gap|spacing|top$/.test(p) && /px$/.test(v) ? px(v) : v;
    }
    const b = el.getBoundingClientRect();
    out.__w = +b.width.toFixed(1);
    out.__h = +b.height.toFixed(1);
    out.__text = (el.textContent ?? "").trim().slice(0, 24);
    return out;
  };

  const modal = document.querySelector("[data-backup-list-modal]");
  if (!modal) return { __error: "弹窗未渲染" };

  const BTN = [
    "border-top-width", "border-top-style", "border-top-color", "background-color",
    "border-top-left-radius", "padding-top", "padding-left", "font-size", "font-weight",
    "height", "line-height", "display", "align-items", "gap", "box-shadow", "color",
  ];
  const TAG = [
    "border-top-width", "border-top-style", "background-color", "border-top-left-radius",
    "padding-top", "padding-left", "font-size", "font-weight", "height", "line-height",
    "display", "align-items", "gap", "color", "letter-spacing", "text-transform", "opacity",
  ];

  // ---- 关闭按钮：标题行（`.modal-title` 的父元素）里的那个按钮 ----
  const closeBtn = modal.querySelector(".modal-title")?.parentElement?.querySelector("button") ?? null;

  // ---- 工具栏按钮：弹窗直接子元素里含 ≥2 个 button 的那个 div 中的所有按钮 ----
  // 不用序号切 `querySelectorAll("button.btn-icon")`：关闭按钮也在弹窗内且排在前面，
  // 按序号切会把关闭按钮误当"刷新按钮"，量出来的自然全是 `.btn-icon` 的默认值。
  const toolbarRow = Array.from(modal.children).find(
    (el) => el.querySelectorAll("button").length >= 2 && !el.querySelector(".modal-title")
  );
  const toolbarButtons = toolbarRow ? Array.from(toolbarRow.querySelectorAll("button")) : [];

  // ---- 来源标签：带 data 锚点的优先，否则退回"行内第一个无 data 的 span" ----
  const row = document.querySelector("[data-backup-row]");
  const originTag =
    document.querySelector("[data-backup-origin]") ??
    (row
      ? Array.from(row.querySelectorAll("span")).find(
          (s) => !s.hasAttribute("data-backup-pinned") && s !== row
        ) ?? null
      : null);
  const pinnedTag = document.querySelector("[data-backup-pinned]");

  // ---- 底部提示：弹窗最后一个子 div（含 auto_backup_row_hint 文案）----
  const children = Array.from(modal.children);
  const hintEl = children[children.length - 1];

  // ---- 汇总行 / 目录行：标题下面的那个信息块 ----
  const titleRow = modal.firstElementChild;
  const infoBlock = titleRow?.nextElementSibling ?? null;
  const countLine = infoBlock?.querySelector("div") ?? null;
  const dirLine = infoBlock?.querySelector("div:nth-child(2)") ?? null;

  /**
   * 读文字对比度。
   *
   * 【为什么必须把 `opacity` 算进去】元素上的 `opacity: 0.8` 不是"文字变淡 20%"，
   * 而是**整个元素（含文字）与下方背景按 80/20 合成**。只算 `color` 与背景的对比度
   * 会得到偏高的值（本弹窗底部提示实测就是这样：忽略 opacity 是 4.7，算进去只有 3.5）。
   * 祖先链上的 opacity 同样生效，因此整条链都要乘进去。
   */
  const readContrast = (el) => {
    if (!el) return null;
    const fg = rgb(getComputedStyle(el).color);
    const bg = effectiveBg(el);
    let alpha = fg ? fg[3] : 1;
    let cur = el;
    while (cur && cur !== document.documentElement) {
      const o = parseFloat(getComputedStyle(cur).opacity);
      if (Number.isFinite(o)) alpha *= o;
      cur = cur.parentElement;
    }
    const effFg = fg
      ? [0, 1, 2].map((i) => Math.round(fg[i] * alpha + bg[i] * (1 - alpha)))
      : null;
    return {
      color: getComputedStyle(el).color,
      opacity: getComputedStyle(el).opacity,
      effectiveFg: effFg ? `rgb(${effFg.join(", ")})` : null,
      effectiveBg: `rgb(${bg.join(", ")})`,
      contrast: effFg ? contrast(effFg, bg) : null,
      fontSize: getComputedStyle(el).fontSize,
    };
  };

  // 弹窗标题（对照：底部提示应该弱于标题但必须可读）
  const title = modal.querySelector(".modal-title");

  return {
    toolbarButtons: toolbarButtons.map((b) => props(b, BTN)),
    originTag: props(originTag, TAG),
    originTagCursor: originTag ? getComputedStyle(originTag).cursor : null,
    originTagRole: originTag ? originTag.getAttribute("role") : null,
    originTagAriaHidden: originTag ? originTag.getAttribute("aria-hidden") : null,
    originTagHasIcon: originTag ? originTag.querySelector("svg") !== null : null,
    pinnedTag: props(pinnedTag, TAG),
    closeBtn: props(closeBtn, BTN),
    closeBtnClass: closeBtn ? closeBtn.className : null,
    hint: { ...(props(hintEl, [...TAG, "color"]) ?? {}), ...(readContrast(hintEl) ?? {}) },
    countLine: props(countLine, ["font-size", "font-weight", "color", "line-height", "opacity", "margin-top"]),
    dirLine: props(dirLine, ["font-size", "font-weight", "color", "line-height", "opacity", "margin-top"]),
    title: props(title, ["font-size", "font-weight", "color"]),
    row: props(row, ["padding-top", "padding-left", "border-top-width", "border-top-left-radius", "background-color"]),
    // 反向对照锚点：证明本脚本确实读到了真实样式（若这个值恒为 0，说明选择器整体没命中）
    modal: props(modal, ["background-color", "border-top-width", "border-top-left-radius", "padding-top"]),
  };
};

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox", "--force-color-profile=srgb"],
});

const results = [];
for (const theme of THEMES) {
  for (const colorMode of MODES) {
    for (const width of WIDTHS) {
      const page = await browser.newPage({ viewport: { width, height: 900 }, deviceScaleFactor: 1 });
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e).slice(0, 300)));
      await page.goto(
        `http://127.0.0.1:${port}/?lang=zh&mode=modal&row=none&theme=${theme}&colorMode=${colorMode}`,
        { waitUntil: "networkidle" }
      );
      await page.waitForTimeout(600);
      const r = await page.evaluate(collect);
      results.push({ theme, colorMode, width, errors, ...r });
      await page.close();
    }
  }
}
await browser.close();
server.close();

if (argv.includes("--json")) {
  console.log(JSON.stringify(results, null, 1));
} else {
  const short = (v) => (typeof v === "number" ? String(v) : String(v ?? "-"));
  for (const r of results) {
    if (r.__error) { console.log(`${r.theme}/${r.colorMode}/${r.width}: ${r.__error}`); continue; }
    console.log(`\n${"=".repeat(78)}\n${r.theme} / ${r.colorMode} / ${r.width}px${r.errors.length ? `  [pageerror × ${r.errors.length}]` : ""}`);
    console.log("  工具栏按钮（刷新 / 立即备份 / 打开文件夹）:");
    for (const [i, b] of r.toolbarButtons.entries()) {
      console.log(
        `    #${i} border=${short(b["border-top-width"])}/${short(b["border-top-style"])} bg=${b["background-color"]} radius=${short(b["border-top-left-radius"])} h=${short(b.height)} pad=${short(b["padding-top"])}/${short(b["padding-left"])} fs=${b["font-size"]} gap=${short(b.gap)} shadow=${String(b["box-shadow"]).slice(0, 28)}`
      );
    }
    const t = r.originTag;
    console.log("  来源标签 vs 按钮 #0:");
    console.log(`    标签 border=${short(t["border-top-width"])}/${short(t["border-top-style"])} bg=${t["background-color"]} radius=${short(t["border-top-left-radius"])} h=${short(t.height)} pad=${short(t["padding-top"])}/${short(t["padding-left"])} fs=${t["font-size"]} fw=${t["font-weight"]} cursor=${r.originTagCursor} svg=${r.originTagHasIcon} letterSpacing=${short(t["letter-spacing"])} transform=${t["text-transform"]}`);
    console.log(`    按钮 h=${short(r.toolbarButtons[0]?.height)} fs=${r.toolbarButtons[0]?.["font-size"]} radius=${short(r.toolbarButtons[0]?.["border-top-left-radius"])}`);
    console.log(`  关闭按钮 cls="${r.closeBtnClass}" h=${short(r.closeBtn?.height)} w=${short(r.closeBtn?.__w)} radius=${short(r.closeBtn?.["border-top-left-radius"])} border=${short(r.closeBtn?.["border-top-width"])}`);
    console.log(`  底部提示 color=${r.hint.color} opacity=${r.hint.opacity} bg=${r.hint.effectiveBg} 对比度=${r.hint.contrast} fs=${r.hint.fontSize}`);
    console.log(`  汇总行 fs=${short(r.countLine?.["font-size"])} fw=${short(r.countLine?.["font-weight"])} opacity=${short(r.countLine?.opacity)}`);
    console.log(`  目录行 fs=${short(r.dirLine?.["font-size"])} fw=${short(r.dirLine?.["font-weight"])} opacity=${short(r.dirLine?.opacity)}`);
    console.log(`  标题   fs=${short(r.title?.["font-size"])} fw=${short(r.title?.["font-weight"])}`);
  }
  const errs = results.filter((r) => r.errors.length);
  console.log(`\n=== 页面错误：${errs.length} 个页面有报错 ===`);
  for (const e of errs.slice(0, 3)) console.log(`  ${e.theme}/${e.colorMode}: ${e.errors[0]}`);
}
