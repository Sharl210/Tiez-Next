import { useState, useEffect, useCallback } from "react";
import { check, Update } from "@tauri-apps/plugin-updater";
import { openUrl } from "@tauri-apps/plugin-opener";
import { isTauriRuntime } from "../lib/tauriRuntime";

export type UpdateStatus = "idle" | "checking" | "downloading" | "ready" | "error";

export const useAutoUpdate = () => {
  const [isOpen, setIsOpen] = useState(false);
  const [status, setStatus] = useState<UpdateStatus>("idle");
  const [version, setVersion] = useState("");
  const [notes, setNotes] = useState("");
  const [downloadProgress] = useState(0);
  const [updateObj, setUpdateObj] = useState<Update | null>(null);

  const checkUpdate = useCallback(async () => {
    if (!isTauriRuntime()) return;
    
    try {
      setStatus("checking");
      
      // Use the native check() which handles RID allocation internally
      const update = await check({
        proxy: undefined, // Or configure if needed
        headers: { "Cache-Control": "no-cache" },
        timeout: 10000
      });

      if (update) {
        console.log(`[Update] New version detected: ${update.version}`);
        setUpdateObj(update);
        setVersion(update.version);
        setNotes(update.body || "");
        setIsOpen(true);
      } else {
        // No update found, emit an event so the UI can show "Up to date"
        import('@tauri-apps/api/event').then(({ emit }) => {
          emit("update-not-available");
        });
      }
      
      setStatus("idle");
    } catch (error) {
      console.error("[Update] Failed to check for updates:", error);
      setStatus("error");
    }
  }, []);

  const startUpdate = async () => {
    if (!updateObj) return;
    const releaseUrl = `https://github.com/Sharl210/Tiez-Next/releases/tag/v${updateObj.version}`;
    await openUrl(releaseUrl).catch((error) => console.error("[Update] Failed to open release:", error));
    setIsOpen(false);
  };

  const applyUpdate = async () => {
    if (!updateObj) return;
    const releaseUrl = `https://github.com/Sharl210/Tiez-Next/releases/tag/v${updateObj.version}`;
    await openUrl(releaseUrl).catch((error) => console.error("[Update] Failed to open release:", error));
    setIsOpen(false);
  };

  useEffect(() => {
    const timer = setTimeout(() => {
      checkUpdate();
    }, 5000);

    const setupListener = async () => {
      if (isTauriRuntime()) {
        const { listen } = await import('@tauri-apps/api/event');
        return listen("check-update-manually", () => {
          checkUpdate();
        });
      }
      return () => {};
    };

    let unlisten: (() => void) | undefined;
    setupListener().then(fn => { unlisten = fn; });
    
    return () => {
      clearTimeout(timer);
      if (unlisten) unlisten();
    };
  }, [checkUpdate]);

  return {
    isOpen,
    status,
    version,
    notes,
    downloadProgress,
    onManualUpdate: checkUpdate,
    onStartDownload: startUpdate,
    onApplyUpdate: applyUpdate,
    onClose: () => setIsOpen(false),
  };
};
