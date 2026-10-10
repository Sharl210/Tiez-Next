/**
 * 从富文本 HTML 派生出**纯文本正文**。
 *
 * # 链接为什么取 href 而不是锚文本
 *
 * 富文本里一条链接长这样：`<a href="http://host/c/T750/+/176116">T750 变更单</a>`，
 * 界面上显示为带下划线的「T750 变更单」。纯文本正文代表"这条内容以纯文本形式粘贴
 * 出去会长什么样"，此时用户要的是**链接本身**（可点、可复制、能直接打开），而不是
 * 屏幕上那层标签。
 *
 * # 但只有"整条内容就是一个链接"时才替换
 *
 * 文章正文里的行内链接必须原样保留人话：`<p>详见 <a href="…">变更单</a> 里的说明</p>`
 * 粘贴成纯文本应当是那句话，不能在句子中间插进一个网址。所以替换的前提是
 * **锚点就是全部可见内容**（并且只有一个锚点）。
 *
 * 判断方式是拿"整段可见文字"与"锚点文字"的归一化结果做比较，而不是数标签 ——
 * 后者会被 `<p>`、`<div>` 这类包裹层干扰。
 *
 * # 与后端的关系
 *
 * 后端 `derive_rich_text_content` 是权威值（写进 `content` 列、粘贴用的就是它）。
 * 本模块是同一口径的界面侧实现，只用于让"界面上显示的正文"与"最终落库的正文"一致。
 * **两边规则必须同步**，改一处就要改另一处 —— 后端实现与回归测试见
 * `src-tauri/src/services/clipboard/utils.rs` 的 `utils::tests`。
 */

/** `javascript:` / `data:` 这类不能当作"链接目标"回填到纯文本里的协议。 */
const NON_NAVIGABLE_HREF_RE = /^\s*(?:javascript|data|vbscript)\s*:/i;

/** 与后端 `collapse_preview_whitespace` 同口径：连续空白压成一个空格并去掉首尾。 */
const collapseWhitespace = (text: string): string => text.replace(/\s+/g, " ").trim();

/**
 * 取"可见文字"。块级元素之间补换行，避免两段文字粘成一行。
 *
 * 直接改传入的 doc：本模块内部先克隆再调用，调用方不受影响。
 */
const visibleTextOf = (doc: Document): string => {
  doc.querySelectorAll("br").forEach((br) => br.replaceWith("\n"));
  doc
    .querySelectorAll("p, div, li, tr, h1, h2, h3, h4, h5, h6, blockquote, pre")
    .forEach((el) => el.append("\n"));
  return (doc.body.textContent ?? "").replace(/\n{3,}/g, "\n\n").trim();
};

/** 可导航的 href，取不到或不可导航时返回 null。 */
const navigableHrefOf = (anchor: Element): string | null => {
  const href = (anchor.getAttribute("href") ?? "").trim();
  if (!href || NON_NAVIGABLE_HREF_RE.test(href)) return null;
  return href;
};

export const htmlToPlainText = (html: string): string => {
  if (!html) return "";

  const doc = new DOMParser().parseFromString(html, "text/html");

  // 先算出"没做任何替换时"的可见文字，用于判断锚点是不是全部内容。
  const visible = visibleTextOf(doc.cloneNode(true) as Document);

  const anchors = Array.from(doc.querySelectorAll("a[href]"));
  if (anchors.length === 1) {
    const href = navigableHrefOf(anchors[0]);
    if (href) {
      const label = collapseWhitespace(anchors[0].textContent ?? "");
      // 锚点必须就是全部可见内容，且标签与地址不同（否则默认路径已给出同样结果）。
      if (label && collapseWhitespace(visible) === label && label !== href) {
        return href;
      }
    }
  }

  // 没有链接、或链接不满足上面那条：原样返回未替换的可见文字。
  return visible;
};
