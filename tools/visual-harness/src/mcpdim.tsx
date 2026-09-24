import React from "react";
import ReactDOM from "react-dom/client";

// ---- 真实样式，与 src/main.tsx 的加载顺序一致 ----
import "../../../src/index.css";
import "../../../src/styles/components/index.css";
import "../../../src/styles/themes/load";

import McpSettingsGroup from "../../../src/features/settings/components/groups/McpSettingsGroup";
import { translations } from "../../../src/locales";

/**
 * 「MCP 压暗」量测台。
 *
 * # 为什么量这块
 *
 * 用户（retro 主题）反馈「mcp 那个压暗还是有，但是好很多了」—— 上一轮修了两处
 * （弹窗背景改不透明、遮罩从 0.56 降到 0.42）之后仍有残留。
 *
 * # 为什么必须挂真实组件 + 量计算样式
 *
 * 「压暗」是**多机制叠加**的结果，看代码看不出来，只有把真实组件渲染出来、
 * 逐元素读 `getComputedStyle` 再**从截图取色**才能定位到底是哪一层在变暗：
 *
 *   - 半透明 `background`（透出下层更暗的东西）
 *   - `box-shadow` 的 `inset`（在元素**内部**铺一层暗色方块）
 *   - 父级 `opacity`（整棵子树被调淡）
 *   - `filter` / `backdrop-filter`
 *
 * 本仓库 retro 主题把 `--data-panel-shadow` 覆盖成
 * `inset 3px 3px 0 rgba(0,0,0,0.05)`（其余五个主题都是 `none`），
 * 而 MCP 分组里有两个 `.data-panel`。这就是要量清楚的对象。
 */

const lang = "zh" as const;
const t = (key: string): string => {
  const dict = translations[lang] as unknown as Record<string, string>;
  return dict[key] ?? key;
};

const theme = new URLSearchParams(location.search).get("theme") ?? "retro";
const colorMode = new URLSearchParams(location.search).get("colorMode") ?? "light";

document.documentElement.classList.add(`theme-${theme}`, `${colorMode}-mode`);
document.body.classList.add(`theme-${theme}`, `${colorMode}-mode`);

const noop = () => {};

ReactDOM.createRoot(document.getElementById("root")!).render(
  <div className="settings-page" style={{ padding: 12 }}>
    <McpSettingsGroup t={t} collapsed={false} onToggle={noop} />
  </div>
);
