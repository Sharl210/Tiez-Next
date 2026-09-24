/**
 * collapse.mjs — 用真实 Chrome 量「条目卡与编辑器会不会坍缩」。
 *
 * # 为什么需要它（而不是再加一条组件断言）
 *
 * `.content-preview` 是 `-webkit-box` + `-webkit-line-clamp:4` + `overflow:hidden`；
 * 富文本预览有三条互斥路径（`<img>` SVG 快照 / `HtmlContent` 注入 HTML / 纯文本兜底）；
 * 条目卡本身是 `display:flex; flex-direction:column; height:100%`。
 *
 * 以上任何一处让高度变成 0，DOM 结构、类名、文案、`title` **全都依然正确** ——
 * `getByText(...)` 之类的断言一条都不会变红。只有几何量测能看见。
 * 本仓库已有同类教训：`var()` 取不到值时整条声明被静默丢弃（`check-css-vars.mjs` 就是
 * 为此而生），而「高度归零」是同一类**静默失败**。
 *
 * # 反向对照
 *
 * 脚本自带 `--sabotage`：注入一条把高度打成 0 的样式（并清掉 `min-height`），
 * 然后要求同一套断言**必须变红**。否则这些断言就是恒真的空断言。
 * 这是本仓库明确要求过的方法（见 `README.md` 的「为什么需要反向对照」）。
 *
 * # 用法
 *
 *   node tools/visual-harness/collapse.mjs                # 量测并断言
 *   node tools/visual-harness/collapse.mjs --sabotage     # 反向对照（必须失败）
 *   node tools/visual-harness/collapse.mjs --json         # 机器可读
 *
 * 前置：`npm run harness:build`
 *
 * # 边界（必须如实说明）
 *
 * 验证台的 `invoke` 是 mock、`listen` 是 no-op ⇒ 只证明渲染与样式，不证明命令行为、
 * 不证明真机事件时序。真机（Windows Tauri 应用）在 WSL2 上无法启动，故未经真机验证。
 */

import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "dist");
const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".css": "text/css",
  ".svg": "image/svg+xml",
  ".map": "application/json",
};

const argv = process.argv.slice(2);
const SABOTAGE = argv.includes("--sabotage");
const AS_JSON = argv.includes("--json");

/** 真机主窗口默认 352×380（`src-tauri/tauri.conf.json`）；高度给足以便一屏看清。 */
const VIEWPORT = { width: 352, height: 780 };

/**
 * 判据阈值。
 *
 * # 这些数字表达的是"不变量"，不是"当前实现恰好产出多少"
 *
 * 判据必须能回答"**凭什么**是这个值"。取"实测值减一点"是反模式：实现变了判据就失效，
 * 而且会让人为了让数字好看去改**判据**而不是改**实现**。这里的每个数字都能追溯到
 * 一条与实现无关的理由：
 *
 * - `MIN_PREVIEW_HEIGHT = 8`（有一条**可辨认**的预览）：只判"非 0"会让 1px 也算通过，
 *   而 1px 在界面上等于什么都看不见。8px 是"能看见有东西"的最低量级。
 * - `MIN_TEXTAREA_HEIGHT = 40`（可输入）：低于此值无法舒适地看到一行文字与光标。
 * - `MIN_DIALOG_HEIGHT = 60`（可操作）：标题 + 一行内容 + 按钮的最小可用高度。
 * - `MIN_ITEM_HEIGHT = 28`（一行元信息 + 内容）：见下方 `MIN_ITEM_HEIGHT` 的说明。
 *
 * ⚠️ **`MIN_FRAGILE_PREVIEW_HEIGHT` 的取值已修正**（原先为 24，无依据）：
 * 「内容没有固有高度」这一组（空富文本、图片加载失败）的唯一硬要求是
 * **预览仍占满一个整行** —— 即高度 ≥ 当前行高。行高由
 * `--content-line-height`（1.45）× `--clipboard-item-font-size`（13px）= **18.85px**
 * 决定，且会随用户字号设置变化。
 *
 * 原先写死 24 是"比实测的 22.19 高一点"，属于**用实测值反推判据**：它既说不出
 * 为什么是 24（19 行不行？），又会在行高随字号变大时自动失效。改为**按行高判定**
 * 后，判据在任何字号下都成立，且理由清楚 —— 预览不许比一行还矮。
 */
const MIN_ITEM_HEIGHT = 28;
const MIN_PREVIEW_HEIGHT = 8;
const MIN_DIALOG_HEIGHT = 60;
const MIN_TEXTAREA_HEIGHT = 40;

/**
 * 「无固有高度」那组（空富文本、图片加载失败）的高危判据 —— **由页面内实测行高赋值**。
 *
 * 初值刻意用 `NaN` 而不是 0：若测量段因故没跑到，`NaN` 让所有比较为 false，
 * 这组断言会**全部变红**；而 0 会让它们**全部假绿**。失败要往安全的方向倒。
 */
let MIN_FRAGILE_PREVIEW_HEIGHT = Number.NaN;

/**
 * 在页面里实测"一行内容"的高度。
 *
 * 用真实计算样式，而不是读 CSS 变量再自己乘 —— 后者会把「变量改了但规则没生效」
 * 这类问题一起漏掉。测法：挂一个只含一个字符的 `.content-preview`，量它的高度。
 */
async function measureLineHeight(page) {
  return page.evaluate(() => {
    const probe = document.createElement("div");
    probe.className = "content-preview";
    probe.textContent = "一";
    probe.style.position = "absolute";
    probe.style.visibility = "hidden";
    probe.style.width = "200px";
    document.body.appendChild(probe);
    const h = probe.getBoundingClientRect().height;
    probe.remove();
    return h;
  });
}

const server = http.createServer((q, r) => {
  const u = decodeURIComponent(q.url.split("?")[0]);
  const p = path.join(ROOT, u === "/" ? "index.html" : u);
  fs.readFile(p, (e, b) => {
    if (e) {
      r.writeHead(404);
      r.end("nf");
      return;
    }
    r.writeHead(200, {
      "Content-Type": MIME[path.extname(p)] ?? "application/octet-stream",
    });
    r.end(b);
  });
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

const browser = await chromium.launch({
  executablePath: "/opt/google/chrome/chrome",
  args: ["--no-sandbox"],
});
const page = await browser.newPage({ viewport: VIEWPORT });

// ── 先量一次"一行内容"的真实高度 ──────────────────────────────────────────
// 「内容无固有高度」那组的高危判据要用它（见文件头对阈值的说明）。
// 放在这里而不是写死常数：行高随用户字号设置变化，写死会在改字号后失效，
// 而"失效"的表现是**判据悄悄变松**（阈值比实际行高小），比报错更危险。
// 也**不能**留 0：阈值 0 会让所有高危断言无条件通过 = 假绿。
{
  await page.goto(`http://127.0.0.1:${port}/src/collapse.html`, { waitUntil: "load" });
  await page.waitForSelector("[data-test-clipboard-item]", { timeout: 15000 });
  const lh = await measureLineHeight(page);
  if (!(lh > 0)) {
    throw new Error(
      `实测行高为 ${lh}，拒绝继续：阈值退化成 0 会让「无固有高度」那组断言全部假绿`
    );
  }
  MIN_FRAGILE_PREVIEW_HEIGHT = Math.round(lh * 100) / 100;
  console.log(`  实测「一行内容」高度 = ${MIN_FRAGILE_PREVIEW_HEIGHT}px（高危判据用此值）`);
}

/**
 * 反向对照要注入的破坏样式。
 *
 * 只有一条 CSS，攻击的正是「高度」这一维度：把它打成 0 并同时清掉 `min-height`
 * （否则 `min-height` 会兜住高度，破坏就注入不进去 —— 那会让反向对照假绿）。
 */
const SABOTAGE_CSS = `
  .history-item { height: 0 !important; min-height: 0 !important; padding: 0 !important; }
  .content-preview { height: 0 !important; min-height: 0 !important; overflow: hidden !important; }
  .content-preview-shell { height: 0 !important; min-height: 0 !important; }
  .entry-body-editor-dialog, .entry-note-editor-dialog { height: 0 !important; min-height: 0 !important; padding: 0 !important; }
  .entry-body-editor-textarea, .entry-note-editor-textarea { height: 0 !important; min-height: 0 !important; }
`;

const installSabotage = async () => {
  if (!SABOTAGE) return;
  await page.addStyleTag({ content: SABOTAGE_CSS });
  // 样式注入是同步的，但让布局稳定后再量。
  await page.waitForTimeout(80);
};

/** 一个用例的量测动作：进页面、等渲染、必要时注入破坏、读几何。 */
const probe = async (query, evaluate) => {
  await page.goto(`http://127.0.0.1:${port}/src/collapse.html${query}`, {
    waitUntil: "load",
  });
  await page.waitForSelector("[data-test-clipboard-item]", { timeout: 15000 });
  // 富文本快照分支要等 SVG → data URL → <img> 解码，兜底计时器是 700ms。
  await page.waitForTimeout(1100);
  await installSabotage();
  return page.evaluate(evaluate);
};

/** 卡片量测：读每一项的**真实** `getBoundingClientRect()` 与计算样式。 */
const measureCardEvaluate = () => {
  const round = (n) => Math.round(n * 100) / 100;
  const item = document.querySelector(".history-item");
  if (!item) return { error: "没有 .history-item" };

  const rect = (el) => {
    if (!el) return null;
    const b = el.getBoundingClientRect();
    return { w: round(b.width), h: round(b.height), x: round(b.x), y: round(b.y) };
  };

  const preview = item.querySelector(".content-preview");
  const shell = item.querySelector(".content-preview-shell");
  const meta = item.querySelector(".item-meta");
  const actions = item.querySelector(".item-actions");

  // 按钮用 **lucide 的图标类名**识别，不用 title 文案：文案正在被 i18n 改造，
  // 用文案当判据会让"改了文案"和"按钮消失"两种失败混在一起。
  const hasIcon = (name) => !!item.querySelector(`svg.lucide-${name}`);
  const buttonTitles = Array.from(item.querySelectorAll(".item-actions button")).map(
    (b) => b.getAttribute("title") ?? ""
  );

  const cs = preview ? getComputedStyle(preview) : null;
  const img = preview?.querySelector("img");

  return {
    item: rect(item),
    meta: rect(meta),
    shell: rect(shell),
    preview: rect(preview),
    actions: rect(actions),
    previewStyle: cs
      ? {
          display: cs.display,
          overflow: cs.overflow,
          maxHeight: cs.maxHeight,
          lineClamp: cs.webkitLineClamp,
          fontSize: cs.fontSize,
        }
      : null,
    /** 预览里真的看得见的字符数（`innerText` 会尊重 `display:none`）。 */
    previewText: (preview?.innerText ?? "").trim().length,
    previewChildTags: preview
      ? Array.from(preview.children).map((c) => c.tagName.toLowerCase())
      : [],
    imgBox: img ? rect(img) : null,
    imgNatural: img ? { w: img.naturalWidth, h: img.naturalHeight } : null,
    buttons: {
      bodyEdit: hasIcon("pencil"),
      noteEdit: hasIcon("sticky-note"),
      open: hasIcon("external-link"),
      pin: hasIcon("pin") || hasIcon("pin-off"),
      tag: hasIcon("tag"),
      tagTransfer: hasIcon("folder-input"),
      del: hasIcon("x"),
    },
    buttonTitles,
  };
};

/** 弹窗量测：正文编辑器 / 备注编辑器都是 portal 到 `<body>` 的固定定位盒。 */
const measureDialogEvaluate = () => {
  const round = (n) => Math.round(n * 100) / 100;
  const rect = (el) => {
    if (!el) return null;
    const b = el.getBoundingClientRect();
    return { w: round(b.width), h: round(b.height) };
  };
  const overlay = document.querySelector(".modal-overlay");
  const bodyDialog = document.querySelector(".entry-body-editor-dialog");
  const noteDialog = document.querySelector(".entry-note-editor-dialog");
  const dialog = bodyDialog ?? noteDialog;
  const ta = dialog?.querySelector("textarea");
  return {
    overlay: rect(overlay),
    dialog: rect(dialog),
    dialogClass: dialog?.className ?? null,
    /** 弹窗里的**可见**字符数 —— 空弹窗（高度归零 / 内容丢失）会掉到 0。 */
    dialogText: (dialog?.innerText ?? "").replace(/\s+/g, " ").trim().length,
    textarea: rect(ta),
    textareaValue: ta?.value ?? null,
    saving: !!dialog?.querySelector("button[disabled]"),
  };
};

const CARDS = [
  "text",
  "code",
  "url",
  "rich_html",
  "rich_table",
  "rich_broken_img",
  "rich_noise",
  "rich_img_only",
  "rich_empty",
  "image",
  "image_missing",
  "file",
  "video",
  "emoji_sync",
  "unknown_type",
];

/** 每种类型的「编辑内容 / 编辑备注」应当具备与否 —— 与产品判据同源（见 types.ts）。 */
const BTN_MATRIX = {
  text: { bodyEdit: true, noteEdit: false },
  code: { bodyEdit: true, noteEdit: false },
  url: { bodyEdit: true, noteEdit: false },
  rich_html: { bodyEdit: true, noteEdit: false },
  rich_table: { bodyEdit: true, noteEdit: false },
  rich_broken_img: { bodyEdit: true, noteEdit: false },
  rich_noise: { bodyEdit: true, noteEdit: false },
  rich_img_only: { bodyEdit: true, noteEdit: false },
  rich_empty: { bodyEdit: true, noteEdit: false },
  image: { bodyEdit: false, noteEdit: true },
  image_missing: { bodyEdit: false, noteEdit: true },
  file: { bodyEdit: false, noteEdit: true },
  video: { bodyEdit: false, noteEdit: true },
  emoji_sync: { bodyEdit: false, noteEdit: true },
  unknown_type: { bodyEdit: false, noteEdit: true },
};

/**
 * 「坍缩高危」用例：这些夹具的内容**本身就没有固有高度**（坏图 / 零高元素 / 文件缺失）。
 * 它们必须靠**布局兜底**（高度下限）才能不塌；只判"非 0"会被 1~3px 蒙过去，
 * 所以这一组单独用更严的下限。数字取在「肉眼能看见一块东西」的量级。
 */
const FRAGILE_CASES = ["rich_img_only", "rich_empty", "image_missing", "rich_broken_img"];

/**
 * 「无固有高度」这一组的判据：**预览必须仍占满一整行**。
 *
 * 理由见上方阈值注释 —— 写死像素值等于用实测值反推判据。这里改为从**实测行高**
 * 推导（`--content-line-height` × 字号），字号变化时判据依然成立。
 * 留 0.5px 容差给子像素舍入。
 */


const findings = [];

const check = (label, ok, detail) => {
  findings.push({ label, ok, detail });
};

const cardResults = {};
for (const id of CARDS) {
  const r = await probe(`?case=${id}&theme=mica&colorMode=light`, measureCardEvaluate);
  cardResults[id] = r;
  if (r.error) {
    check(`[卡片 ${id}] 渲染`, false, r.error);
    continue;
  }
  check(
    `[卡片 ${id}] 条目高度 ≥ ${MIN_ITEM_HEIGHT}px`,
    r.item.h >= MIN_ITEM_HEIGHT,
    `item.h=${r.item.h}`
  );
  check(
    `[卡片 ${id}] 预览高度 ≥ ${MIN_PREVIEW_HEIGHT}px`,
    r.preview !== null && r.preview.h >= MIN_PREVIEW_HEIGHT,
    `preview.h=${r.preview?.h ?? "null"}`
  );
  check(
    `[卡片 ${id}] 预览有可见内容`,
    r.previewText > 0 || r.previewChildTags.length > 0,
    `text=${r.previewText} children=[${r.previewChildTags}]`
  );
  if (FRAGILE_CASES.includes(id)) {
    check(
      `[卡片 ${id}] ⚠️ 高危：内容无固有高度时预览仍需 ≥ ${MIN_FRAGILE_PREVIEW_HEIGHT}px`,
      r.preview !== null && r.preview.h >= MIN_FRAGILE_PREVIEW_HEIGHT,
      `preview.h=${r.preview?.h ?? "null"}（内容本身没有高度，必须靠布局兜底）`
    );
  }
  const want = BTN_MATRIX[id];
  check(
    `[卡片 ${id}] 「编辑内容」按钮 ${want.bodyEdit ? "存在" : "不存在"}`,
    r.buttons.bodyEdit === want.bodyEdit,
    `bodyEdit=${r.buttons.bodyEdit}`
  );
  check(
    `[卡片 ${id}] 「编辑备注」按钮 ${want.noteEdit ? "存在" : "不存在"}`,
    r.buttons.noteEdit === want.noteEdit,
    `noteEdit=${r.buttons.noteEdit}`
  );
}

// ---- 富文本：三种渲染分支各自的真实几何 ----
for (const id of ["rich_html", "rich_table", "rich_broken_img", "rich_noise"]) {
  const r = cardResults[id];
  if (!r || r.error) continue;
  check(
    `[富文本 ${id}] 预览分支产物非空`,
    r.previewChildTags.length > 0 || r.previewText > 0,
    `children=[${r.previewChildTags}] text=${r.previewText} imgBox=${JSON.stringify(r.imgBox)} natural=${JSON.stringify(r.imgNatural)}`
  );
}

// ---- 编辑器弹窗：正文 / 备注各开一次 ----
const dialogResults = {};
for (const [id, kind] of [
  ["text", "body"],
  ["rich_html", "body"],
  ["image", "note"],
  ["file", "note"],
  ["video", "note"],
  ["emoji_sync", "note"],
  ["unknown_type", "note"],
]) {
  const r = await probe(`?case=${id}&open=${kind}&theme=mica&colorMode=light`, measureDialogEvaluate);
  dialogResults[`${id}:${kind}`] = r;
  check(
    `[弹窗 ${id}:${kind}] 弹窗出现`,
    !!r.dialog,
    `class=${r.dialogClass}`
  );
  check(
    `[弹窗 ${id}:${kind}] 弹窗高度 ≥ ${MIN_DIALOG_HEIGHT}px`,
    !!r.dialog && r.dialog.h >= MIN_DIALOG_HEIGHT,
    `dialog.h=${r.dialog?.h}`
  );
  check(
    `[弹窗 ${id}:${kind}] 弹窗有可见文案`,
    r.dialogText > 0,
    `text=${r.dialogText}`
  );
  check(
    `[弹窗 ${id}:${kind}] 输入框高度 ≥ ${MIN_TEXTAREA_HEIGHT}px`,
    !!r.textarea && r.textarea.h >= MIN_TEXTAREA_HEIGHT,
    `textarea.h=${r.textarea?.h}`
  );
  // 正文编辑器只对可编辑正文的类型出现；二进制类型给的是备注编辑器。
  check(
    `[弹窗 ${id}:${kind}] 弹窗种类正确`,
    kind === "body"
      ? r.dialogClass?.includes("entry-body-editor-dialog")
      : r.dialogClass?.includes("entry-note-editor-dialog"),
    `class=${r.dialogClass}`
  );
}

// ---- 紧凑模式：另一套高度与 hover 规则（只量几种代表类型） ----
const compactResults = {};
for (const id of ["text", "rich_html", "image", "file"]) {
  const r = await probe(`?case=${id}&compact=1&theme=mica&colorMode=light`, measureCardEvaluate);
  compactResults[id] = r;
  check(
    // ⚠️ 紧凑模式**不能**用 MIN_ITEM_HEIGHT：那个 28px 的依据是
    // 「一行元信息 + 一行内容」，而紧凑模式**刻意隐藏了元信息行**
    // （`compact-mode.css` 的 `.history-item.compact .item-meta { display: none }`）。
    // 用 28 会把它判成缺陷，而它正是紧凑模式该有的样子。
    //
    // 紧凑模式真正的不变量是：**仍要容纳一行内容**（它的价值就是"还能看清内容"，
    // 塌成几像素就失去意义）。判据同样按实测行高走，与字号设置保持同步。
    `[紧凑 ${id}] 条目高度 ≥ 一行内容（${MIN_FRAGILE_PREVIEW_HEIGHT}px）`,
    !r.error && r.item.h >= MIN_FRAGILE_PREVIEW_HEIGHT,
    `item.h=${r.item?.h}`
  );
  const want = BTN_MATRIX[id];
  check(
    `[紧凑 ${id}] 「编辑内容」按钮 ${want.bodyEdit ? "存在" : "不存在"}`,
    !r.error && r.buttons.bodyEdit === want.bodyEdit,
    `bodyEdit=${r.buttons?.bodyEdit}`
  );
  check(
    `[紧凑 ${id}] 「编辑备注」按钮 ${want.noteEdit ? "存在" : "不存在"}`,
    !r.error && r.buttons.noteEdit === want.noteEdit,
    `noteEdit=${r.buttons?.noteEdit}`
  );
}

await browser.close();
server.close();

const failed = findings.filter((f) => !f.ok);
const payload = {
  mode: SABOTAGE ? "sabotage" : "normal",
  viewport: VIEWPORT,
  summary: { total: findings.length, passed: findings.length - failed.length, failed: failed.length },
  failed,
  cards: cardResults,
  dialogs: dialogResults,
  compact: compactResults,
};

if (AS_JSON) {
  console.log(JSON.stringify(payload, null, 2));
} else {
  console.log(`\n模式：${payload.mode}   视口：${VIEWPORT.width}x${VIEWPORT.height}`);
  console.log("=".repeat(78));
  for (const [id, r] of Object.entries(cardResults)) {
    if (r.error) {
      console.log(`${id.padEnd(16)} ERROR ${r.error}`);
      continue;
    }
    console.log(
      `${id.padEnd(16)} item.h=${String(r.item.h).padStart(7)}  preview.h=${String(
        r.preview?.h ?? "-"
      ).padStart(7)}  text=${String(r.previewText).padStart(3)}  children=[${r.previewChildTags.join(
        ","
      )}]  编辑内容=${r.buttons.bodyEdit ? "有" : "无"}  编辑备注=${r.buttons.noteEdit ? "有" : "无"}`
    );
  }
  console.log("-".repeat(78));
  for (const [k, r] of Object.entries(dialogResults)) {
    console.log(
      `${k.padEnd(16)} dialog=${r.dialog?.h ?? "-"}  textarea=${r.textarea?.h ?? "-"}  text=${r.dialogText}`
    );
  }
  console.log("-".repeat(78));
  console.log(`断言 ${payload.summary.passed} / ${payload.summary.total} 通过`);
  if (failed.length) {
    console.log("\n未通过：");
    for (const f of failed) console.log(`  ✗ ${f.label}  →  ${f.detail}`);
  }
}

/**
 * 退出码语义（反向对照靠它工作）：
 *  - 正常模式：全部通过 → 0；有未通过 → 1
 *  - 反向对照：**必须**出现未通过 → 0（证明断言真的能变红）；全过 → 2（断言是空的）
 */
if (SABOTAGE) {
  if (failed.length === 0) {
    console.error("\n✗ 反向对照失败：注入「高度归零」后断言仍然全绿 ⇒ 这些断言抓不住坍缩。");
    process.exit(2);
  }
  console.log(`\n✓ 反向对照成立：注入「高度归零」后 ${failed.length} 条断言变红。`);
  process.exit(0);
}

process.exit(failed.length ? 1 : 0);
