/**
 * `@tauri-apps/api/window` 的最小替身。
 *
 * 只实现被测组件真正读到的形状（`label` 与**命名导出**的窗口能力），
 * 不伪造更多能力：组件在浏览器里跑时，那些窗口操作本来就没有对应实现，
 * 返回空而不抛错，让它继续渲染出真实的 DOM 与样式 —— 而这正是本验证台要量的对象。
 *
 * ⚠️ 这里有一个容易踩的坑：**rollup 对 ESM 的命名导入要求导出确实存在**，
 * 否则整个构建失败（不是运行时静默降级）。`ClipboardItem` 用到了
 * `currentMonitor` / `PhysicalPosition` / `PhysicalSize`，只导出
 * `getCurrentWindow` 会让 `collapse` 入口构建不出来。因此这三个也一并给出**空实现**：
 * 它们的返回值只被用于摆放浮窗（悬浮预览），量测台不量浮窗位置，所以空实现足够。
 */
const noop = async () => undefined;
export const getCurrentWindow = () => ({
  label: "main",
  listen: noop,
  onFocusChanged: noop,
  setFocus: noop,
  isFocused: async () => true,
  onCloseRequested: noop,
  onResized: noop,
  startDragging: noop,
  scaleFactor: async () => 1,
  outerPosition: async () => ({ x: 0, y: 0 }),
  outerSize: async () => ({ width: 352, height: 380 }),
  innerPosition: async () => ({ x: 0, y: 0 }),
  innerSize: async () => ({ width: 352, height: 380 }),
});

/** 悬浮预览的落点计算会读它；浏览器里没有多屏概念，返回空即可。 */
export const currentMonitor = async () => null;
export const availableMonitors = async () => [];
export const primaryMonitor = async () => null;

/** 与真机同名的两家「几何值对象」；量测台只要求它们能构造，不参与布局。 */
export class PhysicalPosition {
  constructor(x = 0, y = 0) {
    this.x = x;
    this.y = y;
  }
}
export class PhysicalSize {
  constructor(width = 0, height = 0) {
    this.width = width;
    this.height = height;
  }
}
export class LogicalPosition {
  constructor(x = 0, y = 0) {
    this.x = x;
    this.y = y;
  }
}
export class LogicalSize {
  constructor(width = 0, height = 0) {
    this.width = width;
    this.height = height;
  }
}

export const getCurrent = getCurrentWindow;
export default { getCurrentWindow, getCurrent, currentMonitor, availableMonitors, primaryMonitor };
