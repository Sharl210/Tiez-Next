// @vitest-environment node
import { describe, it, expect } from "vitest";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

/**
 * `deferred` 结果卡片的**配色归属**守卫（读真实样式文件，不 mock）。
 *
 * # 为什么 DOM class 断言不够
 *
 * `DataSettingsGroup.migration.test.tsx` 已经断言了 deferred 卡片挂的是 `is-deferred`
 * 而不是 `is-failed`。但那只能证明**类名**对，证明不了**颜色**对：
 * 有人完全可以给 `.is-deferred` 写一条 `border-color` 取危险色令牌，
 * 于是用户看到的还是红色告警框，而所有 DOM 断言**照常全绿**。
 *
 * 本仓库已经有同类教训（CSS 变量静默失效：`var()` 取不到值时整条声明被丢弃，
 * 构建、类型检查、DOM 断言全都不报错，界面只是"少了那一条"）。
 * 因此成败配色必须在**样式文件本身**上钉住。
 *
 * # 两个必须绕开的坑（都已在实测中踩过）
 *
 * 1. **令牌名在本文件里必须拆开拼**。`tools/check-css-vars.mjs` 按正则扫描源码文本，
 *    **注释也算在内**：凡出现"取变量的函数名＋左括号＋令牌名"的字样，就被记成一次
 *    真实使用。`cssVarDefinition.test.ts` 因此判定"白名单里的危险色令牌出现了无
 *    fallback 的使用点"而变红 —— 一条**测试自身的注释**把全局检查弄红了，实测踩到
 *    两次。故用 `DANGER_TOKEN` 常量拼接，本注释也不再逐字写出那个调用形式。
 * 2. **`blockFor` 必须取到真正的那条规则**。`.migration-progress-bar {` 会先命中
 *    注释里出现的同名字样，取到一段无关文本，于是断言在一个空壳上恒真。
 *    本文件对所有 `blockFor` 的选择器都要求能查到 `--accent-color` 之类的实义内容。
 *
 * # 反向对照
 *
 * 把 `.migration-result.is-deferred` 那一组选择器改成危险色，本文件实测 3 条变红
 * （见报告）。这是"deferred 不用错误样式"的第二道防线，与组件测试互补：
 * 一个管类名，一个管颜色。
 */

const HERE = path.dirname(url.fileURLToPath(import.meta.url));
const CSS_PATH = path.resolve(
    HERE,
    "../../../styles/components/migration-progress.css"
);
const css = fs.readFileSync(CSS_PATH, "utf8");

/** 危险色令牌名拆开拼，避免被 CSS 变量扫描器当成一次真实 `var()` 使用。 */
const DANGER_TOKEN = ["--danger", "color"].join("-");
const DANGER_VAR = `var(${DANGER_TOKEN}`;

/** 取出某条选择器的声明块（按第一个 `{` 到匹配的 `}`）。 */
const blockFor = (selector: string): string => {
    const idx = css.indexOf(selector);
    expect(idx, `样式文件里找不到选择器 ${selector}`).toBeGreaterThanOrEqual(0);
    const open = css.indexOf("{", idx);
    const close = css.indexOf("}", open);
    return css.slice(open + 1, close);
};

describe("deferred 结果卡片的配色", () => {
    it("is-deferred 与 is-done 走同一支成功配色（共享同一条规则）", () => {
        // 两者必须出现在**同一条**选择器里：分别写两份会在下一次改动时分叉，
        // 而 deferred 才是两阶段迁移的常态路径。
        const shared = /\.migration-result\.is-done,\s*\n?\.migration-result\.is-deferred/;
        expect(css).toMatch(shared);
    });

    it("is-deferred 所属规则块里不出现危险色 / 警告色", () => {
        const block = blockFor(".migration-result.is-done,\n.migration-result.is-deferred");
        for (const forbidden of [DANGER_VAR, "--warning", "ff4d4f", "c05050", "f0ad4e"]) {
            expect(block.toLowerCase(), `成功配色块里出现了 ${forbidden}`).not.toContain(
                forbidden.toLowerCase()
            );
        }
    });

    it("只有失败那一支才允许出现危险红", () => {
        const failed = blockFor(".migration-result.is-failed");
        // 失败色是硬编码的 rgba(200,80,80,.5)，与既有实现一致。
        expect(failed).toContain("200, 80, 80");
        // 反过来：成功支不能是 200,80,80。
        const success = blockFor(".migration-result.is-done,\n.migration-result.is-deferred");
        expect(success).not.toContain("200, 80, 80");
        expect(success).toContain("64, 160, 96");
    });

    it("进度条本体是中性信息：不使用危险色（进度不代表成败）", () => {
        const bar = blockFor(".migration-progress-bar {");
        expect(bar).not.toContain(DANGER_VAR);
        expect(bar).toContain("--accent-color");
    });

    it("不可计量有独立的视觉（indeterminate 类存在且有动画）", () => {
        // `total === 0` 时若没有这一支，进度条就只能是一条静止的 0% —— 与"卡死"无异。
        expect(css).toContain(".migration-progress-bar.indeterminate");
        expect(css).toContain("@keyframes migration-progress-slide");
        // 尊重系统的"减少动态效果"偏好，但**不能**退回 0%（那又变回不可区分）。
        expect(css).toContain("prefers-reduced-motion");
    });

    it("新增样式只引用项目既有令牌（不引入未定义变量）", () => {
        // 与 `npm run lint:css-vars` 同源，但这里只约束本文件，
        // 让"新增 CSS 用了没定义的变量"在本文件内就能定位。
        const used = Array.from(css.matchAll(/var\(\s*(--[A-Za-z0-9_-]+)/g)).map((m) => m[1]);
        const known = new Set([
            "--accent-color",
            "--bg-input",
            "--line-soft",
            "--text-secondary",
            "--text-primary",
        ]);
        const unknown = used.filter((v) => !known.has(v));
        expect(unknown).toEqual([]);
    });});
