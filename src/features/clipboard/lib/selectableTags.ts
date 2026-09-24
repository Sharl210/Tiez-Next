/**
 * 页面上"可选的全部标签"的收集规则。
 *
 * # 为什么抽成纯函数
 *
 * 这段逻辑原先内联在 `App.tsx` 的 `useMemo` 里，**没有任何测试覆盖** ——
 * 于是它带着一个用户直接感知的缺陷活到了真机：**只**从已加载的历史条目里收集
 * 标签名，导致"在标签管理页建好、但还没赋给任何条目的标签"在主页面打标签时
 * **搜不到**。用户的原话是"怎么输入都只有这个"。
 *
 * 而标签管理页读的是 `get_all_tags_info`（后端 `tag_repo.get_all_with_counts`），
 * 那个查询**已经把 `saved_tags` 里 0 条目的标签也列出来了**
 * （见其内注释 "Also include saved tags with 0 count"）。
 * 两边数据源不同 → 同一个标签在管理页看得到、在主页面看不到。
 *
 * 抽出来之后这条规则可以被直接断言（见 `selectableTags.test.ts`），
 * 而不是只能靠真机观察。
 */

export interface SelectableTagsInput {
    /** 全库标签名（来自 `get_all_tags_info`，含尚无条目的）。 */
    savedTagNames: readonly string[];
    /** 当前已加载历史里出现过的标签名（可能含 `savedTagNames` 里已删掉的名字）。 */
    historyTags: readonly string[];
    /** 页面此刻是否需要这份标签池。为假时返回空数组。 */
    needed: boolean;
}

/**
 * 收集可选标签名，去重并按名称排序。
 *
 * 两个来源都保留，各自解决一个真实场景：
 * - `savedTagNames`：**主**来源。覆盖"用户刚建好、还没用过"的标签。
 * - `historyTags`：**补**来源。覆盖"`saved_tags` 行已被删掉、但条目上仍留着该名字"
 *   的情况 —— 那仍然是这条记录的真实标签，不该在选择器里消失。
 *
 * 排序用 `localeCompare`（与项目其它地方一致），保证中文按拼音、英文按字典序，
 * 且结果稳定 —— 列表顺序不会因为底层数据顺序变化而跳动。
 */
export function collectSelectableTags({
    savedTagNames,
    historyTags,
    needed,
}: SelectableTagsInput): string[] {
    if (!needed) return [];

    const set = new Set<string>();
    for (const name of savedTagNames) {
        if (name) set.add(name);
    }
    for (const name of historyTags) {
        if (name) set.add(name);
    }

    return Array.from(set).sort((a, b) => a.localeCompare(b));
}
