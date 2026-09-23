import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Edit2, Trash2 } from "lucide-react";

/**
 * 标签组的右键菜单（重命名 / 删除）。
 *
 * # 为什么要有这个组件
 *
 * 标签组上的「重命名」「删除」图标原本是两个常驻在行内的 `<span>`（悬浮/选中时显形）。
 * 它们和标签名共处一行，于是每次想点某个标签组时，鼠标扫过行内就会让这两个图标冒出来
 * 抢位——用户点的是"选择这个标签"，落点却可能砸在删除图标上。把这两个动作收进右键菜单，
 * 行内就只剩下"标签色点 + 名称 + 条目数"，选择动作不再被遮挡。
 *
 * # 为什么 portal 到 body
 *
 * 标签列表 `.tag-scroll` 是滚动容器（祖先链上还有 `overflow: hidden` 的侧栏与
 * `clip-path` 的应用外壳）。菜单若留在原地渲染，一靠近列表底部就会被裁剪掉一半。
 * portal 到 `document.body` 后菜单是视口定位的浮层，不再受任何祖先的裁剪与层叠上下文影响。
 *
 * # 关闭时机
 *
 * 右键菜单的惯例是"点到别处就没了"：外部按下、Escape、滚动、窗口尺寸变化、
 * 在别处再点一次右键、窗口失焦——全部关闭。前两者之外还监听滚动，是因为锚点行会随
 * 列表滚走，菜单留在原处指向一个已经不在那里的标签组，比直接关掉更容易误操作。
 *
 * Escape 在**捕获阶段**截断：条目区与主页面自己也在监听键盘事件，不拦截的话
 * "关掉菜单"会顺带把整个剪贴板窗口一起关掉。
 */

export interface TagGroupContextMenuProps {
  /** 右键落点的视口坐标。 */
  x: number;
  y: number;
  /** 被右键的标签组名。 */
  tagName: string;
  /** 该组当前关联的条目数，用于删除确认文案。 */
  affectedCount: number;
  t: (key: string) => string;
  /** 进入行内重命名编辑态（沿用既有的 `editingTag` 逻辑）。 */
  onRename: () => void;
  /** 打开删除二次确认对话框（此处**不**直接删除）。 */
  onDelete: () => void;
  onClose: () => void;
}

export interface ContextMenuPositionInput {
  anchorX: number;
  anchorY: number;
  menuWidth: number;
  menuHeight: number;
  viewportWidth: number;
  viewportHeight: number;
  /** 与视口边缘保留的最小间距，默认 8px。 */
  margin?: number;
}

/**
 * 把菜单夹回视口内：先按落点摆放，越界时向左/向上回退。
 *
 * 单独抽成纯函数是为了能直接测——"菜单跑到屏幕外"这类缺陷在静态渲染里看不见，
 * 只有把几何算清楚了才能断言。
 */
export function resolveContextMenuPosition(
  input: ContextMenuPositionInput
): { left: number; top: number } {
  const margin = input.margin ?? 8;
  const maxLeft = Math.max(margin, input.viewportWidth - input.menuWidth - margin);
  const maxTop = Math.max(margin, input.viewportHeight - input.menuHeight - margin);
  return {
    left: Math.min(Math.max(input.anchorX, margin), maxLeft),
    top: Math.min(Math.max(input.anchorY, margin), maxTop),
  };
}

export const TagGroupContextMenu = ({
  x,
  y,
  tagName,
  affectedCount,
  t,
  onRename,
  onDelete,
  onClose,
}: TagGroupContextMenuProps) => {
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [position, setPosition] = useState<{ left: number; top: number } | null>(null);

  // useLayoutEffect：在浏览器绘制之前量好尺寸并落位，用户看不到"先出现在右下角再跳过去"。
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
    // 第一项拿到焦点，键盘用户不必先 Tab 一次；preventScroll 避免抢焦点时把列表滚走。
    el.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus({ preventScroll: true });
  }, [x, y]);

  const closeOnEscape = useCallback(
    (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // 捕获阶段截断：不拦的话这次 Escape 会继续传给主页面的快捷键处理。
      e.stopPropagation();
      e.preventDefault();
      onClose();
    },
    [onClose]
  );

  useEffect(() => {
    // 捕获阶段监听：祖先上的 stopPropagation 不会让菜单变成关不掉。
    const onPointerDown = (e: PointerEvent) => {
      if (e.target instanceof Node && menuRef.current?.contains(e.target)) return;
      onClose();
    };
    const onScrollOrResize = () => onClose();
    const onContextMenuElsewhere = (e: MouseEvent) => {
      if (e.target instanceof Node && menuRef.current?.contains(e.target)) return;
      onClose();
    };
    // 失焦（切到别的窗口）后菜单会浮在一个已经不活跃的界面之上，直接关掉。
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

  return createPortal(
    <div
      ref={menuRef}
      className="tag-group-menu"
      role="menu"
      aria-label={tagName}
      data-tag-group-menu={tagName}
      style={{
        left: position ? `${position.left}px` : `${x}px`,
        top: position ? `${position.top}px` : `${y}px`,
        // 量出尺寸之前先不可见，避免一帧的错位。
        visibility: position ? "visible" : "hidden",
      }}
      onContextMenu={(e) => {
        // 菜单自身的右键不应该把菜单关掉（关闭监听只认菜单外的目标），
        // 但必须阻止冒泡，否则会往上层再抛一次右键。
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
      <button
        type="button"
        className="tag-group-menu-item"
        role="menuitem"
        onClick={(e) => {
          e.stopPropagation();
          onClose();
          onRename();
        }}
      >
        <Edit2 size={13} />
        <span>{t("rename")}</span>
      </button>
      <button
        type="button"
        className="tag-group-menu-item danger"
        role="menuitem"
        onClick={(e) => {
          e.stopPropagation();
          // 先关菜单再开确认框：确认框自己有遮罩，两个浮层叠在一起没有必要。
          onClose();
          onDelete();
        }}
      >
        <Trash2 size={13} />
        <span>{t("delete")}</span>
        {/* 让用户在按下之前就知道这一组牵动多少条目；真正的拦截在确认框里。 */}
        {affectedCount > 0 && (
          <span className="tag-group-menu-count" aria-hidden>
            {affectedCount}
          </span>
        )}
      </button>
    </div>,
    document.body
  );
};

export default TagGroupContextMenu;
