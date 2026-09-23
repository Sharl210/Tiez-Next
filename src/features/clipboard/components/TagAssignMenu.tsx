import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { ArrowDown, Check, FolderInput, FolderOutput, Plus, X } from "lucide-react";
import { getTagColor, getTagTextColor } from "../../../shared/lib/utils";

/**
 * 条目的「移动到标签 / 复制到标签」对话框。
 *
 * # 为什么单独一个组件文件
 *
 * `ClipboardItem.tsx` 已接近 2500 行，且本功能的入口只是它工具栏上的一个图标按钮；
 * 把整块交互（模式切换、源/目标选择、新标签输入、失败重试）放进独立文件，改动面
 * 只有"一个按钮 + 一段 portal 渲染"，不会与并行修改同一文件的其他改动冲突。
 *
 * # 为什么不用右键菜单
 *
 * 用户明确要求过"标签组操作改右键菜单"（那是为了不遮挡标签选择），而条目的右键
 * 在本项目里已经有含义（右键 = 复制为带格式文本）。于是这里的入口是一个工具栏图标
 * 按钮，对话框本身则沿用条目正文编辑器/备注编辑器的 portal + modal 形态，
 * 视觉语言与相邻元素一致。
 *
 * # 与后端的约定
 *
 * 组件**不做任何标签集合运算**：只把 `fromTag` / `toTag` 和模式发给
 * `move_entry_to_tag` / `copy_entry_to_tag`，由后端共享内核算出结果并落库。
 * 界面上的标签条带由 `clipboard-changed` 事件触发的历史刷新带回。这样"界面算一遍、
 * 后端再算一遍"的分叉就不可能发生。
 */

export interface TagAssignMenuProps {
  /** 正在整理的条目 id。 */
  entryId: number;
  /** 该条目当前的标签（作为"源标签"候选）。 */
  tags: string[];
  /** 全库已知标签名（作为"目标标签"候选）。 */
  allTags: string[];
  tagColors?: Record<string, string>;
  theme: string;
  t: (key: string) => string;
  onClose: () => void;
}

type Mode = "move" | "copy";

/** 一次转移的失败信息。保留在对话框里，让用户可以改完再试，而不是丢掉已做的选择。 */
interface TransferError {
  /** 后端原始错误，排查用。 */
  detail: string;
}

const MAX_TAG_LENGTH = 40;

export const TagAssignMenu = ({
  entryId,
  tags,
  allTags,
  tagColors,
  theme,
  t,
  onClose,
}: TagAssignMenuProps) => {
  const [mode, setMode] = useState<Mode>("move");
  const [fromTag, setFromTag] = useState<string | null>(null);
  const [targetQuery, setTargetQuery] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<TransferError | null>(null);
  const [done, setDone] = useState(false);
  const queryRef = useRef<HTMLInputElement | null>(null);

  const sourceTags = tags || [];

  // 打开时就把焦点放到目标输入框：用户的下一步动作几乎总是"输入或挑选目标标签"。
  useEffect(() => {
    const id = window.setTimeout(() => queryRef.current?.focus(), 0);
    return () => window.clearTimeout(id);
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // 捕获阶段拦下 Escape：条目本身也在监听键盘快捷键，不拦会把"关闭弹窗"
      // 变成"关闭整个剪贴板窗口"。
      e.stopPropagation();
      e.preventDefault();
      if (!saving) onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose, saving]);

  const trimmedQuery = targetQuery.trim();

  /**
   * 目标候选：全库标签去掉"当前条目已有"的（移动/复制到一个已存在的标签没有意义，
   * 移动模式下源标签也不能当目标——那就是原地不动）。
   */
  const targetCandidates = useMemo(() => {
    const lower = (v: string) => v.toLowerCase();
    const excluded = new Set(sourceTags.map(lower));
    if (fromTag) excluded.add(lower(fromTag));
    const q = trimmedQuery.toLowerCase();
    return allTags
      .filter((name) => !excluded.has(lower(name)))
      .filter((name) => !q || lower(name).includes(q))
      .slice(0, 40);
  }, [allTags, sourceTags, fromTag, trimmedQuery]);

  /** 输入框里的文字是否可以作为"新建标签"提交（不与已有候选重复）。 */
  const canCreateNew =
    trimmedQuery.length > 0 &&
    trimmedQuery.length <= MAX_TAG_LENGTH &&
    !allTags.some((name) => name.toLowerCase() === trimmedQuery.toLowerCase());

  const effectiveFrom = fromTag ?? null;

  const submit = async (toTag: string) => {
    const target = toTag.trim();
    if (!target) {
      setError({ detail: t("tag_transfer_need_target") || "请先选择或输入目标标签" });
      return;
    }
    if (mode === "move" && !effectiveFrom) {
      setError({ detail: t("tag_transfer_need_source") || "请先选择要移出的源标签" });
      return;
    }
    setSaving(true);
    setError(null);
    try {
      // 移动需要源标签；复制在界面上不要求用户先点源标签——"复制到 B"的结果与源无关
      // （结果都是"原有标签 + B"），因此源缺省时用当前第一个标签补位，纯为满足后端
      // 参数形状；后端与 MCP 侧的语义都只看 toTag。
      const from = effectiveFrom ?? sourceTags[0] ?? target;
      await invoke<number>(mode === "move" ? "move_entry_to_tag" : "copy_entry_to_tag", {
        id: entryId,
        fromTag: from,
        toTag: target,
      });
      // 后端会发 `clipboard-changed`，列表由既有刷新链路更新；这里只负责给出反馈。
      setDone(true);
      window.setTimeout(onClose, 550);
    } catch (err) {
      setError({ detail: err?.toString() || String(err) });
    } finally {
      setSaving(false);
    }
  };

  const renderChip = (
    name: string,
    opts: { active?: boolean; onClick?: () => void; key?: string; dashed?: boolean }
  ) => {
    const background = tagColors?.[name] || getTagColor(name, theme);
    const textColor = getTagTextColor(background);
    return (
      <button
        key={opts.key ?? name}
        type="button"
        className={`tag-assign-chip${opts.active ? " active" : ""}${opts.dashed ? " create" : ""}`}
        onClick={opts.onClick}
        disabled={saving}
        style={
          opts.dashed
            ? undefined
            : {
                background: opts.active ? background : undefined,
                color: opts.active ? textColor : undefined,
                borderColor: background,
              }
        }
      >
        {opts.dashed && <Plus size={10} />}
        <span className="tag-assign-chip-label">{name}</span>
      </button>
    );
  };

  return createPortal(
    <div
      className={`modal-overlay theme-${theme}`}
      onClick={() => !saving && onClose()}
      onMouseDown={(e) => e.stopPropagation()}
      onContextMenu={(e) => {
        // 条目右键 = 复制为带格式文本；在弹窗里必须截断，否则一次右键会顺手改剪贴板。
        e.preventDefault();
        e.stopPropagation();
      }}
    >
      <div
        className="confirm-dialog tag-assign-dialog"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-label={t("tag_transfer_title") || "整理标签"}
        style={{ maxWidth: "440px", width: "100%" }}
      >
        <div className="tag-assign-header">
          <h3 style={{ margin: 0, fontSize: "15px", fontWeight: 600 }}>
            {t("tag_transfer_title") || "整理标签"}
          </h3>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            disabled={saving}
            title={t("cancel")}
          >
            <X size={12} />
          </button>
        </div>

        <div className="tag-assign-modes" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "move"}
            className={`tag-assign-mode${mode === "move" ? " active" : ""}`}
            onClick={() => {
              setMode("move");
              setError(null);
            }}
            disabled={saving}
          >
            <FolderInput size={13} />
            {t("tag_transfer_move") || "移动到标签"}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "copy"}
            className={`tag-assign-mode${mode === "copy" ? " active" : ""}`}
            onClick={() => {
              setMode("copy");
              setError(null);
            }}
            disabled={saving}
          >
            <FolderOutput size={13} />
            {t("tag_transfer_copy") || "复制到标签"}
          </button>
        </div>

        <p className="tag-assign-hint">
          {mode === "move"
            ? t("tag_transfer_move_hint") ||
              "只替换所选源标签，条目上的其他标签保持不变。"
            : t("tag_transfer_copy_hint") ||
              "保留条目现有的全部标签，额外加入目标标签。"}
        </p>

        <div className="tag-assign-section">
          <div className="tag-assign-section-title">
            {t("tag_transfer_source") || "源标签"}
          </div>
          {sourceTags.length === 0 ? (
            <div className="tag-assign-empty">
              {t("tag_transfer_no_source") || "该条目还没有标签，可直接在下方选择一个目标标签。"}
            </div>
          ) : (
            <div className="tag-assign-chips">
              {sourceTags.map((name) =>
                renderChip(name, {
                  active: effectiveFrom === name,
                  onClick: () => {
                    setFromTag(effectiveFrom === name ? null : name);
                    setError(null);
                  },
                })
              )}
            </div>
          )}
        </div>

        {/* 区块是竖向排布（源在上、目标在下），箭头因此朝下——横向箭头会与布局方向矛盾。 */}
        <div className="tag-assign-arrow" aria-hidden>
          <ArrowDown size={14} />
        </div>

        <div className="tag-assign-section">
          <div className="tag-assign-section-title">
            {t("tag_transfer_target") || "目标标签"}
          </div>
          <input
            ref={queryRef}
            type="text"
            className="tag-assign-input"
            value={targetQuery}
            maxLength={MAX_TAG_LENGTH}
            placeholder={t("tag_transfer_target_placeholder") || "选择已有标签，或输入新标签名"}
            onChange={(e) => {
              setTargetQuery(e.target.value);
              setError(null);
            }}
            onKeyDown={(e) => {
              e.stopPropagation();
              if (e.key === "Enter" && trimmedQuery) {
                e.preventDefault();
                void submit(trimmedQuery);
              }
            }}
            disabled={saving}
          />
          <div className="tag-assign-chips">
            {targetCandidates.map((name) =>
              renderChip(name, { onClick: () => void submit(name) })
            )}
            {canCreateNew &&
              renderChip(trimmedQuery, {
                key: "__create__",
                dashed: true,
                onClick: () => void submit(trimmedQuery),
              })}
            {!canCreateNew && targetCandidates.length === 0 && !trimmedQuery && (
              <div className="tag-assign-empty">
                {t("tag_transfer_no_candidates") || "没有其他可用标签，直接输入一个新建。"}
              </div>
            )}
          </div>
        </div>

        {error && <div className="tag-assign-error">{error.detail}</div>}

        <div className="confirm-dialog-buttons">
          <button type="button" className="confirm-dialog-button" onClick={onClose} disabled={saving}>
            {t("cancel")}
          </button>
          <button
            type="button"
            className="confirm-dialog-button primary"
            disabled={saving || !trimmedQuery.trim()}
            onClick={() => void submit(trimmedQuery)}
          >
            {done ? <Check size={13} /> : saving ? "..." : mode === "move" ? t("tag_transfer_move") || "移动" : t("tag_transfer_copy") || "复制"}
          </button>
        </div>
      </div>
    </div>,
    document.body
  );
};

export default TagAssignMenu;
