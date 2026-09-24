import { useEffect } from "react";
import type { Dispatch, SetStateAction } from "react";
import type { DefaultAppsMap, InstalledAppOption } from "../../features/app/types";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { isTauriRuntime } from "../lib/tauriRuntime";
import type { AutostartState } from "../lib/autostart";

interface UseAppBootstrapOptions {
  fetchEffectiveTransferPath: () => void;
  setDataPath: Dispatch<SetStateAction<string>>;
  setInstalledApps: Dispatch<SetStateAction<InstalledAppOption[]>>;
  setAutoStart: Dispatch<SetStateAction<boolean>>;
  setDefaultApps: Dispatch<SetStateAction<DefaultAppsMap>>;
  setFileServerEnabled: Dispatch<SetStateAction<boolean>>;
  setActualPort: Dispatch<SetStateAction<string>>;
  setLocalIp: Dispatch<SetStateAction<string>>;
  setAvailableIps: Dispatch<SetStateAction<string[]>>;
  setWinClipboardDisabled: Dispatch<SetStateAction<boolean>>;
}

interface FileServerStatusPayload {
  enabled: boolean;
  port: number;
  ip: string;
}

export const useAppBootstrap = ({
  fetchEffectiveTransferPath,
  setDataPath,
  setInstalledApps,
  setAutoStart,
  setDefaultApps,
  setFileServerEnabled,
  setActualPort,
  setLocalIp,
  setAvailableIps,
  setWinClipboardDisabled: _setWinClipboardDisabled
}: UseAppBootstrapOptions) => {
  useEffect(() => {
    if (!isTauriRuntime()) return;

    fetchEffectiveTransferPath();

    invoke<string>("get_data_path").then(setDataPath).catch(console.error);

    invoke<{ name: string; path: string }[]>("scan_installed_apps")
      .then((apps) => {
        if (apps && apps.length > 0) {
          setInstalledApps(
            apps
              .map((a) => ({ label: a.name, value: a.path }))
              .sort((a, b) => a.label.localeCompare(b.label))
          );
        } else {
          console.warn("No apps found by scan_installed_apps");
        }
      })
      .catch((err) => {
        console.error("Failed to scan apps:", err);
      });

    // 自启动的初值：这里只取"是否生效"这一个布尔，用作设置页的首帧。
    // 真正的判定与证据展示在设置页的开关组件里（它会在挂载时重新回读一次）。
    // 后端返回的是**回读状态**（不是写入回话），因此这里读到 true 就是真的生效。
    invoke<AutostartState>("is_autostart_enabled")
      .then((s) => setAutoStart(s.enabled))
      .catch(console.error);


    const types = ["text", "rich_text", "image", "video", "code", "url"];
    types.forEach(async (type) => {
      try {
        const name = await invoke<string>("get_system_default_app", { contentType: type });
        setDefaultApps((prev) => ({ ...prev, [type]: name }));
      } catch (err) {
        console.error(`Failed to get default for ${type}`, err);
      }
    });

    const setupServerListener = async () => {
      const unlisten = await listen<FileServerStatusPayload>("file-server-status-changed", (event) => {
        const payload = event.payload;
        setFileServerEnabled(payload.enabled);
        setActualPort(payload.port === 0 ? "" : payload.port.toString());
        setLocalIp(payload.ip);
      });
      return unlisten;
    };

    let unlistenServer: (() => void) | undefined;
    setupServerListener().then((u) => {
      unlistenServer = u;
    });

    invoke<FileServerStatusPayload>("get_file_server_status")
      .then((status) => {
        setFileServerEnabled(status.enabled);
        setActualPort(status.port === 0 ? "" : status.port.toString());
        setLocalIp(status.ip);
      })
      .catch(console.error);

    invoke<string[]>("get_available_ips")
      .then((ips) => {
        if (ips && ips.length > 0) setAvailableIps(ips);
      })
      .catch(console.error);

    return () => {
      if (unlistenServer) unlistenServer();
    };
  }, [
    fetchEffectiveTransferPath,
    setActualPort,
    setAutoStart,
    setAvailableIps,
    setDataPath,
    setDefaultApps,
    setFileServerEnabled,
    setInstalledApps,
    setLocalIp,
  ]);
};
