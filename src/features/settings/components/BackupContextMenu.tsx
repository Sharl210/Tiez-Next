import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Pin, PinOff, RotateCcw, Trash2 } from "lucide-react";
import { resolveContextMenuPosition } from "../../tag/components/TagGroupContextMenu";

/**
 * 自动备份列表里，某一条备份的右键菜单（固定 / 删除 / 恢复）。
 *
 * # 为什么沿用标签组右键菜单的做法
 *
 * 本项目里已经有一套成型的右键菜单交互（`TagGroupContextMenu`）：portal 到
 * `document.body` 以躲开祖先的裁剪与层叠上下文、`useLayoutEffect` 里先量尺寸再落位、
 * 越界时用 `resolveContextMenuPosition` 夹回视口、外部 pointerdown / Escape / 滚动 /
 * 尺寸变化 / 窗口失焦全部关闭、`role="menu"` + 方向键移动焦点。
 *
 * 这些行为用户已经熟悉，重新发明一套只会让同一个应用里出现两种右键手感。因此这里
 * **复用同一套类名（`tag-group-menu*`）与同一个定位函数**，只是菜单项不同。类名带
 * "tag-group" 字样是历史命名，但它描述的是"这种浮层菜单长什么样"，与数据来源无关；
 * 改类名会牵动标签组的既有测试与样式，收益为零。
 *
 * # 为什么菜单状态由弹窗用一条 `{kind, entry}` 表示，而不是三个布尔量
 *
 * 右键一次只可能选中**一条**备份，三个布尔量（固定框、删除框、恢复框）很容易出现
 * "两个同时为真"的非法状态。一个可区分的联合类型让非法状态无法表达。
 */

export type BackupMenuKind = "pin" | "unpin" | "delete" | "restore";

export interface BackupContextMenuProps {
  /** 右键落点的视口坐标。 */
  x: number;
  y: number;
  /** 被右键的备份文件名（用于 `aria-label` 与 `data-*`，便于测试定位）。 */
  archiveName: string;
  /** 这一份当前是否已固定：决定第一项是「固定」还是「取消固定」。 */
  pinned: boolean;
  t: (key: string) => string;
  onSelect: (kind: BackupMenuKind) => void;
  onClose: () => void;
}

export const BackupContextMenu = ({
  x,
  y,
  archiveName,
  pinned,
  t,
  onSelect,
  onClose,
}: BackupContextMenuProps) => {
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [position, setPosition] = useState<{ left: number; top: number } | null>(null);

  // 与标签组菜单一致：绘制之前量好尺寸并落位，用户看不到"先出现在右下角再跳过去"。
  useLayoutEffect(() => {
    const el = menuRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    setPosition(
      resolveContextMenuPosition({
        anchorX: x,
        anchorY: y,
        menuWidth: rect.width,
        menuHeight: rect.height,
        viewportWidth: window.innerWidth,
        viewportHeight: window.innerHeight,
      })
    );
    el.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus({ preventScroll: true });
  }, [x, y]);

  const closeOnEscape = useCallback(
    (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // 捕获阶段截断：不拦的话这次 Escape 会继续传给主窗口的快捷键处理。
      e.stopPropagation();
      e.preventDefault();
      onClose();
    },
    [onClose]
  );

  useEffect(() => {
    const onPointerDown = (e: PointerEvent) => {
      if (e.target instanceof Node && menuRef.current?.contains(e.target)) return;
      onClose();
    };
    const onScrollOrResize = () => onClose();
    const onContextMenuElsewhere = (e: MouseEvent) => {
      if (e.target instanceof Node && menuRef.current?.contains(e.target)) return;
      onClose();
    };
    const onWindowBlur = () => onClose();

    window.addEventListener("pointerdown", onPointerDown, true);
    window.addEventListener("keydown", closeOnEscape, true);
    window.addEventListener("scroll", onScrollOrResize, true);
    window.addEventListener("resize", onScrollOrResize, true);
    window.addEventListener("contextmenu", onContextMenuElsewhere, true);
    window.addEventListener("blur", onWindowBlur);
    return () => {
      window.removeEventListener("pointerdown", onPointerDown, true);
      window.removeEventListener("keydown", closeOnEscape, true);
      window.removeEventListener("scroll", onScrollOrResize, true);
      window.removeEventListener("resize", onScrollOrResize, true);
      window.removeEventListener("contextmenu", onContextMenuElsewhere, true);
      window.removeEventListener("blur", onWindowBlur);
    };
  }, [closeOnEscape, onClose]);

  const moveFocus = (delta: number) => {
    const items = Array.from(
      menuRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? []
    );
    if (items.length === 0) return;
    const current = items.findIndex((el) => el === document.activeElement);
    const next = (current + delta + items.length) % items.length;
    items[next]?.focus({ preventScroll: true });
  };

  /** 每项都先关菜单再执行：确认框自己有遮罩，两个浮层叠在一起没有必要。 */
  const pick = (kind: BackupMenuKind) => (e: React.MouseEvent) => {
    e.stopPropagation();
    onClose();
    onSelect(kind);
  };

  return createPortal(
    <div
      ref={menuRef}
      className="tag-group-menu"
      role="menu"
      aria-label={archiveName}
      data-backup-menu={archiveName}
      style={{
        left: position ? `${position.left}px` : `${x}px`,
        top: position ? `${position.top}px` : `${y}px`,
        visibility: position ? "visible" : "hidden",
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
      }}
      onKeyDown={(e) => {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          moveFocus(1);
        } else if (e.key === "ArrowUp") {
          e.preventDefault();
          moveFocus(-1);
        }
      }}
    >
      {/* 固定 / 取消固定：同一位置的开关式动作，文案随当前状态切换。 */}
      <button
        type="button"
        className="tag-group-menu-item"
        role="menuitem"
        data-backup-menu-item={pinned ? "unpin" : "pin"}
        onClick={pick(pinned ? "unpin" : "pin")}
      >
        {pinned ? <PinOff size={13} /> : <Pin size={13} />}
        <span>{pinned ? t("auto_backup_unpin") : t("auto_backup_pin")}</span>
      </button>

      {/* 恢复：破坏性（会替换当前数据），确认框里会写明"导入前会自动给你现在的数据
          再做一份旁路备份"，让用户知道可以退回去。 */}
      <button
        type="button"
        className="tag-group-menu-item"
        role="menuitem"
        data-backup-menu-item="restore"
        onClick={pick("restore")}
      >
        <RotateCcw size={13} />
        <span>{t("auto_backup_restore")}</span>
      </button>

      <button
        type="button"
        className="tag-group-menu-item danger"
        role="menuitem"
        data-backup-menu-item="delete"
        onClick={pick("delete")}
      >
        <Trash2 size={13} />
        <span>{t("delete")}</span>
      </button>
    </div>,
    document.body
  );
};

export default BackupContextMenu;
