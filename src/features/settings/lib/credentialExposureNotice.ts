/**
 * 「存量凭据外流」升级告知 —— 前端侧的类型、判据入口与**三语文案**。
 *
 * # 这件事到底是什么（措辞的事实基线，改文案前先读这一段）
 *
 * 云同步的"设置快照"上传路径曾经把三个设置项一起传到用户自己的云端存储：
 * `mqtt_password`、`mqtt_username`、`ai_profiles`。原因不是"云端被攻破"，
 * 也不是"传给了我们"——**它们被上传到了用户自己配置的那个 WebDAV 空间**。
 * 修复只能阻止将来的上传，撤不回已经上传过的内容。
 *
 * 因此本文件里所有面向用户的文案都必须守住三条：
 *  1. 说清**是什么**：哪三项、传到了哪里（**你自己的云端存储**）；
 *  2. 说清**为什么建议换**：云端副本的可见范围由那家云服务/共享设置决定，不在本应用控制内；
 *  3. 不制造恐慌：不说"已经泄露到公网"，不说"被窃取"，不说"我们泄露了你的数据"。
 *
 * # 为什么文案不走 `src/locales.ts`
 *
 * 本轮任务明确划走了 `src/locales.ts` 的所有权（由用户统一维护）。这里的兜底表与
 * 仓库里既有的 `autoBackupErrorText` 同一做法：**先查 locales，查不到（返回键名本身）
 * 再退回本表**。于是：
 *
 * - 用户把草稿填进 `locales.ts` 之后，本表自动失效、不产生第二份权威；
 * - 还没填的时候界面也不会把 `security_exposure_title` 这种内部键名甩到用户脸上。
 *
 * 三语草稿（zh / en / tw）已随本文件一并交付，键名见 `EXPOSURE_LOCALE_KEYS`。
 */

import { invoke } from "@tauri-apps/api/core";
import { isTauriRuntime } from "../../../shared/lib/tauriRuntime";

/** 后端 `ExposureEvidence` 的三个取值（snake_case 序列化）。 */
export type ExposureEvidence = "confirmed_sync_history" | "configured_only" | "not_configured";

/** 后端 `CredentialExposureNotice`（camelCase 序列化）。 */
export interface CredentialExposureNotice {
  shouldNotify: boolean;
  evidence: ExposureEvidence;
  acknowledged: boolean;
  /** 那三项里**当前**本机还留着的键；只作文案材料，不参与判定。 */
  storedCredentialKeys: string[];
}

/**
 * 文案键清单（交付给用户填进 `locales.ts`）。
 *
 * 与 `src/features/settings/lib/autoBackupLocaleContract.test.ts` 同一约定：
 * 键名是界面与语言文件的唯一契约，测试直接对着这份清单核对三语齐全。
 */
export const EXPOSURE_LOCALE_KEYS = {
  title: "security_exposure_title",
  /** 事实陈述：哪三项、传到了哪里。 */
  body: "security_exposure_body",
  /** 为什么建议更换（把"风险由什么决定"讲清楚，不吓人）。 */
  why: "security_exposure_why",
  /** 怎么换：指向设置里的位置。 */
  how: "security_exposure_how",
  /**
   * 那三项在**清单里**的显示名。
   *
   * 【为什么不复用既有的 `mqtt_password` / `mqtt_user` / `ai_settings`】
   * 那三个键是给**输入框标签**写的，`mqtt_password` 的实际词条是"密码（可选）"。
   * 放进"这三项被上传了"的清单里，用户看到"密码（可选）"根本认不出指的是哪个密码。
   * 因此另开三个键：清单要的是"哪一项被上传了"，与输入框标签不是同一句话。
   */
  passwordItem: "security_exposure_item_mqtt_password",
  userItem: "security_exposure_item_mqtt_username",
  aiItem: "security_exposure_item_ai_keys",
  /** 主按钮：去设置里更换。 */
  goSettings: "security_exposure_go_settings",
  /** 次按钮：我已经知道了 / 稍后。 */
  dismiss: "security_exposure_dismiss",
} as const;

/**
 * 三语兜底文案（**权威草稿**，与报告里给用户的那份逐字一致）。
 *
 * 放置顺序与 `locales.ts` 一致：zh → en → tw。
 */
export const EXPOSURE_FALLBACK_TEXT: Record<"zh" | "en" | "tw", Record<string, string>> = {
  zh: {
    security_exposure_title: "安全提示：云同步曾上传过 MQTT 凭据",
    security_exposure_body:
      "这个版本修复了云同步的一个设置同步缺陷：过去如果启用过云同步，" +
      "下面三项会随设置快照一起上传到**你自己配置的云端存储**（WebDAV）。现在已修复，不再上传。",
    security_exposure_why:
      "已经上传的那份副本仍在你的云端存储里，撤不回来。它的可见范围由那家云服务的共享设置决定，" +
      "不由本应用控制。若你在这台机器上使用过 MQTT 同步，或配置过 AI 模型密钥，建议更换这些凭据。",
    security_exposure_how: "在「设置 → 同步（MQTT）」里修改用户名与密码，在「设置 → AI 助手」里更新模型密钥。",
    security_exposure_go_settings: "去设置里更换",
    security_exposure_dismiss: "我知道了",
    // 那三项在清单里的显示名。**刻意在这里给三语兜底**：仓库现有的 `mqtt_password`
    // 词条是"密码（可选）"，那是对着输入框写的，放进"这三项被上传了"的清单里读不出
    // 指的是什么。用户若已在 `locales.ts` 里提供同名键，以 locales 为准。
    security_exposure_item_mqtt_password: "MQTT 密码",
    security_exposure_item_mqtt_username: "MQTT 用户名",
    security_exposure_item_ai_keys: "AI 模型密钥",
  },
  en: {
    security_exposure_title: "Security notice: MQTT credentials were once uploaded by cloud sync",
    security_exposure_body:
      "This version fixes a settings-sync flaw in cloud sync. If you had ever enabled cloud sync, the three items below were uploaded with the settings snapshot to the cloud storage **you configured yourself** (WebDAV). That is fixed now and they are no longer uploaded.",
    security_exposure_why:
      "The copy that was already uploaded is still in your cloud storage and cannot be recalled. Who can see it depends on that cloud service's sharing settings, not on this app. If you used MQTT sync on this machine, or stored AI model keys here, we recommend replacing those credentials.",
    security_exposure_how:
      "Change the username and password under Settings → Sync (MQTT), and update the model keys under Settings → AI Assistant.",
    security_exposure_go_settings: "Open settings",
    security_exposure_dismiss: "Got it",
    security_exposure_item_mqtt_password: "MQTT password",
    security_exposure_item_mqtt_username: "MQTT username",
    security_exposure_item_ai_keys: "AI model keys",
  },
  tw: {
    security_exposure_title: "安全提示：雲端同步曾上傳過 MQTT 憑證",
    security_exposure_body:
      "這個版本修正了雲端同步的一項設定同步缺陷：過去若啟用過雲端同步，" +
      "下列三項會隨設定快照一併上傳到**你自己設定的雲端儲存**（WebDAV）。現在已修正，不會再上傳。",
    security_exposure_why:
      "已經上傳的那份副本仍在你的雲端儲存裡，收不回來。它的可見範圍取決於那家雲端服務的共享設定，" +
      "不由本應用程式控制。若你在這台機器上用過 MQTT 同步，或設定過 AI 模型金鑰，建議更換這些憑證。",
    security_exposure_how: "在「設定 → 同步（MQTT）」修改使用者名稱與密碼，在「設定 → AI 助手」更新模型金鑰。",
    security_exposure_go_settings: "前往設定更換",
    security_exposure_dismiss: "我知道了",
    security_exposure_item_mqtt_password: "MQTT 密碼",
    security_exposure_item_mqtt_username: "MQTT 使用者名稱",
    security_exposure_item_ai_keys: "AI 模型金鑰",
  },
};

/** 与 `App.tsx` 里的 `t` 同一形状：查不到词条时原样返回键名。 */
export type TranslateFn = (key: string) => string;

/** 当前界面语言：靠语言环境推断（`exposureLanguage`）。 */
export function resolveFallbackLanguage(language: string | undefined): "zh" | "en" | "tw" {
  if (language === "en" || language === "tw" || language === "zh") return language;
  return "zh";
}

/**
 * 取词条：**先 locales，后兜底表**。
 *
 * `t` 在查不到时会原样返回键名（见 `App.tsx`：`translations[language][k] || translations.en[k] || key`），
 * 因此"返回值和键名相同"就是"这条还没填进 locales"的可判定信号——
 * 与 `DataSettingsGroup::skipReasonText`、`autoBackupErrorText` 用的是同一个约定。
 */
export function exposureText(
  t: TranslateFn,
  language: string | undefined,
  key: string
): string {
  const primary = t(key);
  if (primary && primary !== key) return primary;
  const fallbackLang = resolveFallbackLanguage(language);
  return (
    EXPOSURE_FALLBACK_TEXT[fallbackLang][key] ??
    EXPOSURE_FALLBACK_TEXT.en[key] ??
    key
  );
}

/**
 * 向宿主查询"要不要提示"。
 *
 * 非 Tauri 环境（例如 vitest / 预览里打开 dist）直接返回 `null`：那条路径上根本没有
 * 设置表，凭空断言"要提示"会让所有浏览器里跑的人都看到一条假的安全警告。
 */
export async function fetchCredentialExposureNotice(): Promise<CredentialExposureNotice | null> {
  if (!isTauriRuntime()) return null;
  try {
    return await invoke<CredentialExposureNotice>("get_credential_exposure_notice");
  } catch (e) {
    console.error("get_credential_exposure_notice failed:", e);
    return null;
  }
}

/**
 * 记下"这条告知已经展示给用户了"。
 *
 * 只在用户点了按钮之后调用（见 `CredentialExposureDialog` 的 `handleAcknowledge`）。
 * 失败只记日志、不阻断界面：宁可下次启动再提示一次，也不要卡住用户的当前操作。
 */
export async function acknowledgeCredentialExposureNotice(): Promise<void> {
  if (!isTauriRuntime()) return;
  try {
    await invoke("mark_credential_exposure_notice_seen");
  } catch (e) {
    console.error("mark_credential_exposure_notice_seen failed:", e);
  }
}
