import { describe, it, expect } from "vitest";
import { selectTagSuggestions, tagSuggestionMaxHeightPx } from "./tagSuggestions";
import { TAG_SUGGEST_VISIBLE_ROWS, TAG_SUGGEST_LIST_MAX } from "../constants";

/**
 * 标签候补列表的规则测试。
 *
 * 这组测试存在的理由：该逻辑原先内联在组件的 `useMemo` 里、**零测试覆盖**，
 * 于是它带着一个明显的交互缺陷活到了用户真机上 —— 一打开标签编辑器就铺开全部标签，
 * 把条目内容遮住（用户称之为"遮挡栏"）。
 */

const base = {
    editing: true,
    query: "",
    allTags: ["ims", "img", "invoice", "笔记"],
    existingTags: [],
};

describe("标签候补：输入为空时不展示", () => {
    it("编辑器打开但没输入 → 不展示任何候补", () => {
        // ⚠️ 这是本次修复的核心。旧实现写作 `!q || tag.includes(q)`，
        // 「没输入就不过滤」⇒ 这里会返回全部 4 个标签。
        expect(selectTagSuggestions({ ...base, query: "" })).toEqual([]);
    });

    it("只有空白字符也算没输入", () => {
        expect(selectTagSuggestions({ ...base, query: "   " })).toEqual([]);
        expect(selectTagSuggestions({ ...base, query: "\t\n" })).toEqual([]);
    });

    it("编辑器未打开 → 不展示（即使有输入）", () => {
        expect(selectTagSuggestions({ ...base, editing: false, query: "i" })).toEqual([]);
    });
});

describe("标签候补：有输入才补全", () => {
    it("输入 i → 只返回包含 i 的", () => {
        expect(selectTagSuggestions({ ...base, query: "i" })).toEqual(["ims", "img", "invoice"]);
    });

    it("大小写无关", () => {
        expect(selectTagSuggestions({ ...base, query: "IMS" })).toEqual(["ims"]);
        expect(selectTagSuggestions({ ...base, query: "ImS" })).toEqual(["ims"]);
    });

    it("中文也能匹配", () => {
        expect(selectTagSuggestions({ ...base, query: "笔" })).toEqual(["笔记"]);
    });

    it("输入前后空格被忽略", () => {
        expect(selectTagSuggestions({ ...base, query: "  img  " })).toEqual(["img"]);
    });

    it("无匹配 → 空数组（界面据此不渲染浮层）", () => {
        expect(selectTagSuggestions({ ...base, query: "zzz" })).toEqual([]);
    });
});

describe("标签候补：排除条目已有标签", () => {
    it("已在该条目上的标签不再出现", () => {
        const r = selectTagSuggestions({ ...base, query: "i", existingTags: ["img"] });
        expect(r).toEqual(["ims", "invoice"]);
        expect(r).not.toContain("img");
    });

    it("全部已存在 → 空数组", () => {
        expect(
            selectTagSuggestions({ ...base, query: "i", existingTags: ["ims", "img", "invoice"] })
        ).toEqual([]);
    });
});

describe("标签候补：条数上限", () => {
    it("不超过 TAG_SUGGEST_LIST_MAX", () => {
        const many = Array.from({ length: 200 }, (_, i) => `tag${i}`);
        const r = selectTagSuggestions({ ...base, query: "tag", allTags: many });
        expect(r.length).toBe(TAG_SUGGEST_LIST_MAX);
    });

    it("上限足够填满可见的 4 行并留有滚动余量", () => {
        // 否则"可以用鼠标滚动查看"就无从谈起：条目数本身就不够多
        expect(TAG_SUGGEST_LIST_MAX).toBeGreaterThan(TAG_SUGGEST_VISIBLE_ROWS);
    });
});

describe("可见高度按行数推导", () => {
    it("4 行的高度 = 4 × 行高 + 内边距", () => {
        // 与 tags.css 的 `calc(rows × 行高 + 6px + 2px)` 同一算法。
        // 写死像素会在用户调整字号后失效（显示不满 4 行、或露出第 5 行半截）。
        expect(tagSuggestionMaxHeightPx(18, 4)).toBe(4 * 18 + 6 + 2);
    });

    it("行数变化时高度随之变化（不是写死的）", () => {
        expect(tagSuggestionMaxHeightPx(18, 5)).toBeGreaterThan(
            tagSuggestionMaxHeightPx(18, 4)
        );
    });
});
