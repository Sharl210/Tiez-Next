/**
 * `@tauri-apps/plugin-updater` 的最小替身。
 *
 * `SettingsFooter` 的更新确认框只在 `check()` 返回一个真值时渲染，而那个框正是
 * 用了 `var(--bg-secondary)` / `var(--border-color)` 的地方。没有这个替身就永远
 * 看不到它，也就无法验证修复。返回一个形状最小、字段齐全的 Update 对象，
 * 让真实组件的真实 inline style 渲染出来。
 */
const noop = async () => undefined;
export const check = async () => ({
  version: "0.5.1",
  date: "2026-09-24T00:00:00Z",
  body: "修复：未定义的 CSS 变量导致部分样式静默失效。",
  downloadAndInstall: noop,
  close: noop,
});
export class Update {}
export default { check, Update };
