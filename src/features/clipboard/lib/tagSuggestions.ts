import { TAG_SUGGEST_LIST_MAX } from "../constants";

/**
 * 标签输入框的候补列表选择。
 *
 * # 为什么抽成纯函数
 *
 * 这段逻辑原先内联在 `ClipboardItem` 的 `useMemo` 里，**没有任何测试覆盖** ——
 * 于是它能带着一个明显的交互缺陷存活到用户真机上被发现。
 *
 * 缺陷是这一行：
 *
 * ```ts
 * .filter((tag) => !q || tag.toLowerCase().includes(q))   // ← 旧写法
 * ```
 *
 * `!q ||` 的意思是"**没输入就不过滤**" ⇒ 一打开标签编辑器就列出全部标签（再截 14 个），
 * 输入框下方凭空多出一大片面板，把条目内容遮住。用户的原话是"不要这种快捷添加的
 * 遮挡栏"，并且明确要求"**我输了内容才进行补全列表的展示**"。
 *
 * 抽出来之后这条规则可以被直接断言（见同目录 `tagSuggestions.test.ts`），
 * 而不是只能靠真机观察。
 */

export interface TagSuggestionInput {
    /** 标签编辑器是否打开。未打开时不应有任何候补。 */
    editing: boolean;
    /** 输入框里的原始文本（未 trim、未小写）。 */
    query: string;
    /** 全部可用标签（来自应用级 state）。 */
    allTags: string[];
    /** 该条目**已有**的标签，不应再出现在候补里。 */
    existingTags: string[];
}

/**
 * 选出要展示的候补标签。
 *
 * 规则（按顺序）：
 * 1. 编辑器没打开 → 空
 * 2. **输入为空 → 空**（这是本次修复的核心；旧实现会返回全部标签）
 * 3. 排除条目已有的标签
 * 4. 只保留**包含**输入串的（大小写无关）
 * 5. 截到上限
 *
 * 第 2 条是用户明确要求的："我输了内容才进行补全列表的展示"。浏览全部标签有
 * 标签管理页，不需要在这里铺开。
 */
export function selectTagSuggestions({
    editing,
    query,
    allTags,
    existingTags,
}: TagSuggestionInput): string[] {
    if (!editing) return [];

    const q = query.trim().toLowerCase();
    // 【输入为空就不展示】—— 见上方文档注释
    if (!q) return [];

    const existing = new Set(existingTags);
    return allTags
        .filter((tag) => !existing.has(tag))
        .filter((tag) => tag.toLowerCase().includes(q))
        .slice(0, TAG_SUGGEST_LIST_MAX);
}

/**
 * 候补列表的可见高度上限（CSS 像素），由"行数 × 行高 + 内边距"推导。
 *
 * 与 `tags.css` 的 `.tag-edit-suggestions-popover` 用同一组变量，这里只用于测试断言
 * 与文档：真正的布局由 CSS 负责。`TAG_SUGGEST_VISIBLE_ROWS` 通过内联 `--tag-suggest-rows`
 * 传给 CSS，两边不会分叉。
 */
export function tagSuggestionMaxHeightPx(rowHeightPx: number, rows: number): number {
    return rows * rowHeightPx + 6 + 2;
}
