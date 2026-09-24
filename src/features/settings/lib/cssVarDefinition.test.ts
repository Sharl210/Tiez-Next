// @vitest-environment node
import { describe, it, expect, beforeAll } from "vitest";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

/**
 * 「使用了从未定义的 CSS 变量」的回归防线。
 *
 * # 为什么这个缺陷能长期存在
 *
 * 浏览器对 `var()` 解析失败是**静默**的：变量取不到值时，整条 CSS 声明被丢弃，
 * 而构建、类型检查、单元测试、DOM 断言全部照常通过。界面只是"少了一条边框/
 * 一块底色"，看起来很正常。本仓库因此积累过 38 个这类变量（例如数字输入框
 * 实测 border 为 0px、对端头像始终透明）。
 *
 * 所以防线必须是"扫描源码里 var() 的使用与定义之差"，而不是"渲染后截图对比"——
 * 截图对比看不出"本该有的边框没出现"。
 *
 * `tools/check-css-vars.mjs` 是同一个检查的独立可执行版本（`npm run lint:css-vars`），
 * 这里再包一层测试，让 `vitest run` 也能拦住它，不必依赖谁记得手动跑脚本。
 */

// 本文件位于 <repo>/src/features/settings/lib/，仓库根是它的四级上级。
const ROOT = path.resolve(path.dirname(url.fileURLToPath(import.meta.url)), "../../../..");
const SCRIPT = path.join(ROOT, "tools/check-css-vars.mjs");

/** 检查脚本的 JSON 输出形状（只声明本测试断言到的字段）。 */
interface CssVarReport {
  missing: Array<{ name: string; sites: string[]; silent?: boolean }>;
  allowed: Array<{ name: string; reason: string; sites: string[] }>;
  illegalAllow: Array<{ name: string; reason: string; sites: string[] }>;
  staleAllow: string[];
}

let result: { code: number; json: CssVarReport };
beforeAll(() => {
  let stdout = "";
  let code = 0;
  try {
    stdout = execFileSync(process.execPath, [SCRIPT, "--json"], { cwd: ROOT, encoding: "utf8" });
  } catch (e) {
    // 脚本以非 0 退出时 execFileSync 抛错，但 stdout 仍然拿得到
    stdout = String((e as { stdout?: string }).stdout ?? "");
    code = (e as { status?: number }).status ?? 1;
  }
  result = { code, json: JSON.parse(stdout) as CssVarReport };
});

describe("CSS 变量定义完整性", () => {
  it("不存在被 var() 引用却从未定义的变量", () => {
    expect(result.json.missing).toEqual([]);
  });

  it("白名单里没有「无 fallback」的使用点（那类会静默丢弃整条声明）", () => {
    expect(result.json.illegalAllow).toEqual([]);
  });

  it("白名单里没有已经不再被引用的过期条目", () => {
    expect(result.json.staleAllow).toEqual([]);
  });

  it("检查脚本本身会因缺陷而失败（防「永远返回成功」的空检查）", () => {
    // 反向自检：往一个临时 CSS 里塞一个未定义变量，脚本必须报出来。
    // 若脚本被改成恒真，这个断言会失败。
    //
    // 探针文本用拼接构造、不写字面量 `var(--…)`：否则这个测试文件自己就会成为
    // 一个「引用了未定义变量」的站点，让上一条断言恒假——那正是本测试要防的
    // 「检查自己把自己绊倒」。
    const probe = path.join(ROOT, "src", "__cssvar_probe__.css");
    const probeText = ":root { color: " + ["va", "r("].join("") + "--definitely-not-defined-var); }\n";
    fs.writeFileSync(probe, probeText);
    let failed = false;
    try {
      execFileSync(process.execPath, [SCRIPT], { cwd: ROOT, encoding: "utf8" });
    } catch {
      failed = true;
    } finally {
      fs.unlinkSync(probe);
    }
    expect(failed).toBe(true);
  });
});
