// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { translations } from "../../../locales";
import { autoBackupErrorText } from "../lib/autoBackupError";

/**
 * 自动备份界面 ↔ `src/locales.ts` ↔ 后端错误载荷 的**契约测试**。
 *
 * # 这个文件为什么必须存在
 *
 * 上面那组界面测试用的是一个**自己写的 `t` 桩**。桩的好处是能不依赖语言文件就把
 * 交互测干净；坏处是**桩自己也会写错**：一旦桩里给某条文案编了真实词条里并不存在的
 * 占位符，那么"确认框写清了删的是哪一份"这类断言就会靠着桩编的占位符变绿，而真机上
 * 用户看到的是一个不指明对象的确认框——测试完全失去判别力。这不是假设，是本轮真实
 * 发生过的：`auto_backup_delete_confirm` / `auto_backup_restore_confirm` 在三种语言里
 * **都没有占位符**，而桩给了 `{time}`/`{size}`/`{origin}`。
 *
 * 因此本文件**不 mock 语言文件**，直接读真实的 `translations`，断言三件事：
 *  1. 界面会引用的每个 key 在三种语言里都存在（缺了会显示成 key 本身）；
 *  2. 三种语言的**占位符集合完全一致**（不一致必然有一处替换不上、把 `{x}` 漏给用户）；
 *  3. 我在代码里真正 `.replace()` 的占位符，在对应词条里确实存在。
 *
 * 第 3 条是这类缺陷的唯一机械检出手段：把"我以为词条里有 {time}"变成一条会红的断言。
 */

const LANGS = ["zh", "en", "tw"] as const;

/** 词条值的类型是字符串；语言字典是嵌套对象。 */
const dict = (lang: (typeof LANGS)[number]) =>
  translations[lang] as unknown as Record<string, string>;

const placeholders = (value: string): string[] =>
  Array.from(new Set(value.match(/\{[a-zA-Z]+\}/g) ?? [])).sort();

/** 界面静态引用（`t("...")` 字面量）的全部 auto_backup_* key。 */
const STATIC_KEYS = [
  "auto_backup_section",
  "auto_backup_intro",
  "auto_backup_enabled",
  "auto_backup_enabled_hint",
  "auto_backup_interval",
  "auto_backup_interval_hint",
  "auto_backup_max_keep",
  "auto_backup_max_keep_hint",
  "auto_backup_on_startup",
  "auto_backup_on_startup_hint",
  "auto_backup_summary",
  "auto_backup_summary_unknown",
  "auto_backup_list_open",
  "auto_backup_pin_note",
  "auto_backup_list",
  "auto_backup_dir",
  "auto_backup_refresh",
  "auto_backup_run_now",
  "auto_backup_open_folder",
  "auto_backup_loading",
  "auto_backup_empty",
  "auto_backup_modal_counts",
  "auto_backup_pinned_badge",
  "auto_backup_row_hint",
  "auto_backup_delete_title",
  "auto_backup_delete_confirm",
  "auto_backup_restore_title",
  "auto_backup_restore_confirm",
  "auto_backup_restore",
  "auto_backup_pin",
  "auto_backup_unpin",
  // 动态拼接：`auto_backup_origin_<origin>`，origin 取后端 `origin_key()` 的三个值。
  "auto_backup_origin_scheduled",
  "auto_backup_origin_startup",
  "auto_backup_origin_manual",
  // 动态拼接：`auto_backup_err_<去掉模块前缀的原因码>`。
  "auto_backup_err_pinned_limit_reached",
  "auto_backup_err_max_keep_out_of_range",
  "auto_backup_err_interval_out_of_range",
  "auto_backup_err_not_found",
  "auto_backup_err_invalid_name",
  "auto_backup_err_dir_inside_data_dir",
  "auto_backup_err_export_failed",
  "auto_backup_err_io",
] as const;

/** 复用既有导入链的重启文案（本功能刻意不新增词条）。 */
const REUSED_KEYS = ["backup_import_restart", "backup_import_restart_now", "cancel", "delete"] as const;

describe("locale 契约（对着真实的 src/locales.ts）", () => {
  it("界面引用的每个 key 在三语里都存在", () => {
    for (const lang of LANGS) {
      const missing = [...STATIC_KEYS, ...REUSED_KEYS].filter((k) => !dict(lang)[k]);
      expect(missing, `语言 ${lang} 缺少词条`).toEqual([]);
    }
  });

  it("三语的占位符集合一致（不一致必然有语言会把 {x} 漏给用户）", () => {
    const mismatched: string[] = [];
    for (const key of [...STATIC_KEYS, ...REUSED_KEYS]) {
      const sets = LANGS.map((lang) => placeholders(dict(lang)[key] ?? "").join(","));
      if (new Set(sets).size !== 1) mismatched.push(`${key}: ${sets.join(" | ")}`);
    }
    expect(mismatched).toEqual([]);
  });

  it("我在代码里真正 replace 的占位符，在词条里确实存在", () => {
    // 这份映射就是代码里的实际用法：key -> 代码会替换的占位符。
    const REPLACED: Record<string, string[]> = {
      auto_backup_max_keep_hint: ["{min}", "{max}"],
      auto_backup_summary: ["{total}", "{pinned}", "{size}"],
      auto_backup_modal_counts: ["{total}", "{pinned}", "{maxPinned}"],
      auto_backup_dir: ["{path}"],
      auto_backup_err_pinned_limit_reached: ["{maxKeep}", "{currentPinned}"],
      auto_backup_err_max_keep_out_of_range: ["{value}", "{min}", "{max}"],
      auto_backup_err_interval_out_of_range: ["{value}", "{min}", "{max}"],
      auto_backup_err_not_found: ["{name}"],
      auto_backup_err_export_failed: ["{detail}"],
      auto_backup_err_io: ["{detail}"],
      auto_backup_err_invalid_name: ["{detail}"],
      auto_backup_err_dir_inside_data_dir: [],
    };
    const broken: string[] = [];
    for (const [key, wanted] of Object.entries(REPLACED)) {
      const have = placeholders(dict("zh")[key] ?? "");
      for (const w of wanted) {
        if (!have.includes(w)) broken.push(`${key} 缺少 ${w}（实际：${have.join(",") || "无"}）`);
      }
    }
    expect(broken).toEqual([]);
  });

  /**
   * 反向锚点：确认文案**按设计不含占位符**，身份信息由弹窗单独渲染。
   *
   * 把这条固定下来，是为了防止下一个人"顺手"往文案里塞 `{time}` 之后，以为界面会填。
   * 词条里没有、代码里却 `.replace("{time}")` 的写法不会报错，只会静默地什么也不填。
   */
  it("删除/恢复确认文案不含占位符（身份信息由独立一行渲染）", () => {
    for (const key of ["auto_backup_delete_confirm", "auto_backup_restore_confirm"]) {
      for (const lang of LANGS) {
        expect(placeholders(dict(lang)[key]), `${lang}.${key}`).toEqual([]);
      }
    }
  });
});

describe("后端错误码 → 文案（真实词条）", () => {
  const tzh = (key: string) => dict("zh")[key] ?? key;
  const ten = (key: string) => dict("en")[key] ?? key;

  it("固定上限达顶：填入载荷里的真实数字", () => {
    const err = new Error(
      JSON.stringify({
        code: "auto_backup_pinned_limit_reached",
        maxKeep: 50,
        maxPinned: 49,
        currentPinned: 49,
      })
    );
    const text = autoBackupErrorText(tzh, err);
    expect(text).toContain("50");
    expect(text).toContain("49");
    expect(text).not.toContain("{maxKeep}");
    expect(text).not.toContain("{currentPinned}");
  });

  it("英文词条同样被填满，不残留占位符", () => {
    const err = new Error(
      JSON.stringify({
        code: "auto_backup_pinned_limit_reached",
        maxKeep: 200,
        maxPinned: 199,
        currentPinned: 199,
      })
    );
    const text = autoBackupErrorText(ten, err);
    expect(text).toContain("200");
    expect(text).toContain("199");
    expect(text).not.toMatch(/\{[a-zA-Z]+\}/);
  });

  it("份数越界：三语都填上实际值与区间", () => {
    const err = new Error(
      JSON.stringify({ code: "auto_backup_max_keep_out_of_range", value: 501, min: 1, max: 200 })
    );
    for (const lang of LANGS) {
      const text = autoBackupErrorText((k) => dict(lang)[k] ?? k, err);
      expect(text, lang).toContain("501");
      expect(text, lang).toContain("200");
      expect(text, lang).not.toMatch(/\{[a-zA-Z]+\}/);
    }
  });

  it("八个已知原因码在真实语言文件里都能查到（不会退化成内部键名）", () => {
    const codes = [
      "auto_backup_pinned_limit_reached",
      "auto_backup_max_keep_out_of_range",
      "auto_backup_interval_out_of_range",
      "auto_backup_not_found",
      "auto_backup_invalid_name",
      "auto_backup_dir_inside_data_dir",
      "auto_backup_export_failed",
      "auto_backup_io",
    ];
    for (const code of codes) {
      const text = autoBackupErrorText(tzh, new Error(JSON.stringify({ code, detail: "x" })));
      expect(text, code).not.toBe(code);
      expect(text, code).not.toContain("auto_backup_err_");
    }
  });
});
