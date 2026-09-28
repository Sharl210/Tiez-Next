export type ClipboardTypeLabel = "富文本" | "纯文本" | `${string}文件`;

/** 返回条目应显示在来源旁的类型气泡文字。 */
export const getClipboardTypeLabel = (contentType: string, content: string): ClipboardTypeLabel => {
  if (contentType === "rich_text") return "富文本";
  if (["text", "code", "url"].includes(contentType)) return "纯文本";
  if (["image", "file", "video"].includes(contentType)) {
    const firstPath = content.split(/\r?\n/).map((value) => value.trim()).find(Boolean) || "";
    const leaf = firstPath.split(/[\\/]/).pop() || "";
    const match = leaf.match(/(\.[A-Za-z0-9]{1,12})$/);
    return match ? `${match[1].toLowerCase()}文件` : "文件";
  }
  return "纯文本";
};
