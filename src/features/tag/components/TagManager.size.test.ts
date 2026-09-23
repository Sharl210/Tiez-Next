import { describe, it, expect } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  parseTagManagerSidebarSize,
  applyCollapseToggle,
  resolveTagManagerLayout,
  resolveInitialTagManagerLayout,
  isStackedViewport,
  COLLAPSE_THRESHOLD_PX,
  EXPANDED_SIDEBAR_WIDTH,
  shouldShowTag,
  isSensitiveFeatureEnabled,
} from "./TagManager";
import TagManager from "./TagManager";

/**
 * R2: the geometry parser is the one piece of the sidebar-persistence feature that
 * is pure logic, so it carries the guarantees the UI depends on:
 *
 *  - a corrupt or absent value must fall back to the defaults instead of producing
 *    `NaN` / negative / absurd sizes that would break the layout;
 *  - the wide and stacked layouts each keep their own numbers;
 *  - `collapsed` is only ever true when the stored value is literally `true`.
 *
 * The component module is imported for its exported helpers, and for the
 * first-frame tests below it is rendered once to static markup (no DOM, no Tauri
 * host required: the remembered geometry arrives as a prop, which is the whole
 * point of the fix).
 */
const DEFAULTS = {
  width: 130,
  height: 180,
  collapsed: false,
  stackedWidth: 130,
  stackedHeight: 180,
  stackedCollapsed: false,
};

describe("parseTagManagerSidebarSize", () => {
  describe("回退：非法/损坏/缺失输入", () => {
    it("缺失或空串回退默认值", () => {
      for (const raw of [undefined, null, "", "   "]) {
        expect(parseTagManagerSidebarSize(raw)).toEqual(DEFAULTS);
      }
    });

    it("截断的 JSON 不抛异常且回退默认值", () => {
      for (const raw of ["{", '{"width":', '{"width":200', "not json at all", "[]"]) {
        expect(() => parseTagManagerSidebarSize(raw)).not.toThrow();
        expect(parseTagManagerSidebarSize(raw)).toEqual(DEFAULTS);
      }
    });

    it("非对象类型回退默认值", () => {
      for (const raw of [42, true, "null", "123", '"a string"']) {
        expect(parseTagManagerSidebarSize(raw)).toEqual(DEFAULTS);
      }
    });

    it("NaN / Infinity / 负值 / 越界值回退默认值", () => {
      const cases = [
        { width: Number.NaN },
        { width: Number.POSITIVE_INFINITY },
        { width: -10 },
        { width: 0 },
        { width: 7 }, // 低于最小宽度
        { width: 99999 }, // 高于最大宽度
        { height: Number.NaN },
        { height: 10 }, // 低于最小高度
        { height: 99_999 },
      ];
      for (const partial of cases) {
        const parsed = parseTagManagerSidebarSize(partial);
        expect(Number.isFinite(parsed.width)).toBe(true);
        expect(Number.isFinite(parsed.height)).toBe(true);
        expect(parsed.width).toBeGreaterThanOrEqual(48);
        expect(parsed.width).toBeLessThanOrEqual(320);
        expect(parsed.height).toBeGreaterThanOrEqual(120);
      }
    });

    it("字符串数字不被接受（存储类型必须正确）", () => {
      expect(parseTagManagerSidebarSize({ width: "200" })).toEqual(DEFAULTS);
    });

    it("单个字段损坏只重置该字段，其余保留", () => {
      const parsed = parseTagManagerSidebarSize({
        width: Number.NaN,
        height: 250,
        collapsed: true,
      });
      expect(parsed.width).toBe(DEFAULTS.width);
      expect(parsed.height).toBe(250);
      expect(parsed.collapsed).toBe(true);
    });
  });

  describe("正常读取", () => {
    it("从 JSON 字符串还原横竖两套尺寸与折叠态", () => {
      const parsed = parseTagManagerSidebarSize(
        JSON.stringify({
          width: 210,
          height: 260,
          collapsed: true,
          stackedWidth: 120,
          stackedHeight: 300,
          stackedCollapsed: false,
        })
      );
      expect(parsed).toEqual({
        width: 210,
        height: 260,
        collapsed: true,
        stackedWidth: 120,
        stackedHeight: 300,
        stackedCollapsed: false,
      });
    });

    it("也接受已解析的对象（beta 的旧值同样可读）", () => {
      // beta 只写过 width / height，没有 collapsed / stacked*。宽高必须保留，
      // 缺失的 stacked* 继承宽高，避免从 beta 升级后竖排布局被重置。
      const parsed = parseTagManagerSidebarSize({ width: 168, height: 200 });
      expect(parsed.width).toBe(168);
      expect(parsed.height).toBe(200);
      expect(parsed.collapsed).toBe(false);
      expect(parsed.stackedWidth).toBe(168);
      expect(parsed.stackedHeight).toBe(200);
      expect(parsed.stackedCollapsed).toBe(false);
    });
  });

  describe("collapsed 只认字面 true", () => {
    it("非布尔真值不会开启折叠", () => {
      for (const value of ["true", 1, "yes", {}, [], null, undefined]) {
        expect(parseTagManagerSidebarSize({ collapsed: value }).collapsed).toBe(false);
        expect(
          parseTagManagerSidebarSize({ stackedCollapsed: value }).stackedCollapsed
        ).toBe(false);
      }
    });

    it("显式 true / false 被保留", () => {
      expect(parseTagManagerSidebarSize({ collapsed: true }).collapsed).toBe(true);
      expect(parseTagManagerSidebarSize({ collapsed: false }).collapsed).toBe(false);
    });
  });

  describe("横竖分别记忆（互不污染）", () => {
    it("改竖排不影响横排数值", () => {
      const parsed = parseTagManagerSidebarSize({
        width: 130,
        height: 180,
        stackedWidth: 300,
        stackedHeight: 150,
      });
      expect(parsed.width).toBe(130);
      expect(parsed.height).toBe(180);
      expect(parsed.stackedWidth).toBe(300);
      expect(parsed.stackedHeight).toBe(150);
    });

    it("边界值被接受（最小与最大）", () => {
      const parsed = parseTagManagerSidebarSize({
        width: 48,
        height: 120,
        stackedWidth: 320,
        stackedHeight: 4000,
      });
      expect(parsed.width).toBe(48);
      expect(parsed.height).toBe(120);
      expect(parsed.stackedWidth).toBe(320);
      expect(parsed.stackedHeight).toBe(4000);
    });
  });
});

describe("applyCollapseToggle（R2 折叠态记忆）", () => {
  const stored = {
    width: 240,
    height: 200,
    collapsed: false,
    stackedWidth: 150,
    stackedHeight: 320,
    stackedCollapsed: false,
  };

  it("横排折叠：只改横排字段，竖排记忆不变", () => {
    const next = applyCollapseToggle(stored, { stacked: false, width: 240, collapsed: false });
    expect(next.collapsed).toBe(true);
    expect(next.width).toBe(240);
    // 竖排字段必须原样保留
    expect(next.stackedCollapsed).toBe(false);
    expect(next.stackedWidth).toBe(150);
    expect(next.stackedHeight).toBe(320);
  });

  it("竖排折叠：只改竖排字段，横排记忆不变", () => {
    const next = applyCollapseToggle(stored, { stacked: true, width: 150, collapsed: false });
    expect(next.stackedCollapsed).toBe(true);
    expect(next.stackedWidth).toBe(150);
    expect(next.collapsed).toBe(false);
    expect(next.width).toBe(240);
  });

  it("从折叠栏展开时恢复可用宽度而非停在 48px", () => {
    const folded = { ...stored, collapsed: true, width: 48 };
    const next = applyCollapseToggle(folded, { stacked: false, width: 48, collapsed: true });
    expect(next.collapsed).toBe(false);
    expect(next.width).toBe(EXPANDED_SIDEBAR_WIDTH);
  });

  it("宽度已达可用值时不覆盖用户原有宽度", () => {
    const next = applyCollapseToggle(stored, { stacked: false, width: 240, collapsed: true });
    expect(next.collapsed).toBe(false);
    expect(next.width).toBe(240);
  });

  it("阈值常量与展开宽度常量一致可用", () => {
    expect(COLLAPSE_THRESHOLD_PX).toBe(110);
    expect(EXPANDED_SIDEBAR_WIDTH).toBeGreaterThan(COLLAPSE_THRESHOLD_PX);
  });

  it("返回新对象，不就地修改传入的存储值", () => {
    const original = { ...stored };
    const next = applyCollapseToggle(stored, { stacked: false, width: 240, collapsed: false });
    expect(stored).toEqual(original);
    expect(next).not.toBe(stored);
  });

  it("折叠后再展开能回到原宽度（往返稳定）", () => {
    const fold = applyCollapseToggle(stored, { stacked: false, width: 240, collapsed: false });
    // 折叠后 UI 宽度会变成 48，但存储里仍是 240，展开时才能还原
    const unfold = applyCollapseToggle(fold, { stacked: false, width: fold.width, collapsed: fold.collapsed });
    expect(unfold.collapsed).toBe(false);
    expect(unfold.width).toBe(240);
  });
});

describe("shouldShowTag（R3 分组显示规则）", () => {
  it("普通分组始终显示，与开关无关", () => {
    for (const enabled of [true, false]) {
      expect(shouldShowTag({ name: "work", count: 0 }, enabled)).toBe(true);
      expect(shouldShowTag({ name: "work", count: 5 }, enabled)).toBe(true);
    }
  });

  it("内置敏感分组：有条目时无论开关如何都显示（不隐藏用户数据）", () => {
    for (const name of ["sensitive", "密码", "password", "Sensitive", "PASSWORD"]) {
      expect(shouldShowTag({ name, count: 1 }, true)).toBe(true);
      expect(shouldShowTag({ name, count: 1 }, false)).toBe(true);
    }
  });

  it("内置敏感分组：空且功能开启时显示（正常可用分组）", () => {
    expect(shouldShowTag({ name: "sensitive", count: 0 }, true)).toBe(true);
    expect(shouldShowTag({ name: "密码", count: 0 }, true)).toBe(true);
  });

  // Decision (user, 2026-09-23): `密码` / `password` are legacy names with no
  // producer anywhere in the code base, so they are no longer tied to the privacy
  // setting. Only `sensitive` — the one name the capture pipeline actually pushes —
  // follows that feature. Deleting any of them is permanent: nothing reseeds them.
  it("内置敏感分组：空且功能关闭时才隐藏（唯一的隐藏条件）", () => {
    expect(shouldShowTag({ name: "sensitive", count: 0 }, false)).toBe(false);
  });

  it("历史遗留名称不再跟随隐私开关（无产生者，按普通分组处理）", () => {
    for (const name of ["密码", "password", "PASSWORD"]) {
      expect(shouldShowTag({ name, count: 0 }, false)).toBe(true);
      expect(shouldShowTag({ name, count: 0 }, true)).toBe(true);
    }
  });

  it("相似但非内置的名称不受影响", () => {
    for (const name of ["sensitive2", "敏感", "pass word", "mypassword"]) {
      expect(shouldShowTag({ name, count: 0 }, false)).toBe(true);
    }
  });
});

describe("isSensitiveFeatureEnabled（R3 开关读取）", () => {
  it("只有字面 false 视为关闭", () => {
    expect(isSensitiveFeatureEnabled({ "app.privacy_protection": "false" })).toBe(false);
  });

  it("true / 缺失 / 空对象 / null 均视为开启", () => {
    expect(isSensitiveFeatureEnabled({ "app.privacy_protection": "true" })).toBe(true);
    expect(isSensitiveFeatureEnabled({})).toBe(true);
    expect(isSensitiveFeatureEnabled(null)).toBe(true);
    expect(isSensitiveFeatureEnabled(undefined)).toBe(true);
  });

  it("读设置失败（空值兜底）不会隐藏分组", () => {
    // fetchTags 在 get_settings 失败时传 {}，语义必须是"功能开启"，否则一次
    // 瞬时读取失败就会让敏感分组消失。
    expect(isSensitiveFeatureEnabled({})).toBe(true);
  });

  it("其它取值不被误判为关闭", () => {
    for (const value of ["FALSE", "0", "no", ""]) {
      expect(isSensitiveFeatureEnabled({ "app.privacy_protection": value })).toBe(true);
    }
  });
});

/**
 * R2（本轮修复）：首帧即记忆比例。
 *
 * 修复前的实现从默认比例起渲染，等 `get_settings` 异步返回后才换成记忆比例，
 * 于是用户看到"先默认、再跳一下"。这些用例锁住的是构造上的性质：**不执行任何
 * effect**（静态渲染不跑 effect），首帧的输出就已经是记忆比例。
 *
 * 反向对照结论（详见交付报告）：同样的断言在旧实现上失败，旧实现首帧输出
 * `--tm-sidebar-width:130px`（默认），新实现输出记忆值。
 */
const renderFirstFrame = (props: Record<string, unknown>): string =>
  renderToStaticMarkup(
    createElement(TagManager, { t: (key: string) => key, theme: "light", ...props }) as never
  );

const sidebarWidthFrom = (html: string): string | undefined =>
  html.match(/--tm-sidebar-width:\s*([^;"]+)/)?.[1]?.trim();

const sidebarHeightFrom = (html: string): string | undefined =>
  html.match(/--tm-sidebar-height:\s*([^;"]+)/)?.[1]?.trim();

describe("首帧即记忆比例（不依赖异步读取）", () => {
  it("传入记忆值时，首帧宽度/高度就是记忆值（静态渲染不执行任何 effect）", () => {
    const remembered = { width: 317, height: 260, collapsed: false };
    const html = renderFirstFrame({ persistedSize: JSON.stringify(remembered) });

    // renderToStaticMarkup 不运行 useEffect：此前读到记忆比例只能靠 effect，
    // 因此这条断言等价于"打开瞬间就是记忆比例"。
    expect(sidebarWidthFrom(html)).toBe("317px");
    expect(sidebarHeightFrom(html)).toBe("260px");
    expect(sidebarWidthFrom(html)).not.toBe("130px");
  });

  it("记忆值为折叠态时，首帧就是折叠态（48px 轨道，不是默认展开宽度）", () => {
    const html = renderFirstFrame({
      persistedSize: { width: 200, height: 180, collapsed: true },
    });

    expect(sidebarWidthFrom(html)).toBe("48px");
    expect(html).toContain("sidebar-collapsed");
  });

  it("传入的是 JSON 字符串（真实设置存储形态）时同样生效", () => {
    const html = renderFirstFrame({
      persistedSize: '{"width":288,"height":190,"collapsed":false}',
    });
    expect(sidebarWidthFrom(html)).toBe("288px");
    expect(sidebarHeightFrom(html)).toBe("190px");
  });

  it("没有记忆值（首次运行）时首帧是默认比例，不抛异常", () => {
    const html = renderFirstFrame({});
    expect(sidebarWidthFrom(html)).toBe("130px");
    expect(sidebarHeightFrom(html)).toBe("180px");
  });

  it("记忆值损坏时首帧回退默认比例，而不是 NaN 或崩溃", () => {
    for (const broken of ['{"width":', "{ not json", { width: "200" }, 0, []]) {
      const html = renderFirstFrame({ persistedSize: broken });
      expect(sidebarWidthFrom(html)).toBe("130px");
      expect(sidebarHeightFrom(html)).toBe("180px");
    }
  });

  it("首帧不等待任何异步结果：整段标记里不出现未解析的占位值", () => {
    const html = renderFirstFrame({ persistedSize: { width: 301, height: 200 } });
    expect(html).not.toContain("NaN");
    expect(html).not.toContain("undefinedpx");
  });
});

describe("resolveTagManagerLayout（首帧选哪套几何）", () => {
  const stored = {
    width: 300,
    height: 210,
    collapsed: false,
    stackedWidth: 120,
    stackedHeight: 340,
    stackedCollapsed: true,
  };

  it("宽布局取宽布局的记忆值", () => {
    expect(resolveTagManagerLayout(stored, false)).toEqual({
      width: 300,
      height: 210,
      collapsed: false,
    });
  });

  it("窄/竖排布局取竖排自己的记忆值（两套几何不互相污染）", () => {
    expect(resolveTagManagerLayout(stored, true)).toEqual({
      width: 120,
      height: 340,
      collapsed: true,
    });
  });

  it("缺失或损坏输入回退默认比例", () => {
    for (const broken of [undefined, null, "", "坏数据", 42]) {
      expect(resolveTagManagerLayout(broken, false)).toEqual({
        width: 130,
        height: 180,
        collapsed: false,
      });
    }
  });

  it("未单独记忆竖排时继承宽布局数值（beta 升级路径）", () => {
    expect(resolveTagManagerLayout({ width: 300, height: 210 }, true)).toEqual({
      width: 300,
      height: 210,
      collapsed: false,
    });
  });

  it("与 isStackedViewport 的阈值一致，避免首帧与 effect 判定分叉", () => {
    expect(isStackedViewport(340)).toBe(true);
    expect(isStackedViewport(341)).toBe(false);
    expect(isStackedViewport(Number.NaN)).toBe(false);
  });
});

describe("resolveInitialTagManagerLayout（本会话写入优先于启动时快照）", () => {
  it("本会话写过就用本会话的值（否则同会话重开会闪回旧比例）", () => {
    const bootValue = { width: 130, height: 180 };
    const sessionValue = { width: 318, height: 240 };

    expect(resolveInitialTagManagerLayout(bootValue, sessionValue, false)).toEqual({
      width: 318,
      height: 240,
      collapsed: false,
    });
  });

  it("本会话没写过就用启动快照", () => {
    expect(resolveInitialTagManagerLayout({ width: 200, height: 190 }, undefined, false)).toEqual({
      width: 200,
      height: 190,
      collapsed: false,
    });
  });

  it("两者都没有时回退默认比例", () => {
    expect(resolveInitialTagManagerLayout(undefined, undefined, false)).toEqual({
      width: 130,
      height: 180,
      collapsed: false,
    });
  });

  it("本会话的值损坏时回退默认，而不是采用启动快照的部分字段", () => {
    expect(resolveInitialTagManagerLayout({ width: 200 }, "坏数据", false)).toEqual({
      width: 130,
      height: 180,
      collapsed: false,
    });
  });
});
