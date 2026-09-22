import { describe, it, expect } from "vitest";
import {
  parseTagManagerSidebarSize,
  applyCollapseToggle,
  COLLAPSE_THRESHOLD_PX,
  EXPANDED_SIDEBAR_WIDTH,
  shouldShowTag,
  isSensitiveFeatureEnabled,
} from "./TagManager";

/**
 * R2: the geometry parser is the one piece of the sidebar-persistence feature that
 * is pure logic, so it carries the guarantees the UI depends on:
 *
 *  - a corrupt or absent value must fall back to the defaults instead of producing
 *    `NaN` / negative / absurd sizes that would break the layout;
 *  - the wide and stacked layouts each keep their own numbers;
 *  - `collapsed` is only ever true when the stored value is literally `true`.
 *
 * The component module is imported for its exported helper only; nothing in this
 * file mounts the component, so no Tauri host or DOM is required.
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

  it("内置敏感分组：空且功能关闭时才隐藏（唯一的隐藏条件）", () => {
    expect(shouldShowTag({ name: "sensitive", count: 0 }, false)).toBe(false);
    expect(shouldShowTag({ name: "密码", count: 0 }, false)).toBe(false);
    expect(shouldShowTag({ name: "password", count: 0 }, false)).toBe(false);
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
