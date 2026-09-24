// @vitest-environment node
import { describe, it, expect } from "vitest";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

/**
 * 跨层契约：**前端 `invoke` 的每个命令，后端都要有定义、并且真的注册进 handler**。
 *
 * # 为什么必须有这条防线
 *
 * 前端调一个后端没有的命令时，UI 侧的表现是"点了没反应"或一条静默的 console 报错，
 * 而所有现有测试都是绿的：组件测试把 `invoke` 桩掉了，Rust 单测碰不到前端。
 * 本仓库真实存在过 4 条这样的命令（`restore_last_focus`／`save_temp_image`／
 * `set_display_ip`／`stop_cloud_sync_client`／`set_quick_paste_modifier`／`set_dock_visible`），
 * 其中 `restore_last_focus` 是**搜索框每次失焦都会调**的一条。这类缺陷只有"把两侧源码
 * 放在一起比对"才能机械检出。
 *
 * # 三个必须处理的坑（否则测试自己会骗自己）
 *
 * 1. **多行与泛型**：`invoke<{...}>(\n  "restore_auto_backup",\n  {...})` 与
 *    `invoke<Array<{...}>>("get_tag_stats")` 都真实存在。用单行正则
 *    `/invoke\("(\w+)"/` 会把它们全漏掉 ⇒ **已齐全的命令被误报成缺失**（假阳性）。
 *    本文件的做法：跳过 `invoke` 之后的可选泛型（做尖括号配平），再跳过空白，
 *    才取字符串字面量——**允许跨行**。
 * 2. **注释**：`generate_handler!` 里每个命令上方都有中文说明注释。若按 `,` 切分后
 *    直接取最后一段，注释会与命令名粘在一起被过滤掉 ⇒ **已注册的命令被判为未注册**。
 *    Rust 与 TS 两侧都必须先剥注释。
 * 3. **`#[command]` 是 `#[tauri::command]` 的导入别名**：`ai_cmd.rs` 用的是
 *    `use tauri::command;` + `#[command]`。只认 `#[tauri::command]` 会让这两条命令
 *    被判为"不存在"。
 *
 * # 自证没有假阳性（本文件的第二条主线）
 *
 * 光断言"差集为空"是不够的——**差集为空也可能是因为我什么都没扫到**。
 * 所以另外锁三组正例：已知齐全的命令必须出现在扫描结果里。它们分别覆盖
 * 多行调用、泛型嵌套、`#[command]` 别名三种写法。任一写法解析失败，正例先红。
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
const TS_SRC = path.join(ROOT, "src");
const RUST_SRC = path.join(ROOT, "src-tauri", "src");

/**
 * 剥掉注释，**保留字符串字面量的内容**。
 *
 * 为什么不能用那种"先删块注释再删行注释"的两行正则：它会吃掉字符串里的 `//`
 * （例如 URL、注释风格的文案），把后面的代码并进字符串里，解析随之错位。
 * 这里按字符扫描并跟踪引号状态，只把注释区间替换成等长空白（保留换行与列偏移）。
 */
const stripComments = (text: string): string => {
  let out = "";
  let i = 0;
  let state: "code" | "line" | "block" | "sq" | "dq" | "bt" = "code";

  while (i < text.length) {
    const c = text[i];
    const n = text[i + 1];

    if (state === "code") {
      if (c === "/" && n === "/") { state = "line"; out += "  "; i += 2; continue; }
      if (c === "/" && n === "*") { state = "block"; out += "  "; i += 2; continue; }
      if (c === "'") { state = "sq"; out += c; i += 1; continue; }
      if (c === '"') { state = "dq"; out += c; i += 1; continue; }
      if (c === "`") { state = "bt"; out += c; i += 1; continue; }
      out += c; i += 1; continue;
    }

    if (state === "line") {
      if (c === "\n") { state = "code"; out += c; } else out += " ";
      i += 1; continue;
    }

    if (state === "block") {
      if (c === "*" && n === "/") { state = "code"; out += "  "; i += 2; continue; }
      out += c === "\n" ? "\n" : " ";
      i += 1; continue;
    }

    // 字符串内部：原样保留，处理转义
    out += c;
    if (c === "\\") { out += text[i + 1] ?? ""; i += 2; continue; }
    if ((state === "sq" && c === "'") || (state === "dq" && c === '"') || (state === "bt" && c === "`")) {
      state = "code";
    }
    i += 1;
  }

  return out;
};

const walk = (dir: string, filter: (name: string) => boolean): string[] => {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(full, filter));
    else if (filter(entry.name)) out.push(full);
  }
  return out;
};

/**
 * 跳过一段可能嵌套的 `<...>`（泛型参数），返回其后的下标；不是泛型则原样返回。
 *
 * 真实写法里泛型本身可以很复杂：`invoke<Array<{ name: string; ... }>>("get_tag_stats")`
 * 与 `invoke<{ restoreReport?: { restartRequired?: boolean } }>(\n "restore_auto_backup")`。
 * 因此**不能**一见到 `{` 或 `;` 就放弃——那两种字符都在对象类型字面量内部合法存在。
 * 只有"花括号之外出现的 `;`"才说明这不是类型参数（而是语句结束），此时判定为非泛型。
 */
const skipGenerics = (text: string, start: number): number => {
  if (text[start] !== "<") return start;
  let angle = 0;
  let brace = 0;
  for (let i = start; i < text.length; i += 1) {
    const c = text[i];
    if (c === "{") { brace += 1; continue; }
    if (c === "}") { brace -= 1; continue; }
    // `=>`（函数类型）里的 `>` 不是泛型闭合
    if (c === ">" && text[i - 1] === "=") continue;
    if (c === "<") { angle += 1; continue; }
    if (c === ">") {
      angle -= 1;
      if (angle === 0) return i + 1;
      continue;
    }
    // 花括号之外的 `;` 属于语句层：这不是泛型参数
    if (c === ";" && brace <= 0) return start;
    if (c === "\n" && brace <= 0 && angle <= 0) return start;
  }
  return start;
};

const lineOf = (text: string, index: number): number =>
  text.slice(0, index).split("\n").length;

export interface TsInvokeScan {
  /** 命令名 → 出现位置（`相对路径:行号`）。 */
  literal: Map<string, string[]>;
  /** 目标不是字符串字面量的调用点（模板串或三元表达式），需要人工确认。 */
  dynamic: string[];
}

/**
 * 扫描 TS 侧 `invoke(...)`。
 *
 * 只取**字面量**目标；三元/模板串记进 `dynamic` 交给用例断言，
 * 不静默跳过——否则"新加一个动态调用"会绕过整条防线。
 */
export const scanTsInvokes = (files: string[]): TsInvokeScan => {
  const literal = new Map<string, string[]>();
  const dynamic: string[] = [];

  for (const file of files) {
    const raw = fs.readFileSync(file, "utf8");
    const text = stripComments(raw);
    const rel = path.relative(ROOT, file).split(path.sep).join("/");

    // `invoke` 之后允许出现空白、可选泛型、再是 `(`
    const re = /\binvoke\b/g;
    let m: RegExpExecArray | null;
    while ((m = re.exec(text)) !== null) {
      let i = m.index + "invoke".length;
      while (i < text.length && /\s/.test(text[i])) i += 1;
      if (text[i] === "<") {
        const after = skipGenerics(text, i);
        if (after === i) continue; // 不是泛型，按普通调用处理
        i = after;
        while (i < text.length && /\s/.test(text[i])) i += 1;
      }
      if (text[i] !== "(") continue;
      i += 1;
      while (i < text.length && /\s/.test(text[i])) i += 1;

      const quote = text[i];
      const where = `${rel}:${lineOf(raw, m.index)}`;
      if (quote === '"' || quote === "'") {
        let j = i + 1;
        let value = "";
        while (j < text.length && text[j] !== quote) {
          if (text[j] === "\\") { value += text[j + 1] ?? ""; j += 2; continue; }
          value += text[j];
          j += 1;
        }
        if (!literal.has(value)) literal.set(value, []);
        (literal.get(value) as string[]).push(where);
      } else if (quote === "`") {
        let j = i + 1;
        let value = "";
        while (j < text.length && text[j] !== "`") { value += text[j]; j += 1; }
        dynamic.push(`模板串 ${JSON.stringify(value)} @ ${where}`);
      } else {
        dynamic.push(`非字面量 @ ${where} :: ${raw.slice(m.index, m.index + 70).split("\n")[0]}`);
      }
    }
  }

  return { literal, dynamic };
};

export interface RustCommandScan {
  /** 命令名 → 定义位置。 */
  defined: Map<string, string>;
  /** `generate_handler!` 里登记过的命令名集合（收集全部出现处，不限于 main.rs）。 */
  registered: Set<string>;
}

/**
 * 扫描 Rust 侧的命令定义与注册。
 *
 * 定义识别两种属性写法：`#[tauri::command]` 与 `#[command]`（后者需要文件顶部
 * `use tauri::command;`，`ai_cmd.rs` 就是这么写的）。属性与 `fn` 之间允许有
 * `pub`／`async` 以及其它属性行。
 */
export const scanRustCommands = (files: string[]): RustCommandScan => {
  const defined = new Map<string, string>();
  const registered = new Set<string>();

  for (const file of files) {
    const raw = fs.readFileSync(file, "utf8");
    const text = stripComments(raw);
    const rel = path.relative(ROOT, file).split(path.sep).join("/");
    const lines = text.split("\n");

    for (let i = 0; i < lines.length; i += 1) {
      if (!/^\s*#\[(?:tauri::command|command)\b/.test(lines[i])) continue;
      for (let k = i + 1; k < Math.min(i + 14, lines.length); k += 1) {
        const mm = /^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)/.exec(lines[k]);
        if (mm) {
          defined.set(mm[1], `${rel}:${k + 1}`);
          break;
        }
        if (/^\s*(?:pub\s+)?(?:async\s+)?fn\b/.test(lines[k])) break;
      }
    }

    // generate_handler! 的方括号内容：按括号配平取段，注释已在上面剥掉
    let from = 0;
    for (;;) {
      const idx = text.indexOf("generate_handler!", from);
      if (idx < 0) break;
      const open = text.indexOf("[", idx);
      if (open < 0) break;
      let depth = 0;
      let end = open;
      for (; end < text.length; end += 1) {
        if (text[end] === "[") depth += 1;
        else if (text[end] === "]") {
          depth -= 1;
          if (depth === 0) break;
        }
      }
      const body = text.slice(open + 1, end);
      for (const item of body.split(",")) {
        const trimmed = item.trim();
        if (!trimmed) continue;
        const name = trimmed.split("::").pop()?.trim() ?? "";
        if (/^[A-Za-z0-9_]+$/.test(name)) registered.add(name);
      }
      from = end + 1;
    }
  }

  return { defined, registered };
};

const tsFiles = walk(TS_SRC, (n) => /\.tsx?$/.test(n) && !/\.test\.tsx?$/.test(n));
const rustFiles = walk(RUST_SRC, (n) => n.endsWith(".rs"));

const ts = scanTsInvokes(tsFiles);
const rust = scanRustCommands(rustFiles);

/**
 * 已知齐全的命令——三种"难解析写法"各至少一条。
 *
 * 这三条是**先证伪点**：如果解析器把多行调用、嵌套泛型或 `#[command]` 别名漏掉，
 * 下面的差集断言会"因为差集为空"而假绿，正例则会立刻变红。
 */
const KNOWN_COMPLETE: Array<{ name: string; why: string }> = [
  {
    name: "restore_auto_backup",
    why: "调用跨行：`invoke<{...}>(\n  \"restore_auto_backup\",\n  {...}`",
  },
  {
    name: "get_tag_stats",
    why: "泛型里含对象类型：`invoke<Array<{ name: string; ... }>>(\"get_tag_stats\")`",
  },
  {
    name: "call_ai",
    why: "定义用的是 `#[command]` 别名（`ai_cmd.rs` 顶部 `use tauri::command;`）",
  },
  {
    name: "set_display_ip",
    why: "注册项上方紧跟中文注释，且注释里出现逗号",
  },
];

/**
 * 允许"前端调了但后端不是自定义命令"的例外。
 *
 * `plugin:app|version` 是 Tauri 核心 app 插件的命令（`core:app:allow-version`），
 * 由 `tauri::app::plugin` 注册，不出现在本仓库的 `generate_handler!` 里。
 */
const PLUGIN_COMMAND_ALLOWLIST = new Set(["plugin:app|version"]);

const isPluginCommand = (name: string): boolean => name.includes(":");
const isAllowedPlugin = (name: string): boolean => PLUGIN_COMMAND_ALLOWLIST.has(name);

describe("跨层契约：TS invoke 目标 → Rust 定义", () => {
  it("**扫描器先自证**：已知齐全的命令都被扫到（不是空集合上的恒真断言）", () => {
    // 先证明扫描确实有产出，否则下面的差集断言毫无意义
    expect(ts.literal.size).toBeGreaterThan(100);
    expect(rust.defined.size).toBeGreaterThan(100);
    expect(rust.registered.size).toBeGreaterThan(100);

    for (const { name, why } of KNOWN_COMPLETE) {
      expect(
        ts.literal.has(name),
        `TS 侧没扫到 ${name}（${why}）——说明解析漏了这类写法`
      ).toBe(true);
      expect(
        rust.defined.has(name),
        `Rust 侧没扫到 ${name} 的定义（${why}）——说明定义解析漏了这类写法`
      ).toBe(true);
      expect(
        rust.registered.has(name),
        `Rust 侧没扫到 ${name} 的注册（${why}）——说明注册解析漏了这类写法`
      ).toBe(true);
    }
  });

  it("每个前端调用的自定义命令都有对应的 Rust 命令定义", () => {
    const missing: string[] = [];
    for (const [name, where] of ts.literal) {
      if (isPluginCommand(name)) continue;
      if (!rust.defined.has(name)) missing.push(`${name} @ ${where.join(", ")}`);
    }
    expect(missing, `前端调用了后端不存在的命令：\n${missing.join("\n")}`).toEqual([]);
  });

  it("每个前端调用的自定义命令都已注册进 invoke_handler", () => {
    const missing: string[] = [];
    for (const [name, where] of ts.literal) {
      if (isPluginCommand(name) || isAllowedPlugin(name)) continue;
      if (!rust.registered.has(name)) {
        missing.push(`${name}（定义在 ${rust.defined.get(name) ?? "?"}）@ ${where.join(", ")}`);
      }
    }
    expect(
      missing,
      `前端调用了没注册的命令（点了会静默失败）：\n${missing.join("\n")}`
    ).toEqual([]);
  });

  it("注册表里的每一条都能找到定义（防拼错/防模块路径失效）", () => {
    // `generate_handler!` 里的名字写错时 Rust 编译会失败，但那是"编译期才发现"；
    // 这条让它在测试里先暴露，并且顺带保证 `#[tauri::command]` 属性没被误删。
    const orphan = [...rust.registered].filter((n) => !rust.defined.has(n));
    expect(orphan, `已注册但找不到 #[tauri::command] 定义：${orphan.join(", ")}`).toEqual([]);
  });

  it("插件命令只允许出现在白名单里", () => {
    const plugins = [...ts.literal.keys()].filter(isPluginCommand).sort();
    const unexpected = plugins.filter((n) => !isAllowedPlugin(n));
    expect(
      unexpected,
      `出现新的插件命令，请确认它由哪个插件提供并登记白名单：${unexpected.join(", ")}`
    ).toEqual([]);
  });

  it("动态目标的调用点与已登记清单一致（新动态调用必须显式确认）", () => {
    // 这两处不是字符串字面量，静态解析拿不到最终命令名；但它们**都是可解析的**：
    // 三元表达式的两个分支都是字面量，模板串来自一个导出的常量。
    // 这里锁"动态点的数量与位置"，避免以后新增动态调用被静默放过。
    expect(ts.dynamic.length).toBe(2);
    expect(ts.dynamic.join("\n")).toContain("TagAssignMenu.tsx");
    expect(ts.dynamic.join("\n")).toContain("useMigrationProgress.ts");

    // 三元表达式的两个分支必须真的存在，且已注册
    for (const name of ["move_entry_to_tag", "copy_entry_to_tag"]) {
      expect(rust.registered.has(name), `${name} 未注册`).toBe(true);
    }
    // 模板串引用的常量值指向的也是真实命令
    const progressLib = fs.readFileSync(
      path.join(TS_SRC, "features/settings/lib/migrationProgress.ts"),
      "utf8"
    );
    const constMatch = /COMMAND_MIGRATION_PROGRESS_SNAPSHOT\s*=\s*"([^"]+)"/.exec(progressLib);
    expect(constMatch, "常量 COMMAND_MIGRATION_PROGRESS_SNAPSHOT 应保持为字符串字面量").not.toBeNull();
    expect(rust.registered.has((constMatch as RegExpExecArray)[1])).toBe(true);
  });
});

describe("跨层契约：本次修复的六条命令", () => {
  it.each([
    "restore_last_focus",
    "save_temp_image",
    "set_display_ip",
    "get_download_url",
    "stop_cloud_sync_client",
    "set_quick_paste_modifier",
    "set_dock_visible",
  ])("%s 既有定义也已注册", (name) => {
    expect(rust.defined.has(name), `${name} 缺 #[tauri::command] 定义`).toBe(true);
    expect(rust.registered.has(name), `${name} 未注册进 invoke_handler`).toBe(true);
    expect(ts.literal.has(name), `${name} 已无前端调用点，请同步删除白名单`).toBe(true);
  });
});
