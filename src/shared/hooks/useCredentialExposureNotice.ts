import { useCallback, useEffect, useState } from "react";
import {
  fetchCredentialExposureNotice,
  type CredentialExposureNotice,
} from "../../features/settings/lib/credentialExposureNotice";

/**
 * 启动时查一次「存量凭据外流」的告知是否需要展示。
 *
 * # 为什么只在启动时查一次、且不做轮询
 *
 * 判据的全部输入都是本机设置表里的静态事实（配过没有、有没有上传痕迹），它们在一次
 * 会话里不会自己变化。轮询只会白跑 IPC。
 *
 * # 为什么查询失败时**不**展示
 *
 * 失败意味着"不知道"，而不是"需要提示"。把"不知道"当成"要提示"，等于给所有用户
 * （包括从未用过云同步的人）弹一条他自己无法证伪的安全警告——警告一旦变成噪音，
 * 真正受影响的人也只会闭着眼睛点掉。这与仓库里"不许把 UNKNOWN 洗成结论"的一贯做法一致。
 *
 * # 状态机（三态，避免用两个布尔拼出第四种非法组合）
 *
 * `loading` → `ready(notice | null)`。失败的语义与"不需要提示"一致，都落在 `null`，
 * 但失败会记日志，方便排障时区分。
 */
export function useCredentialExposureNotice(enabled: boolean) {
  const [notice, setNotice] = useState<CredentialExposureNotice | null>(null);
  const [settled, setSettled] = useState(false);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    fetchCredentialExposureNotice()
      .then((result) => {
        if (cancelled) return;
        setNotice(result && result.shouldNotify ? result : null);
      })
      .finally(() => {
        if (!cancelled) setSettled(true);
      });
    return () => {
      cancelled = true;
    };
  }, [enabled]);

  /**
   * 关掉弹窗。
   *
   * 注意这里**只改前端状态**：一次性标记由弹窗在用户点击时落盘（见
   * `CredentialExposureDialog::settle`）。若在此处就清掉 `notice`，一个"点了关闭但
   * 标记没写成功"的路径会让用户既没看到完整内容、下次也不再被提示。
   */
  const dismiss = useCallback(() => setNotice(null), []);

  return { notice, settled, dismiss };
}
