import { describe, it, expect } from "vitest";
import { collectSelectableTags } from "./selectableTags";

/**
 * 可选标签池的规则测试。
 *
 * 这组测试存在的理由：该逻辑原先内联在 `App.tsx` 的 `useMemo` 里、**零覆盖**，
 * 于是它带着"在标签管理页建好的标签，主页面打标签时搜不到"这个缺陷活到了真机。
 */

const base = { savedTagNames: [], historyTags: [], needed: true };

describe("可选标签池：未被使用的标签也要在", () => {
    it("saved_tags 里没有条目在用的标签，仍然出现在池子里", () => {
        // ⚠️ 这是本次修复的核心。旧实现只看 historyTags，这里会得到 []。
        const r = collectSelectableTags({
            ...base,
            savedTagNames: ["ims-未使用", "img-未使用"],
            historyTags: [],
        });
        expect(r).toEqual(["img-未使用", "ims-未使用"]);
    });

    it("只建过、没用过的标签 + 已用过的标签，两者都在", () => {
        const r = collectSelectableTags({
            ...base,
            savedTagNames: ["ims", "invoice"],
            historyTags: ["ims"],
        });
        expect(r).toEqual(["ims", "invoice"]);
    });
});

describe("可选标签池：合并与去重", () => {
    it("同一名字出现在两个来源时只保留一份", () => {
        const r = collectSelectableTags({
            ...base,
            savedTagNames: ["work"],
            historyTags: ["work", "work"],
        });
        expect(r).toEqual(["work"]);
    });

    it("saved_tags 里已删除、但条目上仍留着的标签依然保留", () => {
        // 那种标签仍然是这条记录的真实标签，不该在选择器里消失。
        const r = collectSelectableTags({
            ...base,
            savedTagNames: ["work"],
            historyTags: ["work", "旧标签"],
        });
        expect(r).toContain("旧标签");
    });

    it("空字符串被忽略（不产生一个空白候选项）", () => {
        const r = collectSelectableTags({
            ...base,
            savedTagNames: ["", "ok"],
            historyTags: ["", ""],
        });
        expect(r).toEqual(["ok"]);
    });
});

describe("可选标签池：不需要时为真空", () => {
    it("needed=false → 空数组（不白算）", () => {
        const r = collectSelectableTags({
            savedTagNames: ["a"],
            historyTags: ["b"],
            needed: false,
        });
        expect(r).toEqual([]);
    });
});

describe("可选标签池：排序稳定", () => {
    it("按名称排序，且与输入顺序无关", () => {
        const a = collectSelectableTags({ ...base, savedTagNames: ["c", "a", "b"], historyTags: [] });
        const b = collectSelectableTags({ ...base, savedTagNames: ["b", "c", "a"], historyTags: [] });
        expect(a).toEqual(["a", "b", "c"]);
        expect(a).toEqual(b);
    });
});
