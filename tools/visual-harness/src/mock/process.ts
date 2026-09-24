/** `@tauri-apps/plugin-process` 替身：本验证台不重启进程。 */
export const relaunch = async () => undefined;
export const exit = async () => undefined;
export default { relaunch, exit };
