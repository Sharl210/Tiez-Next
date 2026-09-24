import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";
import { fileURLToPath } from "node:url";

// 本文件位于 <repo>/tools/visual-harness/，仓库根是它的两级上级。
// 不写死绝对路径，换机器/换检出位置都能跑。
const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../..");

export default defineConfig({
  root: HERE,
  plugins: [react()],
  resolve: {
    alias: {
      // 只替换宿主 API，被测组件一行不改。
      "@tauri-apps/api/core": path.resolve(HERE, "src/mock/core.ts"),
      "@tauri-apps/api/event": path.resolve(HERE, "src/mock/event.ts"),
      "@tauri-apps/plugin-dialog": path.resolve(HERE, "src/mock/dialog.ts"),
      "@tauri-apps/api/app": path.resolve(HERE, "src/mock/app.ts"),
      // 组件源码里用的是仓库内相对路径，这里补上别名便于 harness 引用。
      "@repo": REPO,
      // FileTransferChatView / SettingsFooter 额外依赖的宿主模块。
      "@tauri-apps/api/window": path.resolve(HERE, "src/mock/window.ts"),
      "@tauri-apps/plugin-updater": path.resolve(HERE, "src/mock/updater.ts"),
      "@tauri-apps/plugin-opener": path.resolve(HERE, "src/mock/opener.ts"),
      "@tauri-apps/plugin-process": path.resolve(HERE, "src/mock/process.ts"),
    },
  },
  build: {
    outDir: path.resolve(HERE, "dist"),
    emptyOutDir: true,
    chunkSizeWarningLimit: 4000,
    // 两个入口：index.html = 既有组件级视觉验证台；src/cssvars.html = CSS 变量探针台，
    // 由 measure.mjs 驱动读 computed style。两者共用同一套真实样式与组件。
    rollupOptions: {
      input: {
        main: path.resolve(HERE, "index.html"),
        cssvars: path.resolve(HERE, "src/cssvars.html"),
      },
    },
  },
  server: { fs: { allow: [REPO, HERE] } },
});
