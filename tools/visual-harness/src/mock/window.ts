/**
 * `@tauri-apps/api/window` 的最小替身。
 *
 * 只实现被测组件真正读到的形状（`label` 与非 DOM 的窗口能力），不伪造更多能力：
 * 组件在浏览器里跑时，那些窗口操作本来就没有对应实现，返回空而不抛错，
 * 让它继续渲染出真实的 DOM 与样式——而这正是本验证台要量的对象。
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
});
export const getCurrent = getCurrentWindow;
export default { getCurrentWindow, getCurrent };
