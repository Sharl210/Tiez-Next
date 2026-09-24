import type { MouseEvent, ReactNode } from "react";
import type { DragControls } from "framer-motion";
import type { ClipboardEntry, Locale } from "../../shared/types";

export interface QuickPasteHint {
  slot: number;
  combo: string;
}

export interface ClipboardItemProps {
  item: ClipboardEntry;
  isSelected: boolean;
  windowPinned: boolean;
  isSensitiveHidden: boolean;
  isRevealed: boolean;
  isEditingTags: boolean;
  tagInput: string;
  /** Tags used elsewhere in history; shown as quick-pick when editing tags */
  tagSuggestions?: string[];
  /**
   * v0.5 需求⑨: every known tag name, available **regardless of the tag editor's state**.
   *
   * `tagSuggestions` above is deliberately empty unless the inline tag editor is open
   * (the parent passes `EMPTY_TAG_SUGGESTIONS` otherwise, to keep the virtualised rows
   * cheap). The "move to tag / copy to tag" dialog can be opened on its own, so it needs a
   * full candidate list at all times — hence a separate prop instead of reusing that one
   * and silently showing an empty candidate list.
   */
  allTagNames?: string[];
  theme: string;
  language: Locale;
  t: (key: string) => string;
  isAIProcessing?: boolean;
  aiEnabled?: boolean;
  tagColors?: Record<string, string>;
  aiOptionsOpen?: boolean;
  richTextSnapshotPreview?: boolean;
  showSourceAppIcon?: boolean;
  sensitiveMaskPrefixVisible?: number;
  sensitiveMaskSuffixVisible?: number;
  sensitiveMaskEmailDomain?: boolean;
  quickPasteHint?: QuickPasteHint;

  onSelect: () => void;
  onCopy: (withFormat?: boolean) => void;
  onToggleReveal: (e: MouseEvent) => void;
  onOpen: (e: MouseEvent) => void;
  onTogglePin: (e: MouseEvent) => void;
  onDelete: (e: MouseEvent) => void;
  onToggleTagEditor: (e: MouseEvent) => void;
  onTagInput: (val: string) => void;
  onTagAdd: () => void;
  /** Pick an existing tag from the suggestion list (typically closes editor after add) */
  onTagPick?: (tag: string) => void;
  /** Close tag editor without adding (e.g. Escape) */
  onTagEditCancel?: () => void;
  onTagDelete: (tag: string) => void;
  onAIAction?: (type: string) => void;
  onAIOptionsToggle?: () => void;
  onInputSubmit?: (val: string) => void;
  /** R10: open the body editor for this entry. Omitted for types whose body is not text. */
  onEdit?: (e: MouseEvent) => void;
  /** R10: this entry's body editor is open. */
  isEditingBody?: boolean;
  /** R10: draft the editor starts from; only read when the dialog mounts. */
  bodyInitialDraft?: string;
  /**
   * R13: 富文本条目的 HTML 初值。只有 `rich_text` 会给，其它类型为空。
   *
   * 之前弹窗的初值只有 `item.content`（**纯文本列**），所以用户一打开富文本条目的
   * 编辑器，格式就已经没了 —— 这是"编辑富文本坍缩成纯文本"的前端侧那一半根因，
   * 与后端的降级彼此独立，只修一处都不够。
   */
  bodyInitialHtml?: string;
  /** R13: 这条正文是否以富文本（可编辑 HTML）方式编辑。 */
  bodyEditIsRich?: boolean;
  /** R10: a save is in flight (dialog disables its buttons). */
  bodyEditSaving?: boolean;
  /** R10: error text from the last failed save, shown inside the dialog. */
  bodyEditError?: string | null;
  /**
   * R10/R13: commit the edited body. The renderer hook owns the backend call.
   * 第二个参数是编辑后的 HTML，**只有** `rich_text` 条目会带上。
   */
  onBodyEditSave?: (newContent: string, htmlContent?: string) => void;
  /** R10: close the body editor without saving. */
  onBodyEditCancel?: () => void;
  /**
   * R11: open the note-only editor. Supplied for `image` / `file` / `video`, whose body
   * is a path or a `data:` URL and therefore cannot be edited as text. Text-like types
   * keep `onEdit` (body) and never get this, so a binary row can never show a body
   * field it is not allowed to write.
   */
  onEditNote?: (e: MouseEvent) => void;
  /** R11: this entry's note editor is open. */
  isEditingNote?: boolean;
  /** R11: note the editor starts from; only read when the dialog opens. */
  noteInitialDraft?: string;
  /** R11: a note save is in flight (dialog disables its buttons). */
  noteEditSaving?: boolean;
  /** R11: error text from the last failed note save, shown inside the dialog. */
  noteEditError?: string | null;
  /** R11: commit the edited note. The renderer hook owns the backend call. */
  onNoteEditSave?: (note: string) => void;
  /** R11: close the note editor without saving. */
  onNoteEditCancel?: () => void;
  dragControls?: DragControls;
  id?: string;
  disableLayout?: boolean;
}

/**
 * R6: safe accessor for the per-entry note.
 *
 * `ClipboardEntry.note` is optional (`src/shared/types/clipboard.ts`), because a payload
 * from a build that predates the column carries no `note` at all. This collapses the
 * optional field and any unexpected runtime shape into a plain string, so every call
 * site can treat "no note" and "empty note" identically.
 */
export const getEntryNote = (item: ClipboardEntry): string => {
  const note = item.note;
  return typeof note === "string" ? note : "";
};

/** R10: content types whose `content` column holds editable text. */
export const EDITABLE_BODY_TYPES: readonly string[] = ["text", "code", "url", "rich_text"];

/**
 * R11: content types whose `content` column holds a path or a `data:` URL instead of
 * editable text. Mirror of `is_binary_content_type` in
 * `src-tauri/src/infrastructure/repository/clipboard_repo.rs`; the back end uses the same
 * list to refuse body edits, and this list decides which rows get the note-only editor.
 */
export const BINARY_CONTENT_TYPES: readonly string[] = ["image", "file", "video"];

/**
 * R11: upper bound for a per-entry note, mirroring `MAX_ENTRY_NOTE_CHARS` in the
 * repository layer (the tag manager carries the same mirror). The back end trims and
 * clamps rather than rejecting, so this is a UI affordance, not the authority.
 */
export const MAX_ENTRY_NOTE_CHARS = 2000;

/**
 * R10: whether this entry's body can be edited as text.
 *
 * `image` / `file` / `video` store a filesystem path or a `data:` URL. Rewriting it
 * would leave `content_hash` pointing at the previous payload, so those rows get the
 * note editor only — the backend refuses body edits for them as well.
 */
export const isBodyEditable = (contentType: string): boolean =>
  EDITABLE_BODY_TYPES.includes(contentType);

/**
 * R11: whether this entry gets a note editor.
 *
 * # 判据：**永远为真** —— 备注与内容类型无关
 *
 * 这个函数被改过两次，两次都因为"拿正文的可编辑性去推备注的可编辑性"而漏掉一批条目：
 *
 * | 版本 | 判据 | 漏掉谁 |
 * |---|---|---|
 * | 最初 | `["image","file","video"].includes(t)` | `emoji_sync`、后端将来新增的任何类型 |
 * | v0.5.4 | `!EDITABLE_BODY_TYPES.includes(t)` | **`text` / `code` / `url` / `rich_text`** |
 *
 * 第二版看起来"反过来了就对了"，其实只是把漏掉的那批**换成了另一批** —— 凡是有正文编辑
 * 入口的类型，就**没有**备注入口。而用户的原话是「**编辑备注内容每个条目都要有这个按钮**」。
 *
 * 根子上错在：**备注是条目元数据**（`note` 列），与 `content` 列里放的是文本、路径还是
 * data URL **完全无关**。后端 `update_entry_note` 对所有行一视同仁。所以"正文能不能编辑"
 * 与"备注能不能编辑"是两件独立的事，不该用前者派生后者。
 *
 * 于是这里直接返回 `true`：**每个条目都能编辑备注**。保留成函数（而不是删掉判断）
 * 是为了让调用点保持一致的形状，也为将来万一真有"不允许备注"的类型留一个落点。
 *
 * ## 与 `isBodyEditable` 的关系
 *
 * 两者**独立**，不是互补：
 * - `isBodyEditable` —— 仍是白名单（正文编辑确实存在"未预见的类型会静默获得未经审查的
 *   写路径"的风险，那个担心成立）
 * - `isNoteEditable` —— 恒真（备注写路径唯一：`update_entry_note`，与类型无关）
 *
 * 所以 `text` 类条目**同时**有「编辑内容」和「编辑备注」两个入口，这是**正确**的，
 * 不是重复：两个按钮进的是两个不同的弹窗、改的是两个不同的字段。
 */
export const isNoteEditable = (_contentType: string): boolean => true;


/**
 * R13: 这条正文是否应以**富文本**（可编辑 HTML）方式打开编辑器。
 *
 * 只有 `rich_text` 为真。
 *
 * # 为什么其它文本类型不改成富文本编辑器
 *
 * `text` / `code` / `url` 的 `html_content` 要么为空、要么没有语义（代码的缩进与
 * 换行在 HTML 里要靠 `<pre>` 才能保住，往返一趟反而会改变用户看到的东西）。
 * 给它们一个 contentEditable 只会让"所见即所得"承诺落空 —— 用户改了间距、加了
 * 换行，存回去的 HTML 与 `content` 对不上。所以富文本编辑器**只**服务真正带格式的
 * 类型，其余仍是 `<textarea>`。
 *
 * 旧名字是 `bodyEditDowngradesFormat`（"这条保存后会降级"）—— 降级已经被修掉，
 * 那个函数随之删除；语言测试与界面里针对它的警告文案也一并移除。
 */
export const isRichBodyEditable = (contentType: string): boolean =>
  contentType === "rich_text";

export type ClipboardRenderItem = (
  item: ClipboardEntry,
  index: number,
  isFirst: boolean
) => ReactNode;

export interface VirtualClipboardListProps {
  items: ClipboardEntry[];
  renderItem: ClipboardRenderItem;
  onLoadMore?: () => void;
  hasMore: boolean;
  isLoading: boolean;
  selectedIndex: number;
  isKeyboardMode: boolean;
  onScroll?: (offset: number) => void;
  compactMode: boolean;
  header?: ReactNode;
}

export interface VirtualClipboardListHandle {
  scrollToItem: (index: number) => void;
  scrollToTop: () => void;
  resetAfterIndex: (index: number) => void;
}
