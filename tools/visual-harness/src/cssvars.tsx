import React from "react";
import ReactDOM from "react-dom/client";

// ---- 真实样式，与 src/main.tsx 的加载顺序一致 ----
import "../../../src/index.css";
import "../../../src/styles/components/index.css";
import "../../../src/styles/themes/load";

// ---- 真实组件（一字未改）----
import ClipboardSettingsGroup from "../../../src/features/settings/components/groups/ClipboardSettingsGroup";
import SettingsFooter from "../../../src/features/settings/components/SettingsFooter";
import FileTransferChatView from "../../../src/features/file-transfer/components/FileTransferChatView";
import { translations } from "../../../src/locales";

/**
 * CSS 变量缺陷验证台。
 *
 * # 为什么必须挂真实组件
 *
 * 「变量未定义」的表现是整条声明被静默丢弃——量 `getComputedStyle` 才能看见，
 * 看 DOM 结构或类名看不出来。所以本页挂载真实组件，再由 `measure.mjs` 逐字段
 * 读计算值：修之前 border 是 0px，修之后必须是某具体宽度。
 *
 * `TagManager.tsx` 的样式走组件内的 `<style>{...}` 字符串，React 会把它原样
 * 注入文档；因此量它的规则不需要挂载组件本身，只需把它的样式块注入即可
 * （`tagmanager.mjs` 从源码里抽出那段字符串，保证与真实渲染逐字节一致）。
 * 之所以不整挂 TagManager：它依赖 12 个宿主命令与虚拟列表，挂载失败会让
 * 「量不到」被误读成「已修复」。样式块是与组件解耦的，单独注入同样有效。
 */

const lang = "zh" as const;
const t = (key: string): string => {
  const dict = translations[lang] as unknown as Record<string, string>;
  return dict[key] ?? key;
};

const theme = new URLSearchParams(location.search).get("theme") ?? "mica";
const colorMode = new URLSearchParams(location.search).get("colorMode") ?? "light";

document.documentElement.classList.add(`theme-${theme}`, `${colorMode}-mode`);
document.body.classList.add(`theme-${theme}`, `${colorMode}-mode`);

const LabelWithHint = ({ label }: { label: string; hint?: unknown; hintKey: string }) => (
  <div className="item-label-group">
    <span className="item-label">{label}</span>
  </div>
);

/** ClipboardSettingsGroup 的 64 个 props 里，本页只喂"能让目标输入框渲染出来"的必要项。 */
const noop = () => {};
const ClipboardGroup = () => (
  <ClipboardSettingsGroup
    t={t}
    collapsed={false}
    onToggle={noop}
    LabelWithHint={LabelWithHint}
    persistent
    setPersistent={noop}
    persistentLimitEnabled
    setPersistentLimitEnabled={noop}
    persistentLimit={200}
    setPersistentLimit={noop}
    saveAppSetting={noop}
    deduplicate
    setDeduplicate={noop}
    captureFiles
    setCaptureFiles={noop}
    captureRichText
    setCaptureRichText={noop}
    richTextSnapshotPreview
    setRichTextSnapshotPreview={noop}
    richPasteHotkey="Ctrl+Shift+V"
    isRecordingRich={false}
    setIsRecordingRich={noop}
    updateRichPasteHotkey={noop}
    searchHotkey="Ctrl+F"
    isRecordingSearch={false}
    setIsRecordingSearch={noop}
    updateSearchHotkey={noop}
    quickPasteModifier="none"
    setQuickPasteModifier={noop}
    quickPasteHotkey="Ctrl+Shift+Q"
    isRecordingQuickPaste={false}
    setIsRecordingQuickPaste={noop}
    updateQuickPasteHotkey={noop}
    historyLimit={500}
    setHistoryLimit={noop}
    historyLimitEnabled
    setHistoryLimitEnabled={noop}
    ignoreList={[]}
    newIgnoreRule=""
    setNewIgnoreRule={noop}
    addIgnoreRule={noop}
    removeIgnoreRule={noop}
    ignoreListEnabled
    setIgnoreListEnabled={noop}
    autoClearEnabled
    setAutoClearEnabled={noop}
    autoClearDays={7}
    setAutoClearDays={noop}
    maxItemSizeMb={10}
    setMaxItemSizeMb={noop}
    ignoreSensitiveApps={false}
    setIgnoreSensitiveApps={noop}
    sensitiveApps={[]}
    newSensitiveApp=""
    setNewSensitiveApp={noop}
    addSensitiveApp={noop}
    removeSensitiveApp={noop}
    sensitiveFeatureEnabled
    setSensitiveFeatureEnabled={noop}
    imageCaptureEnabled
    setImageCaptureEnabled={noop}
    maxImageSizeMb={10}
    setMaxImageSizeMb={noop}
  />
);

/**
 * SettingsFooter 的更新确认框只在 `pendingUpdate` 非空时渲染，而它只能由
 * `check()` 返回一个 Update 才置上。这里的 `@tauri-apps/plugin-updater` 由
 * vite alias 换成 mock（见 vite.config.mjs），点击「检查更新」即可让真实组件
 * 渲染出真实弹框——样式来自组件自己的 inline style，不受 mock 影响。
 */
const FooterStage = () => {
  const [status, setStatus] = React.useState("");
  React.useEffect(() => {
    const id = setInterval(() => {
      const btn = document.querySelector<HTMLElement>("[data-check-update]");
      if (btn) {
        clearInterval(id);
        btn.click();
      }
    }, 100);
    return () => clearInterval(id);
  }, []);
  return (
    <SettingsFooter
      t={t}
      appVersion="0.5.0"
      updateStatus={status}
      setUpdateStatus={setStatus}
      onResetSettings={noop}
    />
  );
};

const ChatStage = () => (
  <div style={{ width: 352, height: 520 }}>
    <FileTransferChatView t={t} localIp="192.168.1.20" actualPort="51820" />
  </div>
);

const stage = new URLSearchParams(location.search).get("stage") ?? "clipboard";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <div className="app-container" style={{ width: "min(352px, 100%)", minHeight: "100vh" }}>
    <div className="settings-view" style={{ padding: 8, boxSizing: "border-box" }}>
      {stage === "clipboard" && <ClipboardGroup />}
      {stage === "footer" && <FooterStage />}
      {stage === "chat" && <ChatStage />}
    </div>
  </div>
);

(window as unknown as { __READY__: boolean }).__READY__ = true;
