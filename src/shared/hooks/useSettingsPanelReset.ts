import { useEffect } from "react";
import { createAllCollapsedGroups } from "../config/settingsGroups";

interface UseSettingsPanelResetOptions {
  showSettings: boolean;
  setCollapsedGroups: (val: Record<string, boolean>) => void;
  setSettingsSubpage: (val: "home" | "advanced") => void;
}

export const useSettingsPanelReset = ({
  showSettings,
  setCollapsedGroups,
  setSettingsSubpage
}: UseSettingsPanelResetOptions) => {
  useEffect(() => {
    if (showSettings) {
      setSettingsSubpage("home");
      // 从共享清单派生。**不要**在这里手写键 —— 这正是 bug 的来源：
      // 这里原先硬编码 9 个键，新增的 `mcp` / `auto_backup` 只加到了
      // `useAppState` 的初值里，于是重置后这两个键变 undefined，
      // 被 `SettingsPanel` 的 falsy 判定当成「展开」。详见 settingsGroups.ts 的注释。
      setCollapsedGroups(createAllCollapsedGroups());
    }
  }, [showSettings, setCollapsedGroups, setSettingsSubpage]);
};
