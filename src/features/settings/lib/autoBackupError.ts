import { parseBackendError } from "./backendError";

/**
 * 自动备份的后端错误 → 当前语言文案（带真实数字）。
 *
 * # 键名规则：去掉冗余前缀
 *
 * 后端的原因码是 `auto_backup_pinned_limit_reached` 这种带模块前缀的形式。若直接拼成
 * `auto_backup_err_auto_backup_pinned_limit_reached`，词条名会长到没法读，也容易写错。
 * 这里统一剥掉开头那一段 `auto_backup_`，于是每条词条名是
 * `auto_backup_err_<去掉前缀的码>`（例如 `auto_backup_err_pinned_limit_reached`）。
 *
 * # 为什么必须按字段填数，而不是在文案里写死
 *
 * 用户明确要求固定上限的提示里出现**真实数字**（"当前配置最大留存备份数量为 50，
 * 而当前您已固定 49 个"），50 与 49 都是用户可调的配置，不能在文案里写常量。后端在
 * 载荷里带了 `maxKeep` / `maxPinned` / `currentPinned`（以及越界类错误的 `min`/`max`/`value`），
 * 这里逐个替换；缺失时填 `?` 而不是留一个空占位符。
 *
 * 未知原因码退化为后端明细原文，绝不把 `auto_backup_err_xxx` 这种内部键名甩给用户。
 */
export const autoBackupErrorText = (t: (key: string) => string, e: unknown): string => {
  const { code, detail, fields } = parseBackendError(e);
  if (!code) return detail;

  const short = code.startsWith("auto_backup_") ? code.slice("auto_backup_".length) : code;
  const key = `auto_backup_err_${short}`;
  const text = t(key);
  if (text === key) return detail;

  const fill = (name: string) => String(fields?.[name] ?? "?");
  return text
    .replace(/\{maxKeep\}/g, fill("maxKeep"))
    .replace(/\{maxPinned\}/g, fill("maxPinned"))
    .replace(/\{currentPinned\}/g, fill("currentPinned"))
    .replace(/\{min\}/g, fill("min"))
    .replace(/\{max\}/g, fill("max"))
    .replace(/\{value\}/g, fill("value"))
    .replace(/\{name\}/g, fill("name"))
    .replace(/\{detail\}/g, detail)
    .replace(/\{e\}/g, detail);
};

export default autoBackupErrorText;
