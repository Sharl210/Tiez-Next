// @vitest-environment node
import { describe, it, expect } from "vitest";
import { translations } from "../../../locales";

/**
 * 迁移进度与 `deferred` 结果 ↔ `src/locales.ts` 的**契约测试**。
 *
 * # 为什么必须单独有这一份
 *
 * 界面测试（`DataSettingsGroup.migration.test.tsx`）用的是**自己写的 `t` 桩**。
 * 桩让交互能脱离语言文件测干净，但桩本身也会写错：
 * 一旦桩给某条文案编了真实词条里不存在的占位符，那么"数字被替换进提示"这类断言
 * 就会靠着桩自己编的东西变绿，而真机上用户看到的是一个没被替换的 `{progress}`。
 * 本仓库已经真实踩过这个坑（`auto_backup_delete_confirm` 三种语言都没有占位符，
 * 而桩给了 `{time}`/`{size}`/`{origin}`，见 `autoBackupLocaleContract.test.ts`）。
 *
 * 所以本文件**不 mock 语言文件**，直接读真实的 `translations`，断言：
 *  1. 界面会引用的每个 key 在三种语言里都存在（缺了页面上会直接显示 key 名）；
 *  2. 三种语言的**占位符集合完全一致**（不一致必然有一处替换不上）；
 *  3. 界面真正 `.replace()` 的占位符，在对应词条里确实存在。
 *
 * # 为什么不给 stage → 文案做词条
 *
 * 冻结契约 §2 明确：阶段名由后端在事件里给出人话（`stageLabel`），前端原样显示。
 * 因此这里**故意不检查**任何 `migration_progress_<stage>` 之类的键 ——
 * 它们不该存在。真出现两套翻译，就是下一个"后端改了文案用户还看旧话"的来源。
 */

const LANGS = ["zh", "en", "tw"] as const;

const dict = (lang: (typeof LANGS)[number]) =>
    translations[lang] as unknown as Record<string, string>;

const placeholders = (value: string): string[] =>
    Array.from(new Set(value.match(/\{[a-zA-Z]+\}/g) ?? [])).sort();

/** 迁移进度 / deferred 界面静态引用的全部 key。 */
const STATIC_KEYS = [
    "migration_deferred_title",
    "migration_deferred_hint",
    "migration_progress_indeterminate",
    "migration_progress_items",
    "migration_progress_bytes",
];

/** 界面还会复用的既有键（deferred 分支与结果卡片都用到）。 */
const REUSED_EXISTING_KEYS = ["legacy_migrate_restart_now", "legacy_migrate_result_migrated", "notice"];

describe("新增 locale 键的三语一致性", () => {
    it.each(STATIC_KEYS)("三种语言都有 %s", (key) => {
        for (const lang of LANGS) {
            const value = dict(lang)[key];
            expect(value, `${lang} 缺 ${key}`).toBeTruthy();
            // `t()` 查不到时返回 key 名本身；词条若等于 key 名等于没翻译。
            expect(value).not.toBe(key);
        }
    });

    it("复用的既有键也都在（deferred 分支依赖它们）", () => {
        for (const key of REUSED_EXISTING_KEYS) {
            for (const lang of LANGS) {
                expect(dict(lang)[key], `${lang} 缺 ${key}`).toBeTruthy();
            }
        }
    });

    it.each(STATIC_KEYS)("%s 的占位符三语完全一致", (key) => {
        const sets = LANGS.map((lang) => placeholders(dict(lang)[key]));
        expect(sets[0]).toEqual(sets[1]);
        expect(sets[1]).toEqual(sets[2]);
    });

    it("界面真正替换的 {progress} 在两条带占位符的词条里都存在", () => {
        // 界面里写的是 `.replace("{progress}", ...)`。词条里没有这个占位符，
        // 那一行就会原样把 `{progress}` 印给用户。
        for (const lang of LANGS) {
            expect(placeholders(dict(lang).migration_progress_items)).toContain("{progress}");
            expect(placeholders(dict(lang).migration_progress_bytes)).toContain("{progress}");
        }
    });

    it("「不确定进度」与「重启后接管」两条不含占位符（界面不做任何替换）", () => {
        for (const lang of LANGS) {
            expect(placeholders(dict(lang).migration_progress_indeterminate)).toEqual([]);
            expect(placeholders(dict(lang).migration_deferred_hint)).toEqual([]);
        }
    });

    it("不存在按 stage 命名的进度词条（那会与后端的 stageLabel 形成第二套翻译）", () => {
        const offenders = Object.keys(dict("zh")).filter((k) =>
            /^migration_progress_(precheck|copying|verifying|deferred|done|failed)$/.test(k)
        );
        expect(offenders).toEqual([]);
    });

    it("deferred 的中文文案明确交代「重启后自动完成接管」与「无需其他操作」", () => {
        // 这两点缺任一，用户就会把 deferred 理解成"出问题了"或"我还得再做点什么"。
        expect(dict("zh").migration_deferred_hint).toContain("重启应用后自动完成接管");
        expect(dict("zh").migration_deferred_hint).toContain("无需其他操作");
        // 反向：不得出现失败类措辞。
        for (const word of ["失败", "错误", "未完成", "无法"]) {
            expect(dict("zh").migration_deferred_hint).not.toContain(word);
        }
    });
});
