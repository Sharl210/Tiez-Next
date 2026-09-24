# 组件级视觉验证台

**用途**：在**没有目标平台宿主**的情况下，把真实组件渲染出来并量测。

本机是 WSL2，跑不了 Windows 应用，但界面问题（排版、溢出、样式失效、文案泄漏）
可以在浏览器里验。这个验证台就是为此存在：**组件代码一行不改**，只把宿主 API 换成 mock。

## 怎么用

```bash
# 在仓库根目录
npx vite --config tools/visual-harness/vite.config.mjs
```

然后在浏览器里打开输出的地址。

## 它替换了什么（也只替换这些）

`vite.config.mjs` 里的 `resolve.alias` 把四个宿主模块指到 `src/mock/`：

| 真实模块 | mock |
|---|---|
| `@tauri-apps/api/core` | `src/mock/core.ts`（`invoke` 返回预设数据） |
| `@tauri-apps/api/event` | `src/mock/event.ts`（`listen` 返回 no-op） |
| `@tauri-apps/plugin-dialog` | `src/mock/dialog.ts` |
| `@tauri-apps/api/app` | `src/mock/app.ts`（版本号等） |

**被测组件的源码不动**，所以量到的几何与样式就是真实的。

## 配套脚本

| 脚本 | 作用 |
|---|---|
| `shot.mjs` / `shot2.mjs` | 按不同视口宽度截图 |
| `geo.mjs` | 量测元素几何（宽高、padding、圆角、边框） |
| `btn.mjs` | 按钮逐字段量测（用来对比相邻组件是否一致） |
| `overflow.mjs` / `overflow2.mjs` | 检查横向溢出（`scrollWidth` vs `clientWidth`） |

它们用系统 Chrome（`/opt/google/chrome/chrome`）跑，无需额外安装浏览器。

## 为什么需要"逐字段量测"

**视觉模型的印象会出错。** 实测案例：视觉工具判断某按钮是"全圆 pill、邻居是圆角矩形"，
而逐字段量测显示两者的 `radius 12 / height 26 / width 84 / font 10px / border none`
**完全相同** —— 小尺寸下缩放会误导视觉判断。所以判定样式一致性时：

1. **量测几何**（可复现的数字），不要只靠截图印象
2. 必要时**按原始像素裁图**复核

## 为什么需要"反向对照"

实测案例：验证台一度把外壳宽度**写死 352px**，于是在 250px 视口下量出
"分组 336 > 视口 250" 的**假溢出**。

**识别为验证台缺陷后，必须把据此误加的样式回退** —— 否则会把"验证工具的 bug"
写进生产代码。判断方法：把被测对象换成一个已知正常的组件，若也"溢出"，问题在验证台。

## 边界（必须如实说明）

- 只验证**渲染与样式**，不验证真实 Tauri 命令的行为（`invoke` 是 mock 的）
- 不验证平台特性（托盘、全局快捷键、窗口管理）
- 不替代真机验证
