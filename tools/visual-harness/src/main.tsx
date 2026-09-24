import React from "react";
import ReactDOM from "react-dom/client";
import { HelpCircle } from "lucide-react";

// ---- 真实样式，与 src/main.tsx 的加载顺序一字不差 ----
// src/main.tsx 的顺序是：index.css → styles/components/index.css → styles/themes/load。
// 【本台曾经漏了第三行】漏掉它时页面只剩 `base.css` 的默认变量：`theme-mica` /
// `theme-paper` / `dark-mode` 这些类挂到根元素上也不产生任何效果。于是 `?theme=xxx`
// 参数看着在生效，实际六套主题量到的全是同一组 base 值（实测：`--bg-button` 在
// mica / paper / acrylic / sakura / retro / sticky-note 下都返回 base 的
// `rgba(255, 255, 255, .76)`）。"跨主题一致性"这类缺陷正是靠多主题遍历发现的，
// 主题样式没加载 = 遍历无效，量出来的"一致"是假的。因此这行不能省。
import "../../../src/index.css";
import "../../../src/styles/components/index.css";
import "../../../src/styles/themes/load";
// 自定义右键菜单的样式是「按需 import」的（TagManager.tsx 里那行），
// 备份列表的菜单复用同一套类名，因此这里也必须把它加载进来。
import "../../../src/styles/components/tag-group-menu.css";

// ---- 真实组件（一字未改）----
import AutoBackupSettingsGroup from "../../../src/features/settings/components/groups/AutoBackupSettingsGroup";
import DataSettingsGroup from "../../../src/features/settings/components/groups/DataSettingsGroup";
import BackupListModal from "../../../src/features/settings/components/BackupListModal";

// ---- 真实文案（不做任何子集裁剪）----
import { translations } from "../../../src/locales";

/**
 * 组件级视觉验证台。
 *
 * 只替换宿主 API（见 vite.config.mjs 的 alias），组件代码一行不改；文案直接读
 * 真实的 `src/locales.ts`（不裁剪子集），这样"key 缺失"或"占位符没被替换"会
 * 以原样出现在截图里，而不是被桩掩盖。
 *
 * 视口宽度按 tauri.conf.json 的真实主窗口 352px 设置（minWidth 250）。
 */

const lang = (new URLSearchParams(location.search).get("lang") ?? "zh") as "zh" | "en" | "tw";
const mode = new URLSearchParams(location.search).get("mode") ?? "groups";
const theme = new URLSearchParams(location.search).get("theme") ?? "mica";
const colorMode = new URLSearchParams(location.search).get("colorMode") ?? "light";

const t = (key: string): string => {
  const dict = translations[lang] as unknown as Record<string, string>;
  return dict[key] ?? (translations.zh as unknown as Record<string, string>)[key] ?? key;
};

/** 与 SettingsPanel 里同名的那个组件逐字一致（含 hint 图标开关）。 */
const LabelWithHint = ({
  label,
  hint,
}: {
  label: string;
  hint?: string | React.ReactNode;
  hintKey: string;
}) => (
  <div className="item-label-group">
    <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
      <span className="item-label">{label}</span>
      {hint && (
        <button
          type="button"
          className="hint-icon-btn"
          title={typeof hint === "string" ? hint : undefined}
        >
          <HelpCircle size={12} />
        </button>
      )}
    </div>
  </div>
);

const Groups = () => (
  <>
    {/* 相邻分组：用真实组件做基线对照。折叠态设为展开（collapsed=false）。 */}
    <DataSettingsGroup t={t} collapsed={false} onToggle={() => {}} dataPath="/home/u/.local/share/com.tiez.next" />
    <AutoBackupSettingsGroup
      t={t}
      collapsed={false}
      onToggle={() => {}}
      LabelWithHint={LabelWithHint}
      theme={theme}
    />
  </>
);

/** 悬浮窗 + 可选地把右键菜单/二次确认框打开，便于一次截图看全。 */
const ModalStage = () => {
  const [open, setOpen] = React.useState(true);
  React.useEffect(() => {
    const driver = async () => {
      const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
      await wait(300);
      const rows = document.querySelectorAll<HTMLElement>("[data-backup-row]");
      const row = rows[Number(new URLSearchParams(location.search).get("row") ?? 0)];
      if (!row) return;
      row.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 96, clientY: 132 })
      );
      await wait(200);
      const what = new URLSearchParams(location.search).get("click");
      if (what === "delete" || what === "restore") {
        document.querySelector<HTMLElement>(`[data-backup-menu-item="${what}"]`)?.click();
        await wait(200);
      }
    };
    void driver();
  }, []);
  return <BackupListModal open={open} t={t} theme={theme} onClose={() => setOpen(false)} />;
};

/**
 * 主窗口是 352px 宽。为了让"真实宽度下的排版"能被量到，这里把 app 外壳的
 * 关键祖先一并复现（`.app-container`）。
 */
/**
 * 主窗口宽 352px 是 tauri.conf.json 里的**默认**值，但 minWidth 是 250 且窗口可缩放。
 * 因此外壳宽度必须是"最多 352、随视口收缩"，不能写死 352——写死会让 250px 视口下
 * 量出 352 的容器，从而把一个**验证台自身的缺陷**误报成应用的横向溢出。
 */
const HARNESS_WIDTH_CSS = "min(352px, 100%)";

document.documentElement.classList.add(`theme-${theme}`);
document.documentElement.classList.add(`${colorMode}-mode`);
document.body.classList.add(`theme-${theme}`);
document.body.classList.add(`${colorMode}-mode`);

ReactDOM.createRoot(document.getElementById("root")!).render(
  <div
    className="app-container"
    style={{ width: HARNESS_WIDTH_CSS, height: "100vh", overflow: "hidden" }}
  >
    <div
      className="settings-view"
      style={{
        display: "flex",
        flexDirection: "column",
        gap: "12px",
        height: "100%",
        width: "100%",
        overflowY: "auto",
        padding: "8px",
        boxSizing: "border-box",
      }}
    >
      {mode === "groups" ? <Groups /> : <ModalStage />}
    </div>
  </div>
);

// 让截图脚本知道可以开始了
(window as unknown as { __READY__: boolean }).__READY__ = true;
