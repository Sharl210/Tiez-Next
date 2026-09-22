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
  /** R10: a save is in flight (dialog disables its buttons). */
  bodyEditSaving?: boolean;
  /** R10: error text from the last failed save, shown inside the dialog. */
  bodyEditError?: string | null;
  /** R10: commit the edited body. The renderer hook owns the backend call. */
  onBodyEditSave?: (newContent: string) => void;
  /** R10: close the body editor without saving. */
  onBodyEditCancel?: () => void;
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
 * R10: whether this entry's body can be edited as text.
 *
 * `image` / `file` / `video` store a filesystem path or a `data:` URL. Rewriting it
 * would leave `content_hash` pointing at the previous payload, so those rows get the
 * note editor only — the backend refuses body edits for them as well.
 */
export const isBodyEditable = (contentType: string): boolean =>
  EDITABLE_BODY_TYPES.includes(contentType);

/**
 * R10: `rich_text` is editable, but saving downgrades it to plain text and drops
 * `html_content` (backend behaviour in `update_entry_content_with_conn`). The editor
 * warns about that before the user commits.
 */
export const bodyEditDowngradesFormat = (contentType: string): boolean =>
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
