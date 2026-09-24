// @vitest-environment node
import { describe, it, expect } from "vitest";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";
import ts from "typescript";

/**
 * 悬浮说明守卫：**只有图标、没有文字的按钮，必须带 `title` 或 `aria-label`**。
 *
 * # 为什么判据必须解析 JSX，而不是"文件里有没有出现 title="
 *
 * 「文件里出现了 `title=`」是最容易写出来的弱判据，而且它**几乎恒真**：
 * 一个文件里只要有任何一处 `title`（哪怕是别处无关元素上的说明），
 * 整条断言就会通过——哪怕同一文件里新加了一个完全没说明的图标按钮。
 * 本仓库的 tooltip 机制只有原生 `title` 一种，「新加了个没说明的图标按钮」
 * 正是这次要防的回归。
 *
 * 因此这里用 TypeScript 编译器 API 把每个 `.tsx` 解析成 AST，逐元素判定：
 *
 *   1. 这个元素**可交互**吗？
 *      （`<button>`/`<a>`/`<summary>`，或带 `onClick`/`onPointerDown`/`onMouseDown`，
 *        或 `role="menuitem|button|tab|option"`）
 *   2. 它的可访问名字是否**只来自图标**？
 *      （子树里出现 lucide 图标组件或 `img`/`svg`/`canvas`，但没有可见文本节点，
 *        也没有嵌套的可交互子元素——嵌套的那些由它们自己接受检查）
 *   3. 若成立，它必须带 `title` 或 `aria-label`。
 *
 * 这样判据既不会因为"文件里别处有 title"而放行，也不会对带文字的按钮误报。
 *
 * # 明确不检查什么（避免把数字做大到失真）
 *
 * - `<input type="checkbox">` 开关：旁边的 `<label>` 已说明用途
 * - `<option>`：下拉项本身就是文案
 * - 纯信息悬浮（如文件缩略图上的完整路径）：它们是**读取点**，不是交互点
 * - 遮罩层 `.modal-overlay`：点它等于关闭，属于"点空白处"的通用惯例
 * - `aria-hidden="true"` 的元素：对辅助技术本就不存在
 */

/** 从本文件向上找到仓库根（含 package.json 的目录）。 */
const findRoot = (): string => {
    let dir = path.dirname(url.fileURLToPath(import.meta.url));
    for (let i = 0; i < 8; i += 1) {
        if (fs.existsSync(path.join(dir, "package.json"))) return dir;
        dir = path.dirname(dir);
    }
    throw new Error("找不到仓库根目录");
};

const ROOT = findRoot();

/** 全部待检查的源码文件（排除测试自身）。 */
const sourceFiles = (): string[] => {
    const out: string[] = [];
    const walk = (dir: string) => {
        for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
            const full = path.join(dir, entry.name);
            if (entry.isDirectory()) walk(full);
            else if (entry.name.endsWith(".tsx") && !entry.name.endsWith(".test.tsx")) out.push(full);
        }
    };
    walk(path.join(ROOT, "src"));
    return out;
};

const hasAttr = (opening: ts.JsxOpeningLikeElement, name: string): boolean =>
    opening.attributes.properties.some(
        (a) => ts.isJsxAttribute(a) && a.name.getText() === name
    );

const attrText = (opening: ts.JsxOpeningLikeElement, name: string): string | null => {
    const attr = opening.attributes.properties.find(
        (a): a is ts.JsxAttribute => ts.isJsxAttribute(a) && a.name.getText() === name
    );
    if (!attr) return null;
    return attr.initializer ? attr.initializer.getText() : "";
};

interface Hit {
    file: string;
    line: number;
    tag: string;
    icons: string[];
    snippet: string;
}

interface Acc {
    /** 子树里出现了可见文本 */
    text: boolean;
    icons: string[];
    /** 子树里嵌套了另一个可交互元素（由它自己接受检查） */
    nestedInteractive: number;
    /** 当前统计的根节点，用于排除根自身 */
    root: ts.JsxElement;
}

/** 扫描一个文件，返回其中"含图标、无可见文本、无嵌套交互"的交互元素（含已有说明的）。 */
const scanFile = (file: string): { all: Hit[]; gaps: Hit[] } => {
    const source = fs.readFileSync(file, "utf8");
    const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);

    // 只把该文件**实际从 lucide-react 引入**的组件名当作图标，
    // 而不是"首字母大写就算图标"——后者会把 `LabelWithHint`、`AppSelector`
    // 这类普通组件误判成图标，产生大量假阳性。
    const iconNames = new Set<string>();
    sf.forEachChild((node) => {
        if (!ts.isImportDeclaration(node)) return;
        if (!ts.isStringLiteral(node.moduleSpecifier)) return;
        if (node.moduleSpecifier.text !== "lucide-react") return;
        const bindings = node.importClause?.namedBindings;
        if (!bindings || !ts.isNamedImports(bindings)) return;
        for (const el of bindings.elements) iconNames.add(el.name.getText());
    });

    const all: Hit[] = [];
    const gaps: Hit[] = [];

    const isInteractive = (opening: ts.JsxOpeningLikeElement): boolean => {        const tag = opening.tagName.getText();
        if (/^(button|a|summary)$/.test(tag) || /button$/i.test(tag)) return true;
        if (
            hasAttr(opening, "onClick") ||
            hasAttr(opening, "onPointerDown") ||
            hasAttr(opening, "onMouseDown")
        ) {
            return true;
        }
        return (
            hasAttr(opening, "role") &&
            /menuitem|button|tab|option/.test(attrText(opening, "role") ?? "")
        );
    };

    /** 表达式位（`{...}`）是否渲染出文本。 */
    const exprIsText = (expr: ts.Expression, acc: Acc): void => {
        if (ts.isJsxElement(expr) || ts.isJsxSelfClosingElement(expr)) {
            jsx(expr, acc);
            return;
        }
        if (ts.isParenthesizedExpression(expr)) {
            exprIsText(expr.expression, acc);
            return;
        }
        if (ts.isConditionalExpression(expr)) {
            exprIsText(expr.whenTrue, acc);
            exprIsText(expr.whenFalse, acc);
            return;
        }
        if (ts.isBinaryExpression(expr)) {
            const kind = expr.operatorToken.kind;
            if (
                kind === ts.SyntaxKind.BarBarToken ||
                kind === ts.SyntaxKind.QuestionQuestionToken ||
                kind === ts.SyntaxKind.PlusToken
            ) {
                exprIsText(expr.left, acc);
                exprIsText(expr.right, acc);
            }
            // 比较 / 逻辑运算不是文案
            return;
        }
        if (ts.isCallExpression(expr)) {
            for (const arg of expr.arguments) exprIsText(arg, acc);
            acc.text = true;
            return;
        }
        if (ts.isArrowFunction(expr) || ts.isFunctionExpression(expr)) return;
        const raw = expr.getText().replace(/\s+/g, "");
        if (!raw || /^(!|false$|null$|undefined$)/.test(raw)) return;
        // 标识符 / 成员访问 / 字符串 / 模板 —— 都会渲染出文本
        acc.text = true;
    };

    const jsx = (node: ts.JsxElement | ts.JsxSelfClosingElement, acc: Acc): void => {
        const opening: ts.JsxOpeningLikeElement = ts.isJsxElement(node) ? node.openingElement : node;
        const tag = opening.tagName.getText();
        if (iconNames.has(tag) || /^(img|svg|canvas)$/.test(tag)) acc.icons.push(tag);
        if (isInteractive(opening) && ts.isJsxElement(node) && node !== acc.root) {
            acc.nestedInteractive += 1;
        }
        if (ts.isJsxElement(node)) {
            for (const child of node.children) {
                if (ts.isJsxText(child)) {
                    if (child.getText().replace(/\s+/g, "").length > 0) acc.text = true;
                } else if (ts.isJsxExpression(child)) {
                    if (child.expression) exprIsText(child.expression, acc);
                } else if (ts.isJsxElement(child) || ts.isJsxSelfClosingElement(child)) {
                    jsx(child, acc);
                }
            }
        }
    };

    const visit = (node: ts.Node): void => {
        if (ts.isJsxElement(node)) {
            const opening = node.openingElement;
            if (isInteractive(opening)) {
                const acc: Acc = { text: false, icons: [], nestedInteractive: 0, root: node };
                jsx(node, acc);
                const className = attrText(opening, "className") ?? "";
                const isOverlay = /overlay/.test(className);
                const isHidden = attrText(opening, "aria-hidden") === "true";
                const named =
                    attrText(opening, "title") !== null || attrText(opening, "aria-label") !== null;
                if (
                    acc.icons.length > 0 &&
                    !acc.text &&
                    acc.nestedInteractive === 0 &&
                    !isOverlay &&
                    !isHidden
                ) {
                    const pos = sf.getLineAndCharacterOfPosition(opening.getStart());
                    const hit: Hit = {
                        file: path.relative(ROOT, file),
                        line: pos.line + 1,
                        tag: opening.tagName.getText(),
                        icons: acc.icons,
                        snippet: opening.getText().slice(0, 90).replace(/\s+/g, " "),
                    };
                    all.push(hit);
                    if (!named) gaps.push(hit);
                }
            }
        }
        ts.forEachChild(node, visit);
    };
    visit(sf);
    return { all, gaps };
};

const scanAll = () => sourceFiles().map((f) => ({ f, ...scanFile(f) }));

describe("悬浮说明守卫：图标按钮必须有 title 或可见文字", () => {
    it("扫描器真的在工作（否则下面的断言会恒真）", () => {
        const files = sourceFiles();
        expect(files.length).toBeGreaterThan(30);
        const all = scanAll().flatMap((r) => r.all);
        // 仓库里本来就存在大量"带说明的图标按钮"；数量为 0 说明解析器坏了。
        expect(all.length).toBeGreaterThan(20);
    });

    it("没有任何「只有图标、没有文字、且无 title/aria-label」的交互元素", () => {
        const gaps = scanAll().flatMap((r) => r.gaps);
        const report = gaps
            .map((h) => `  ${h.file}:${h.line} <${h.tag}> [${h.icons.join(", ")}] ${h.snippet}`)
            .join("\n");
        expect(
            gaps,
            `以下交互元素只有图标、没有可见文字，也没有 title/aria-label，` +
                `鼠标悬浮时用户无从知道它是什么：\n${report}\n\n` +
                `修法：加 title={t('...')}（词条写进 src/locales.ts 的三语），` +
                `或给按钮加上可见文字。`
        ).toEqual([]);
    });
});

describe("悬浮说明文案必须是三语齐备的 i18n 键", () => {
    it("由 title 暴露的文案不得写死成字符串字面量", () => {
        // 防止"补了 tooltip 但不随语言切换"的半吊子修法：
        // `title="Delete"` 在英文界面正确，中文界面就成了英文。
        const offenders: string[] = [];
        for (const file of sourceFiles()) {
            const source = fs.readFileSync(file, "utf8");
            const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
            const visit = (node: ts.Node): void => {
                if (ts.isJsxAttribute(node) && node.name.getText() === "title" && node.initializer) {
                    const text = node.initializer.getText().trim();
                    // 裸字符串字面量就是写死；`{...}` 表达式放行（必须有 t()，
                    // 由 typecheck 与下面的 key 存在性检查兜底）。
                    if (/^["']/.test(text)) {
                        const pos = sf.getLineAndCharacterOfPosition(node.getStart());
                        offenders.push(`${path.relative(ROOT, file)}:${pos.line + 1}  title=${text}`);
                    }
                }
                ts.forEachChild(node, visit);
            };
            visit(sf);
        }
        expect(
            offenders,
            `以下 title 写死成字符串，不随语言切换（应改为 title={t('...')}）：\n${offenders.join("\n")}`
        ).toEqual([]);
    });
});

describe("新增 tooltip 词条的三语一致性", () => {
    it("每个 tooltip_* 键在三种语言里都存在且不是键名本身", async () => {
        const { translations } = await import("../../../locales");
        const langs = ["zh", "en", "tw"] as const;
        const dict = (lang: (typeof langs)[number]) =>
            translations[lang] as unknown as Record<string, string>;
        const keys = Object.keys(dict("zh")).filter((k) => k.startsWith("tooltip_"));
        expect(keys.length).toBeGreaterThan(0);
        for (const key of keys) {
            for (const lang of langs) {
                const value = dict(lang)[key];
                expect(value, `${lang} 缺 ${key}`).toBeTruthy();
                // `t()` 查不到时返回键名本身；等于键名等于没翻译。
                expect(value, `${lang} 的 ${key} 未翻译`).not.toBe(key);
            }
        }
    });
});
