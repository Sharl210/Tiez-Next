import { useState } from "react";
import { DEFAULT_THEME } from "../../../shared/config/themes";
import { createAllCollapsedGroups } from "../../../shared/config/settingsGroups";
import { setRuntimeLocale } from "../../../shared/lib/runtimeLocale";
import type { ClipboardEntry, Locale } from "../../../shared/types";
import type {
  AppState,
  CloudSyncContentPrefs,
  DefaultAppsMap,
  InstalledAppOption,
  QuickPasteModifier,
  SettingsSubpage
} from "../types";
import { DEFAULT_CLOUD_SYNC_CONTENT_PREFS } from "../types";
import type { AiProfile, AppCleanupPolicy } from "../../settings/types";

const DEFAULT_AI_KEY = import.meta.env.VITE_AI_DEFAULT_API_KEY ?? "";

export const useAppState = (): AppState => {
  const [showSettings, setShowSettings] = useState(false);
  const [settingsSubpage, setSettingsSubpage] = useState<SettingsSubpage>("home");
  const [showTagManager, setShowTagManager] = useState(false);
  const [tagManagerEnabled, setTagManagerEnabled] = useState(true);
  // 初值从共享清单派生：新增分组只改 `shared/config/settingsGroups.ts` 一处，
  // 初值与设置页重置不会再各写一份（那正是 mcp/auto_backup 默认展开的成因）。
  const [collapsedGroups, setCollapsedGroups] = useState<Record<string, boolean>>(
    createAllCollapsedGroups
  );
  const [history, setHistory] = useState<ClipboardEntry[]>([]);
  const [search, setSearch] = useState("");
  const [isComposing, setIsComposing] = useState(false);
  const [searchIsFocused, setSearchIsFocused] = useState(false);
  const [showTagFilter, setShowTagFilter] = useState(false);
  const [tagInput, setTagInput] = useState("");
  const [showEmojiPanel, setShowEmojiPanel] = useState(false);
  const [emojiFavorites, setEmojiFavorites] = useState<string[]>([]);
  const [aiOptionsOpenId, setAiOptionsOpenId] = useState<number | null>(null);
  const [editingTagsId, setEditingTagsId] = useState<number | null>(null);
  const [revealedIds, setRevealedIds] = useState<Set<number>>(new Set());
  const [autoStart, setAutoStart] = useState(true);
  const [deduplicate, setDeduplicate] = useState(true);
  const [persistent, setPersistent] = useState(true);
  const [persistentLimitEnabled, setPersistentLimitEnabled] = useState(true);
  const [persistentLimit, setPersistentLimit] = useState<number>(1000);
  const [appSettings, setAppSettings] = useState<Record<string, string>>({});
  const [defaultApps, setDefaultApps] = useState<DefaultAppsMap>({});
  const [showAppSelector, setShowAppSelector] = useState<string | null>(null);
  const [chatMode, setChatMode] = useState(false);
  const [installedApps, setInstalledApps] = useState<InstalledAppOption[]>([]);
  const [dataPath, setDataPath] = useState<string>("");
  const [hotkey, setHotkey] = useState<string>("Alt+C");
  const [sequentialHotkey, setSequentialHotkey] = useState<string>("Alt+V");
  const [richPasteHotkey, setRichPasteHotkey] = useState<string>("Alt+Shift+V");
  const [searchHotkey, setSearchHotkey] = useState<string>("Alt+F");
  const [quickPasteModifier, setQuickPasteModifier] =
    useState<QuickPasteModifier>("disabled");
  const [sequentialMode, setSequentialModeState] = useState(false);
  const [isRecording, setIsRecording] = useState(false);
  const [isRecordingSequential, setIsRecordingSequential] = useState(false);
  const [isRecordingRich, setIsRecordingRich] = useState(false);
  const [isRecordingSearch, setIsRecordingSearch] = useState(false);
  const [deleteAfterPaste, setDeleteAfterPaste] = useState(false);
  const [moveToTopAfterPaste, setMoveToTopAfterPaste] = useState(true);
  const [privacyProtection, setPrivacyProtection] = useState(true);
  const [privacyProtectionKinds, setPrivacyProtectionKinds] = useState<string[]>([
    "phone",
    "idcard",
    "email",
    "secret"
  ]);
  const [privacyProtectionCustomRules, setPrivacyProtectionCustomRules] = useState<string>("");
  const [sensitiveMaskPrefixVisible, setSensitiveMaskPrefixVisible] = useState(3);
  const [sensitiveMaskSuffixVisible, setSensitiveMaskSuffixVisible] = useState(3);
  const [sensitiveMaskEmailDomain, setSensitiveMaskEmailDomain] = useState(false);
  const [cleanupRules, setCleanupRules] = useState<string>("");
  const [appCleanupPolicies, setAppCleanupPolicies] = useState<AppCleanupPolicy[]>([]);
  const [captureFiles, setCaptureFiles] = useState(true);
  const [captureRichText, setCaptureRichText] = useState(false);
  const [richTextSnapshotPreview, setRichTextSnapshotPreview] = useState(true);
  const [silentStart, setSilentStart] = useState(true);
  const [followMouse, setFollowMouse] = useState(false);
  const [showAppBorder, setShowAppBorder] = useState(false);
  const [winClipboardDisabled, setWinClipboardDisabled] = useState(false);
  const [registryWinVEnabled, setRegistryWinVEnabled] = useState(false);
  // 初值必须与后端默认一致（`setup.rs` / 粘贴路径都按 `shift_insert` 兜底）。
  // 原先是 `"simulate"`：那既不是后端的默认值，也不是下拉框里的任何一个选项，
  // 于是在"设置还没加载完"的那一帧，下拉框会显示成一个空白的非法状态。
  const [pasteMethod, setPasteMethod] = useState("shift_insert");
  const [theme, setTheme] = useState(DEFAULT_THEME);
  const [colorMode, setColorMode] = useState("system");
  const [showSourceAppIcon, setShowSourceAppIcon] = useState(true);

  const [compactMode, setCompactMode] = useState(false);
  const [clipboardItemFontSize, setClipboardItemFontSize] = useState(13);
  const [clipboardTagFontSize, setClipboardTagFontSize] = useState(10);
  const [emojiPanelEnabled, setEmojiPanelEnabled] = useState(false);
  const [emojiPanelTab, setEmojiPanelTab] = useState<"emoji" | "favorites">("emoji");
  const [showHotkeyHint, setShowHotkeyHint] = useState(false);
  const [showAutoCloseHint, setShowAutoCloseHint] = useState(false);
  const [language, setLanguage] = useState<Locale>("zh");
  // 把语言同步给模块级镜像：`UpdateDialog` / `Announcement` 这类没有 `t` prop 的
  // 最外层共享组件靠它取当前语言，否则它们的悬浮说明会写死一种语言。
  // 必须在函数体内（渲染期）赋值，effect 会晚一拍，导致这些文案滞后一次渲染。
  setRuntimeLocale(language);
  const [settingsLoaded, setSettingsLoaded] = useState(false);
  const [isWindowPinned, setIsWindowPinned] = useState(false);
  // 与设置加载同一首帧读取：避免先显示搜索框、再异步恢复为关闭状态造成闪烁。
  // Tauri 的持久设置仍是权威来源；localStorage 只是同步启动快照，加载后会校正。
  const [showSearchBox, setShowSearchBox] = useState(() => {
    try {
      return localStorage.getItem("tiez_show_search_box") !== "false";
    } catch {
      return true;
    }
  });
  const [scrollTopButtonEnabled, setScrollTopButtonEnabled] = useState(true);
  const [arrowKeySelection, setArrowKeySelection] = useState(true);
  const [hideTrayIcon, setHideTrayIcon] = useState(false);
  const [hideDockIcon, setHideDockIcon] = useState(false);
  const [edgeDocking, setEdgeDocking] = useState(false);
  const [customBackground, setCustomBackground] = useState<string>("");
  const [customBackgroundOpacity, setCustomBackgroundOpacity] = useState(45);
  const [surfaceOpacity, setSurfaceOpacity] = useState(50);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [isKeyboardMode, setIsKeyboardMode] = useState(false);
  const [isLoadingMore, setIsLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(true);
  const [currentOffset, setCurrentOffset] = useState(0);
  const [mqttEnabled, setMqttEnabled] = useState(false);
  const [mqttServer, setMqttServer] = useState("");
  const [mqttPort, setMqttPort] = useState("1883");
  const [mqttUser, setMqttUser] = useState("");
  const [mqttPass, setMqttPass] = useState("");
  const [mqttTopic, setMqttTopic] = useState("");
  const [mqttProtocol, setMqttProtocol] = useState("mqtt://");
  const [mqttWsPath, setMqttWsPath] = useState("/mqtt");
  const [mqttNotificationEnabled, setMqttNotificationEnabled] = useState(true);
  const [cloudSyncEnabled, setCloudSyncEnabled] = useState(false);
  const [cloudSyncAuto, setCloudSyncAuto] = useState(true);
  const [cloudSyncProvider, setCloudSyncProvider] = useState<"http" | "webdav">("webdav");
  const [cloudSyncServer, setCloudSyncServer] = useState("");
  const [cloudSyncApiKey, setCloudSyncApiKey] = useState("");
  const [cloudSyncIntervalSec, setCloudSyncIntervalSec] = useState("120");
  const [cloudSyncSnapshotIntervalMin, setCloudSyncSnapshotIntervalMin] = useState("720");
  const [cloudSyncWebdavUrl, setCloudSyncWebdavUrl] = useState("");
  const [cloudSyncWebdavUsername, setCloudSyncWebdavUsername] = useState("");
  const [cloudSyncWebdavPassword, setCloudSyncWebdavPassword] = useState("");
  const [cloudSyncWebdavBasePath, setCloudSyncWebdavBasePath] = useState("tiez-sync");
  const [cloudSyncContentPrefs, setCloudSyncContentPrefs] =
    useState<CloudSyncContentPrefs>(DEFAULT_CLOUD_SYNC_CONTENT_PREFS);
  const [fileServerEnabled, setFileServerEnabled] = useState(false);
  const [fileServerPort, setFileServerPort] = useState("12345");
  const [localIp, setLocalIp] = useState("");
  const [availableIps, setAvailableIps] = useState<string[]>([]);
  const [actualPort, setActualPort] = useState("");
  const [fileTransferPath, setFileTransferPath] = useState("");
  const [fileTransferAutoOpen, setFileTransferAutoOpen] = useState(false);
  const [fileTransferAutoCopy, setFileTransferAutoCopy] = useState(false);
  const [fileServerAutoClose, setFileServerAutoClose] = useState(false);
  const [soundEnabled, setSoundEnabled] = useState(false);
  const [pasteSoundEnabled, setPasteSoundEnabled] = useState(true);
  const [soundVolume, setSoundVolume] = useState(1.0);
  const [aiEnabled, setAiEnabled] = useState(true);
  const [aiTargetLang, setAiTargetLang] = useState("zh");
  const [aiThinkingBudget, setAiThinkingBudget] = useState("1024");
  const [aiProfiles, setAiProfiles] = useState<AiProfile[]>([
    {
      id: "lc_flash_v1",
      baseUrl: "https://api.longcat.chat/openai/v1",
      apiKey: DEFAULT_AI_KEY,
      model: "LongCat-Flash-Chat",
      enableThinking: false
    },
    {
      id: "lc_think_v1",
      baseUrl: "https://api.longcat.chat/openai/v1",
      apiKey: DEFAULT_AI_KEY,
      model: "LongCat-Flash-Thinking",
      enableThinking: true
    },
    {
      id: "lc_think_2601_v1",
      baseUrl: "https://api.longcat.chat/openai/v1",
      apiKey: DEFAULT_AI_KEY,
      model: "LongCat-Flash-Thinking-2601",
      enableThinking: true
    }
  ]);
  const [aiAssignedProfileTask, setAiAssignedProfileTask] = useState("default");
  const [aiAssignedProfileMouthpiece, setAiAssignedProfileMouthpiece] = useState("default");
  const [aiAssignedProfileTranslate, setAiAssignedProfileTranslate] = useState("default");
  const [processingAiId, setProcessingAiId] = useState<number | null>(null);
  const [typeFilter, setTypeFilter] = useState<string | null>(null);

  return {
    showSettings,
    setShowSettings,
    settingsSubpage,
    setSettingsSubpage,
    showTagManager,
    setShowTagManager,
    tagManagerEnabled,
    setTagManagerEnabled,
    collapsedGroups,
    setCollapsedGroups,
    history,
    setHistory,
    search,
    setSearch,
    isComposing,
    setIsComposing,
    searchIsFocused,
    setSearchIsFocused,
    showTagFilter,
    setShowTagFilter,
    tagInput,
    setTagInput,
    showEmojiPanel,
    setShowEmojiPanel,
    emojiFavorites,
    setEmojiFavorites,
    aiOptionsOpenId,
    setAiOptionsOpenId,
    editingTagsId,
    setEditingTagsId,
    revealedIds,
    setRevealedIds,
    autoStart,
    setAutoStart,
    deduplicate,
    setDeduplicate,
    persistent,
    setPersistent,
    persistentLimitEnabled,
    setPersistentLimitEnabled,
    persistentLimit,
    setPersistentLimit,
    appSettings,
    setAppSettings,
    defaultApps,
    setDefaultApps,
    showAppSelector,
    setShowAppSelector,
    chatMode,
    setChatMode,
    installedApps,
    setInstalledApps,
    dataPath,
    setDataPath,
    hotkey,
    setHotkey,
    sequentialHotkey,
    setSequentialHotkey,
    richPasteHotkey,
    setRichPasteHotkey,
    searchHotkey,
    setSearchHotkey,
    quickPasteModifier,
    setQuickPasteModifier,
    sequentialMode,
    setSequentialModeState,
    isRecording,
    setIsRecording,
    isRecordingSequential,
    setIsRecordingSequential,
    isRecordingRich,
    setIsRecordingRich,
    isRecordingSearch,
    setIsRecordingSearch,
    deleteAfterPaste,
    setDeleteAfterPaste,
    moveToTopAfterPaste,
    setMoveToTopAfterPaste,
    privacyProtection,
    setPrivacyProtection,
    privacyProtectionKinds,
    setPrivacyProtectionKinds,
    privacyProtectionCustomRules,
    setPrivacyProtectionCustomRules,
    sensitiveMaskPrefixVisible,
    setSensitiveMaskPrefixVisible,
    sensitiveMaskSuffixVisible,
    setSensitiveMaskSuffixVisible,
    sensitiveMaskEmailDomain,
    setSensitiveMaskEmailDomain,
    cleanupRules,
    setCleanupRules,
    appCleanupPolicies,
    setAppCleanupPolicies,
    captureFiles,
    setCaptureFiles,
    captureRichText,
    setCaptureRichText,
    richTextSnapshotPreview,
    setRichTextSnapshotPreview,
    silentStart,
    setSilentStart,
    followMouse,
    setFollowMouse,
    showAppBorder,
    setShowAppBorder,
    winClipboardDisabled,
    setWinClipboardDisabled,
    registryWinVEnabled,
    setRegistryWinVEnabled,
    pasteMethod,
    setPasteMethod,
    theme,
    setTheme,
    colorMode,
    setColorMode,
    showSourceAppIcon,
    setShowSourceAppIcon,

    compactMode,
    setCompactMode,
    clipboardItemFontSize,
    setClipboardItemFontSize,
    clipboardTagFontSize,
    setClipboardTagFontSize,
    emojiPanelEnabled,
    setEmojiPanelEnabled,
    emojiPanelTab,
    setEmojiPanelTab,
    showHotkeyHint,
    setShowHotkeyHint,
    showAutoCloseHint,
    setShowAutoCloseHint,
    language,
    setLanguage,
    settingsLoaded,
    setSettingsLoaded,
    isWindowPinned,
    setIsWindowPinned,
    showSearchBox,
    setShowSearchBox,
    scrollTopButtonEnabled,
    setScrollTopButtonEnabled,
    arrowKeySelection,
    setArrowKeySelection,
    hideTrayIcon,
    setHideTrayIcon,
    hideDockIcon,
    setHideDockIcon,
    edgeDocking,
    setEdgeDocking,

    customBackground,
    setCustomBackground,
    customBackgroundOpacity,
    setCustomBackgroundOpacity,
    surfaceOpacity,
    setSurfaceOpacity,
    selectedIndex,
    setSelectedIndex,
    isKeyboardMode,
    setIsKeyboardMode,
    isLoadingMore,
    setIsLoadingMore,
    hasMore,
    setHasMore,
    currentOffset,
    setCurrentOffset,
    mqttEnabled,
    setMqttEnabled,
    mqttServer,
    setMqttServer,
    mqttPort,
    setMqttPort,
    mqttUser,
    setMqttUser,
    mqttPass,
    setMqttPass,
    mqttTopic,
    setMqttTopic,
    mqttProtocol,
    setMqttProtocol,
    mqttWsPath,
    setMqttWsPath,
    mqttNotificationEnabled,
    setMqttNotificationEnabled,
    cloudSyncEnabled,
    setCloudSyncEnabled,
    cloudSyncAuto,
    setCloudSyncAuto,
    cloudSyncProvider,
    setCloudSyncProvider,
    cloudSyncServer,
    setCloudSyncServer,
    cloudSyncApiKey,
    setCloudSyncApiKey,
    cloudSyncIntervalSec,
    setCloudSyncIntervalSec,
    cloudSyncSnapshotIntervalMin,
    setCloudSyncSnapshotIntervalMin,
    cloudSyncWebdavUrl,
    setCloudSyncWebdavUrl,
    cloudSyncWebdavUsername,
    setCloudSyncWebdavUsername,
    cloudSyncWebdavPassword,
    setCloudSyncWebdavPassword,
    cloudSyncWebdavBasePath,
    setCloudSyncWebdavBasePath,
    cloudSyncContentPrefs,
    setCloudSyncContentPrefs,
    fileServerEnabled,
    setFileServerEnabled,
    fileServerPort,
    setFileServerPort,
    localIp,
    setLocalIp,
    availableIps,
    setAvailableIps,
    actualPort,
    setActualPort,
    fileTransferPath,
    setFileTransferPath,
    fileTransferAutoOpen,
    setFileTransferAutoOpen,
    fileTransferAutoCopy,
    setFileTransferAutoCopy,
    fileServerAutoClose,
    setFileServerAutoClose,
    soundEnabled,
    setSoundEnabled,
    pasteSoundEnabled,
    setPasteSoundEnabled,
    soundVolume,
    setSoundVolume,
    aiEnabled,
    setAiEnabled,
    aiTargetLang,
    setAiTargetLang,
    aiThinkingBudget,
    setAiThinkingBudget,
    aiProfiles,
    setAiProfiles,
    aiAssignedProfileTask,
    setAiAssignedProfileTask,
    aiAssignedProfileMouthpiece,
    setAiAssignedProfileMouthpiece,
    aiAssignedProfileTranslate,
    setAiAssignedProfileTranslate,
    processingAiId,
    setProcessingAiId,
    typeFilter,
    setTypeFilter
  };
};
