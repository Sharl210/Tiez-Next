/**
 * 设置页分组的**唯一清单**。
 *
 * ## 为什么要有这个文件
 *
 * 设置页的「哪些分组是展开的」由 `collapsedGroups` 这个 `Record<string, boolean>` 表达，
 * 但它的键在**三处**各自维护：`useAppState` 的初值、`useSettingsPanelReset` 的重置字典、
 * 以及 `SettingsPanel` 实际消费的分组。三处一旦不同步就会出问题。
 *
 * 这不是假想 —— 真机上已经出过一次：`useSettingsPanelReset` 的重置字典停留在 9 个键，
 * 而 `useAppState` 初值有 12 个。新增的 `mcp` 与 `auto_backup` 只加进了初值，**没加进重置字典**。
 *
 * 后果很具体：重置时用的是 `setCollapsedGroups({...})`（**整体替换**），替换后
 * `collapsedGroups["mcp"]` 变成 `undefined`；而 `SettingsPanel` 的判定是
 * `collapsed ? "collapsed" : ""`，`undefined` 是 falsy → **判定为「展开」**。
 *
 * 于是用户看到的现象是：**首次**打开设置页一切正常（12 个键的初值全是 `true`，全部收起），
 * 但只要**开关过设置页**触发那次重置，MCP 与「自动容灾备份」就变成展开的，
 * 而其他分组仍是收起的 —— 「默认状态和其他选项不一致」。
 *
 * 两次都漏（`ff1a481` 加 `mcp`、`0b2e389` 加 `auto_backup`），说明靠人工同步不可靠。
 * 因此把清单收敛到这一处：**新增分组时只改这里**，初值与重置都从它派生。
 *
 * ## 与 `SettingsPanel` 消费的分组的关系
 *
 * 本清单包含 `advanced`，但 `advanced` 在 `SettingsPanel` 里**不是折叠分组**
 * （它是「高级设置」入口，用 `settings-nav-card`，不读 `collapsedGroups`）。
 * 保留它是因为 `useAppState` 的初值里有它 —— 删掉属于另一件事，会改变
 * `collapsedGroups` 的类型形状。这里只保证**三处一致**，不借机做别的清理。
 */

/** 分组键的规范顺序（也是「全部收起」时的遍历顺序）。 */
export const SETTINGS_GROUP_KEYS = [
  "general",
  "clipboard",
  "advanced",
  "appearance",
  "sync",
  "cloud_sync",
  "ai",
  "file_transfer",
  "default_apps",
  "data",
  "auto_backup",
  "mcp"
] as const;

export type SettingsGroupKey = (typeof SETTINGS_GROUP_KEYS)[number];

/**
 * 全部分组都收起的字典。
 *
 * **每次调用都返回新对象** —— 调用方（React state）依赖引用变化触发重渲染；
 * 若返回同一个冻结对象，`setCollapsedGroups(sameRef)` 会被 React 判定为无变化而跳过更新。
 */
export const createAllCollapsedGroups = (): Record<string, boolean> =>
  Object.fromEntries(SETTINGS_GROUP_KEYS.map((key) => [key, true]));
