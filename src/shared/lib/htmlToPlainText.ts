/**
 * 从富文本 HTML 派生出**纯文本正文**。
 *
 * # 核心规则：所有超链接都换成网址本身
 *
 * 富文本里一条链接长这样：`<a href="http://host/c/T750/+/176116">T750 变更单</a>`，
 * 界面上显示为带下划线的「T750 变更单」。纯文本正文代表"这条内容以纯文本形式粘贴
 * 或转换出去长什么样"，此时链接的**真实目标**是 `href` —— 用户要能直接看到、复制、
 * 打开那个地址，而不是屏幕上的那层装饰文字。
 *
 * 所以只要有超链接，就换成网址本身，**不论它在正文的什么位置、是不是唯一一个**。
 *
 * # 不改写的情况
 *
 * - 没有 href、href 为空；
 * - `javascript:` / `data:` / `vbscript:` 这类不是可导航目标；
 * - 标签文字与地址本来就完全相同（改写等于没改）。
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

/** 可导航的 href，取不到或不可导航时返回 null。 */
const navigableHrefOf = (anchor: Element): string | null => {
  const href = (anchor.getAttribute("href") ?? "").trim();
  if (!href || NON_NAVIGABLE_HREF_RE.test(href)) return null;
  return href;
};

export const htmlToPlainText = (html: string): string => {
  if (!html) return "";

  const doc = new DOMParser().parseFromString(html, "text/html");

  // 每个超链接换成地址本身。锚点内部的格式标签（<a href="X"><b>粗</b></a>）不用
  // 单独处理：整个锚点被替换成地址后，原本嵌在里面的标签自然消失。
  doc.querySelectorAll("a[href]").forEach((anchor) => {
    const href = navigableHrefOf(anchor);
    if (!href) return; // 保留原文
    const label = (anchor.textContent ?? "").trim();
    if (label === href) return; // 标签本来就是地址，改写等于没改
    anchor.replaceWith(doc.createTextNode(href));
  });

  // 块级元素之间补换行，否则两段文字会粘成一行。
  //
  // 换行口径必须与后端 `extract_plain_text_from_htmlish` 一致，否则同一条目
  // "点按钮转换"与"直接纯文本粘贴"会得到不同的行结构 —— 例如两段 `<p>` 在前端被
  // 压成一行、表格两格粘成 "项目值"。后端是权威（粘贴走它），这里对齐它。
  doc.querySelectorAll("br").forEach((br) => br.replaceWith("\n"));

  // 块级元素的**前后各补一个换行**，而不是只在末尾补。
  //
  // 后端是把块标签的**开标签和闭标签都**换成换行（`</p><p>` 于是产生两个换行），
  // 因此两个相邻段落之间是空行、表格两格之间也是空行。只在末尾补一个换行的话，
  // 前端会给出 "第一段\n第二段" 而后端给 "第一段\n\n第二段" —— 同一条目
  // "点按钮转换"与"直接纯文本粘贴"的行结构就不一样了。后端是粘贴那条路的权威，
  // 这里对齐它。
  doc
    .querySelectorAll("p, div, li, tr, td, th, table, h1, h2, h3, h4, h5, h6, section, article, ul, ol, blockquote, pre")
    .forEach((el) => {
      el.before("\n");
      el.after("\n");
    });

  return (doc.body.textContent ?? "").replace(/\n{3,}/g, "\n\n").trim();
};
