/**
 * 后端错误 → 结构化载荷 / 当前语言文案。
 *
 * # 为什么存在
 *
 * 后端（`system_cmd::backup_err`、`auto_backup::to_app_error`）把需要让用户看到原因的
 * 失败序列化成**裸 JSON**（`{"code":..,"detail":..}` 或带数值字段）。这套解析原先只写在
 * `DataSettingsGroup.tsx` 里；自动备份也要按原因码显示"当前最大留存数量为 N、已固定 M"
 * 这类**带真实数字**的提示，于是提出来共用，避免两份 `JSON.parse` 兜底逻辑各自演化。
 *
 * # 为什么还要裁前缀
 *
 * 后端有些错误变体的 `Display` 会加中文类别前缀（如 `验证错误: {...}`）。一旦如此，
 * 对整串 `JSON.parse` 必然失败，界面就只能把带前缀的原文（含中文）丢给英文/繁体用户，
 * 多语言映射全部失效。这里主动从第一个 `{` 起再解析一次。
 */

/** 解析结果：`code` 为 null 表示"这不是一个带原因码的后端错误"。 */
export interface BackendErrorPayload {
  code: string | null;
  /** 命中原因码时的明细（`detail` / `name` / 原因码本身）。 */
  detail: string;
  /** 解析成功时的完整对象，供调用方取数值字段（如 `maxKeep` / `currentPinned`）。 */
  fields: Record<string, unknown> | null;
  /** 原始文本，供无法解析时原样展示。 */
  raw: string;
}

export const parseBackendError = (e: unknown): BackendErrorPayload => {
  const raw = e instanceof Error ? e.message : String(e);

  const tryParse = (text: string): BackendErrorPayload | null => {
    try {
      const parsed = JSON.parse(text);
      if (parsed && typeof parsed === "object" && typeof parsed.code === "string") {
        return {
          code: parsed.code as string,
          detail: String(parsed.detail ?? parsed.name ?? parsed.code),
          fields: parsed as Record<string, unknown>,
          raw,
        };
      }
    } catch {
      /* 不是合法 JSON */
    }
    return null;
  };

  // 先按"裸 JSON"解析（后端用 `~AppError.Raw~` 时就是这个形状）。
  const direct = tryParse(raw);
  if (direct) return direct;

  const brace = raw.indexOf("{");
  if (brace > 0) {
    const sliced = tryParse(raw.slice(brace));
    if (sliced) return sliced;
  }

  return { code: null, detail: raw, fields: null, raw };
};

/**
 * 按原因码取当前语言文案；未知码退化为后端明细原文。
 *
 * 需要明细的码（如 `land_failed` / `io` / `count_mismatch`）会把 `{detail}` / `{e}`
 * 填进去。
 */
export const backendErrorText = (t: (key: string) => string, e: unknown): string => {
  const { code, detail } = parseBackendError(e);
  if (!code) return detail;
  const key = `backup_err_${code}`;
  const text = t(key);
  if (text === key) return detail;
  return text.replace("{detail}", detail).replace("{e}", detail);
};
