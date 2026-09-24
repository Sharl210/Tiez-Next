import { translations } from "../../locales";
import type { Locale } from "../types";

/**
 * 当前界面语言的运行时镜像。
 *
 * # 为什么需要它
 *
 * 绝大多数组件通过 `t` prop 拿到翻译函数（由 `App` 用 `useCallback` 按语言重建）。
 * 但有几个挂在最外层的共享组件（`UpdateDialog`、`Announcement`）**没有 `t` prop**，
 * 于是它们的 `title` 只能写死中/英文——切换语言时这些悬浮说明不会跟着变，
 * 中文用户看到英文、英文用户看到中文，属于"半个 tooltip"。
 *
 * 给它们补 `t` prop 需要动 `App.tsx`（本轮授权范围之外），因此改为让它们从一个
 * 模块级镜像里自取当前语言。
 *
 * # 为什么在渲染期赋值
 *
 * `useAppState` 里持有的 `language` 就是唯一事实源。镜像若放在 `useEffect` 里更新，
 * 顺序会变成「App 先用新语言重渲染子组件（此时镜像还是旧值）→ 之后 effect 才更新镜像」，
 * 于是标题会慢一拍、且不会自行纠正（后续没有触发重渲染的状态变化）。
 * 因此在 `useAppState` 的**函数体内**赋值：它在 App 渲染期间、子组件渲染之前执行，
 * 子组件读到的必定是本次渲染对应的语言。该赋值幂等、无副作用，可以安全地在渲染期执行。
 */
let currentLocale: Locale = "zh";

/** 由 `useAppState` 在每次渲染时写入当前语言。 */
export const setRuntimeLocale = (locale: Locale): void => {
  currentLocale = locale;
};

export const getRuntimeLocale = (): Locale => currentLocale;

/**
 * 按当前语言翻译一个键。
 *
 * 回退顺序与 `App.tsx` 里的 `t` 保持一致：当前语言 → 英文 → 原样返回键名
 * （原样返回键名是"这条还没填进 locales"的可判定信号，便于自查）。
 */
export const runtimeT = (key: string): string => {
  const k = key as keyof typeof translations["zh"];
  return translations[currentLocale][k] || translations["en"][k] || key;
};
