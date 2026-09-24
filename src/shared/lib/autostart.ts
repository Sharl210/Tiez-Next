/**
 * 开机自启动的**回读状态**：界面显示的依据。
 *
 * # 为什么不是一个布尔值
 *
 * 这个功能的旧实现是"点一下开关 → 点亮 → 后台写注册表，写失败只打 console"。
 * 于是开关**永远停在"已开"**，而系统可能根本不会在开机时拉起这个程序：
 * 值没写进去，或者写进去的是一个**指向改名/搬家前旧路径**的失效命令。
 *
 * 用户对这类设置的要求说得很直白：「纯应用里面显示设置了不一定生效」。
 * 所以判定"是否生效"不能听写入 API 的回话，只能**回读注册表、比对值内容**。
 * 下面这几个字段就是回读的结果：开关亮不亮由 `enabled` 决定，而 `enabled` 的判据是
 * "注册表里的命令**指向当前这个 exe**"，不是"某个名字存在"。
 */
export interface AutostartState {
  /** 当前是否**真的**会开机自启（注册表值存在 **且** 指向当前 exe）。 */
  enabled: boolean;
  /** 注册表里读回的命令原文：这是"真的生效了"唯一可信的证据，界面应展示给用户。 */
  registeredCommand: string | null;
  /** 当前进程自身的 exe 路径（与回读值对比的基准）。 */
  currentExe: string;
  /**
   * 旧版本遗留的自启动项名字（`TieZ` / `tie-z`）。
   *
   * 它们**不**参与"是否已开启"的判定——这正是老判据的漏洞：只要旧名残留，
   * 就算新值没写成功也会报"已开启"。这里只如实告知，并在关闭时顺手清理。
   */
  staleNames: string[];
  /** 判定过程是否可信。`false` 表示读注册表本身失败（不是"未开启"）。 */
  readable: boolean;
}

/** 后端不可用 / 尚未读取时的保守初值：一律按"未开启且未能确认"处理。 */
export const UNKNOWN_AUTOSTART_STATE: AutostartState = {
  enabled: false,
  registeredCommand: null,
  currentExe: "",
  staleNames: [],
  readable: false,
};

/**
 * 从读回的命令串里取出路径部分（`"C:\app.exe" --minimized` → `C:\app.exe`）。
 *
 * 界面展示的是**读回来的原文**，但原串带引号和启动参数，直接铺在设置行里很吵；
 * 这里只做展示用的裁剪，判定仍由后端进行（前端不重复实现判据，避免两处判据分叉）。
 */
export const registeredPathOf = (command: string): string => {
  const trimmed = command.trim();
  if (trimmed.startsWith('"')) {
    const end = trimmed.indexOf('"', 1);
    return end > 0 ? trimmed.slice(1, end) : trimmed.slice(1);
  }
  return trimmed.split(/\s+/)[0] ?? trimmed;
};
