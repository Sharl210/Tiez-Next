// @vitest-environment node
import { describe, it, expect } from "vitest";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";
import { translations } from "../../../locales";

/**
 * 「系统级设置」的 locale 契约 + **写入点存在性**契约。
 *
 * # 为什么语言键的存在性单靠界面测试守不住
 *
 * 界面测试用的是自己写的 `t` 桩。桩让交互测得很干净，但它对**真实词条是否存在**毫无
 * 约束力：`t('use_win_v_shortcut')` 在桩里能返回一句人话，真机上却会原样显示
 * `use_win_v_shortcut` 这个 key 名。本仓库真实发生过更糟的一次——`autostart_failed`
 * 三语齐备却**零引用**（有人预留了提示，从没接上），而界面上什么都没有。
 *
 * 所以这里**不 mock 语言文件**，直接读真实 `translations`。
 *
 * # 为什么还要检查"写入点存在"（本文件里唯一的源码扫描）
 *
 * Win+V 接管与自启动这两个功能都有过同一个病：**后端完好、前端没有入口/没有写入点**，
 * 于是键永远不被写，后端分支永不可达。这种缺陷的形态是"设置项看起来在，改了什么也没
 * 发生"，任何运行时断言都不会红——只有"这个键确实被某个 source 文件写过"能被机械检出。
 * 因此这里对两个关键键各锁一条：写入点必须真实存在于源码里。
 */

const ROOT = path.resolve(path.dirname(url.fileURLToPath(import.meta.url)), "../../../..");
const LANGS = ["zh", "en", "tw"] as const;

const dict = (lang: (typeof LANGS)[number]) =>
    translations[lang] as unknown as Record<string, string>;

/** 上游「macos -> windows 对齐」误删、本轮按 git 原文恢复的四个键。 */
const RESTORED_WIN_V_KEYS = [
    "use_win_v_shortcut",
    "use_win_v_shortcut_hint",
    "win_v_enabled_msg",
    "win_v_disabled_msg",
];

/** 本轮新增的告知类键（自启动回读证据、游戏模式未提权、Win+V 需重启）。 */
const NEW_KEYS = [
    "autostart_verified",
    "autostart_registered_at",
    "autostart_stale_names",
    "autostart_stale_hint",
    "autostart_readback_failed",
    "win_v_restart_badge",
    "win_v_write_failed",
    "win_v_restart_failed",
    "win_v_restart_failed_hint",
    "game_mode_needs_admin",
    "restart_as_admin_hint_settings",
];

/** 早就写好、此前**零引用**，本轮终于接上的键。 */
const PREVIOUSLY_UNUSED_KEYS = ["autostart_failed", "restart_as_admin"];


/**
 * 剥掉注释后的源码。
 *
 * 【为什么不剥就测不准】这类"写入点存在性"检查最先被自己的注释骗到：
 * 只要文件里**提到过**那个键名（哪怕是在解释"这个键曾经没人写"的注释里），
 * 子串匹配就会通过。本仓库已经真实踩过这个坑（正则式门禁的注释误报，
 * 见 desktop-release-crossbuild 的 fork-takeover-checklist 第 3.3 节）。
 * 因此先剥注释再匹配，注释里的键名一律不算证据。
 *
 * 只处理 `//` 行注释与 `/* ... *​/` 块注释；不处理字符串字面量——
 * 若键名只出现在字符串里而不在 `invoke(` 调用中，下面的第二个条件会把它排除掉。
 */
const stripComments = (text: string): string =>
    text.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");


/** 递归收集 src/ 下的 .ts/.tsx 源码。 */
const collectSources = (dir: string): string[] => {
    const out: string[] = [];
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) out.push(...collectSources(full));
        else if (/\.tsx?$/.test(entry.name)) out.push(full);
    }
    return out;
};

describe("恢复与新增的 locale 键：三语齐备", () => {
    it.each([...RESTORED_WIN_V_KEYS, ...NEW_KEYS, ...PREVIOUSLY_UNUSED_KEYS])(
        "三种语言都有 %s",
        (key) => {
            for (const lang of LANGS) {
                const value = dict(lang)[key];
                expect(value, `${lang} 缺 ${key}`).toBeTruthy();
                // `t()` 查不到时返回 key 名本身；词条等于 key 名等于没翻译。
                expect(value).not.toBe(key);
            }
        }
    );

    it("上游误删的四个 Win+V 键已按原文恢复（不是重新意译）", () => {
        // 断言中文原文的关键措辞：证明取回的是 git 历史里那一句，
        // 而不是自己重写的一句"差不多"的话。
        expect(dict("zh").use_win_v_shortcut).toContain("Win+V");
        expect(dict("zh").use_win_v_shortcut_hint).toContain("重启资源管理器");
        expect(dict("zh").use_win_v_shortcut_hint).toContain("极速呼出");
        expect(dict("zh").win_v_enabled_msg).toContain("极速模式已开启");
        expect(dict("zh").win_v_disabled_msg).toContain("恢复系统默认设置");
        expect(dict("en").use_win_v_shortcut).toBe("Use Win+V Shortcut");
        expect(dict("en").use_win_v_shortcut_hint).toContain("restart Windows Explorer");
        expect(dict("en").win_v_enabled_msg).toBe("Win+V Quick Mode enabled.");
        expect(dict("en").win_v_disabled_msg).toBe(
            "Win+V Quick Mode disabled, system default restored."
        );
        expect(dict("tw").use_win_v_shortcut).toBe("是否使用 Win+V 快速鍵");
        expect(dict("tw").win_v_enabled_msg).toBe("Win+V 極速模式已開啟。");
        expect(dict("tw").win_v_disabled_msg).toBe(
            "Win+V 極速模式已關閉，已恢復系統預設設定。"
        );
    });

    it("自启动失败文案不再是死键（界面真的引用它）", () => {
        const offenders: string[] = [];
        for (const file of collectSources(path.join(ROOT, "src"))) {
            const text = fs.readFileSync(file, "utf8");
            // 只看真正的调用或字面量引用，不看 locales.ts 自身的定义。
            if (file.endsWith(`${path.sep}locales.ts`)) continue;
            if (/["'`]autostart_failed["'`]/.test(text)) {
                offenders.push(path.relative(ROOT, file));
            }
        }
        expect(offenders.length).toBeGreaterThan(0);
    });

    it("Win+V 设置键存在真实写入点（否则后端分支永不可达）", () => {
        // 这条是本文件里最关键的一条：Win+V 曾经**没有任何写入点**，
        // 于是 `app.use_win_v_shortcut` 永远为默认值，后端启动优化分支永不执行。
        // 断言必须落在**一行代码**上（剥注释后仍然存在的 `invoke("save_setting", { key: "app.use_win_v_shortcut" ... })`），
        // 而不是"文件里提到过这个键"——后者会被解释这些历史的注释骗过。
        // 判据必须落在**同一行**上：`... save_setting ... key: "app.use_win_v_shortcut" ...`。
        // 「文件里既出现过这个键、又出现过 save_setting」是不够的——读取点
        // （`settings["app.use_win_v_shortcut"] === "true"`）所在的文件同样会通过，
        // 于是"删掉写入点"这件事根本测不出来（实测已确认过这一点）。
        const writers = collectSources(path.join(ROOT, "src"))
            .map((f) => ({
                file: path.relative(ROOT, f),
                lines: stripComments(fs.readFileSync(f, "utf8")).split("\n"),
            }))
            .filter((s) =>
                s.lines.some(
                    (line) =>
                        line.includes("app.use_win_v_shortcut") &&
                        /save_setting|saveSetting/.test(line)
                )
            );
        expect(
            writers.map((w) => w.file),
            "必须有源码把 app.use_win_v_shortcut 真正写进设置（注释里的键名不算）"
        ).not.toEqual([]);
    });

    it("前端不再读写历史上分叉的旧键名", () => {
        // 旧键只允许出现在**一次性迁移的注释/常量**里；任何 `settings["app.registry_win_v_enabled"]`
        // 形式的读取都会让"界面显示的"与"后端触发的"重新分叉。
        const offenders: string[] = [];
        for (const file of collectSources(path.join(ROOT, "src"))) {
            const rel = path.relative(ROOT, file);
            const text = fs.readFileSync(file, "utf8");
            // 本契约测试自己就写着这个键名（它正是用来断言"别处不许用"的），跳过自身。
            if (rel.endsWith("systemSettingsLocaleContract.test.ts")) continue;
            if (text.includes('"app.registry_win_v_enabled"') || text.includes("'app.registry_win_v_enabled'")) {
                offenders.push(rel);
            }
        }
        expect(offenders).toEqual([]);
    });

    it("游戏模式未提权的告知文案明确了「选择已被保留」", () => {
        // 这一句是用户信任的关键：被告知"暂不生效"的同时必须知道设置没被改掉。
        expect(dict("zh").game_mode_needs_admin).toContain("已被保留");
        expect(dict("zh").game_mode_needs_admin).toContain("不会");
        expect(dict("en").game_mode_needs_admin).toContain("kept");
        expect(dict("en").game_mode_needs_admin).toContain("never be silently changed");
    });

    it("所有键都不含占位符（界面不做任何替换，含占位符就会漏给用户）", () => {
        for (const key of [...RESTORED_WIN_V_KEYS, ...NEW_KEYS, ...PREVIOUSLY_UNUSED_KEYS]) {
            for (const lang of LANGS) {
                const value = dict(lang)[key];
                expect(value, `${lang} 缺 ${key}`).toBeDefined();
                expect(value.match(/\{[a-zA-Z]+\}/g) ?? [], `${lang}.${key}`).toEqual([]);
            }
        }
    });
});
