import { useEffect, useState } from "react";
import { ShieldAlert, X } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import {
  acknowledgeCredentialExposureNotice,
  exposureText,
  EXPOSURE_LOCALE_KEYS,
  type CredentialExposureNotice,
  type TranslateFn,
} from "../lib/credentialExposureNotice";

/**
 * 「存量凭据外流」升级告知弹窗。
 *
 * # 为什么复用 `.modal-overlay` + `.confirm-dialog` 这套类名
 *
 * 仓库里已经有这条视觉语言：`ConfirmDialog`、`BackupListModal` 的二次确认、
 * 标签管理的删除确认，用的都是 `.modal-overlay` 外壳加 `.confirm-dialog` 卡片。
 * 本窗是**同一种东西**（一个必须被读到、然后被关掉的说明），因此照抄这套结构，
 * 不新造样式——自成一派的弹窗在多主题下最容易出现"某个主题下看不见"的问题。
 *
 * 唯一的增量是**结构**（标题 + 事实 + 原因 + 怎么做 + 两个按钮），不是配色：
 * 颜色一律取自既有主题变量，`list-style` 之类只在最小范围内用内联样式。
 *
 * # 为什么按钮文案不是"确认 / 取消"
 *
 * `ConfirmDialog` 的默认措辞（确认/取消）回答的是"要不要执行某个动作"。这里没有
 * 待执行的动作——只有一条需要被读到的告知。因此两个按钮分别是**行动**（去设置里更换）
 * 与**收条**（我知道了）。用户在任意一个按钮上点击都算"已经看到"。
 *
 * # 为什么两个按钮都要落标记
 *
 * 标记的语义是"这条告知已经展示并处理过了"，不是"用户点了主按钮"。若只在主按钮落标记，
 * 点"我知道了"的用户下次启动还会再被弹一次——那正是把人往"闭着眼睛点"训练。
 */

interface CredentialExposureDialogProps {
  /** 为 null 时不渲染（查询未完成 / 非 Tauri 环境 / 不需要提示）。 */
  notice: CredentialExposureNotice | null;
  t: TranslateFn;
  language: string;
  theme: string;
  /** 用户决定去设置里处理：打开设置面板并跳到相关分组。 */
  onOpenSettings: () => void;
  /** 关掉弹窗（标记已在组件内部落盘）。 */
  onClose: () => void;
}

/**
 * 那三项在界面上的显示名。
 *
 * 用**专属**词条（`security_exposure_item_*`）而不是复用输入框标签：`mqtt_password`
 * 的既有词条是"密码（可选）"，那是对着输入框写的；放在"这三项被上传了"的清单里，
 * 用户读不出指的是哪个密码。词条名与后端 `CREDENTIAL_EXPOSURE_SUBJECT_KEYS` 逐项对应。
 */
const SUBJECT_ITEMS = [
  { settingsKey: "mqtt_password", labelKey: EXPOSURE_LOCALE_KEYS.passwordItem },
  { settingsKey: "mqtt_username", labelKey: EXPOSURE_LOCALE_KEYS.userItem },
  { settingsKey: "ai_profiles", labelKey: EXPOSURE_LOCALE_KEYS.aiItem },
] as const;

const CredentialExposureDialog = ({
  notice,
  t,
  language,
  theme,
  onOpenSettings,
  onClose,
}: CredentialExposureDialogProps) => {
  const [busy, setBusy] = useState(false);

  /**
   * 弹窗开着时不让主窗口因失焦而隐藏。
   *
   * 与 `UpdateDialog` 同一做法：主窗口的默认行为是失焦即隐藏，而这条告知必须被**读完**；
   * 若用户在读的过程中点到别处导致窗口消失，这条告知就等于没送达。
   */
  useEffect(() => {
    if (!notice) return;
    invoke("set_ignore_blur", { ignore: true }).catch(console.error);
    return () => {
      invoke("set_ignore_blur", { ignore: false }).catch(console.error);
    };
  }, [notice]);

  if (!notice) return null;

  const text = (key: string) => exposureText(t, language, key);

  /** 任一出口都先落标记，再关窗——顺序反了会在关窗崩掉时丢掉标记。 */
  const settle = async (after: () => void) => {
    if (busy) return;
    setBusy(true);
    await acknowledgeCredentialExposureNotice();
    setBusy(false);
    after();
  };

  const handleGoSettings = () => {
    void settle(() => {
      onClose();
      onOpenSettings();
    });
  };

  const handleDismiss = () => {
    void settle(onClose);
  };

  /** 只列出**本机当前确实还留着**的那几项，列不出就不列（不编造）。 */
  const storedItems = SUBJECT_ITEMS.filter((item) =>
    notice.storedCredentialKeys.includes(item.settingsKey)
  );

  return (
    <div className="modal-overlay" data-credential-exposure-dialog="" style={{ zIndex: 3400 }}>
      <div
        className={`confirm-dialog theme-${theme}`}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="credential-exposure-title"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: "460px" }}
      >
        <div
          className="confirm-dialog-title"
          id="credential-exposure-title"
          style={{ display: "flex", alignItems: "center", gap: "8px" }}
        >
          <ShieldAlert size={18} style={{ flexShrink: 0 }} />
          <span style={{ flex: 1 }}>{text("security_exposure_title")}</span>
          <button
            type="button"
            className="btn-icon"
            aria-label={text("security_exposure_dismiss")}
            data-credential-exposure-close=""
            disabled={busy}
            onClick={handleDismiss}
            style={{ flexShrink: 0 }}
          >
            <X size={14} />
          </button>
        </div>

        <div
          className="confirm-dialog-message"
          style={{ marginBottom: "12px", display: "flex", flexDirection: "column", gap: "10px" }}
        >
          <span data-credential-exposure-body="">{text("security_exposure_body")}</span>

          {/* 具体是哪三项。列的是"本机还留着的"，因此每一项都真实存在。 */}
          {storedItems.length > 0 && (
            <ul
              data-credential-exposure-items=""
              style={{ margin: 0, paddingLeft: "18px", fontSize: "13px", lineHeight: 1.7 }}
            >
              {storedItems.map((item) => (
                <li key={item.settingsKey}>{text(item.labelKey)}</li>
              ))}
            </ul>
          )}

          <span data-credential-exposure-why="" style={{ opacity: 0.9 }}>
            {text("security_exposure_why")}
          </span>
          <span data-credential-exposure-how="" style={{ opacity: 0.9 }}>
            {text("security_exposure_how")}
          </span>
        </div>

        <div className="confirm-dialog-buttons">
          <button
            type="button"
            className="confirm-dialog-button"
            data-credential-exposure-dismiss=""
            disabled={busy}
            onClick={handleDismiss}
          >
            {text("security_exposure_dismiss")}
          </button>
          <button
            type="button"
            className="confirm-dialog-button primary"
            data-credential-exposure-go-settings=""
            disabled={busy}
            onClick={handleGoSettings}
          >
            {text("security_exposure_go_settings")}
          </button>
        </div>
      </div>
    </div>
  );
};

export default CredentialExposureDialog;
