// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import CredentialExposureDialog from "./CredentialExposureDialog";
import { useCredentialExposureNotice } from "../../../shared/hooks/useCredentialExposureNotice";
import {
  EXPOSURE_FALLBACK_TEXT,
  EXPOSURE_LOCALE_KEYS,
  exposureText,
  type CredentialExposureNotice,
} from "../lib/credentialExposureNotice";

/**
 * 「存量凭据外流」升级告知的界面契约。
 *
 * # 这个文件要钉住什么
 *
 * 1. **该提示的必须提示**：后端说 `shouldNotify` 时，弹窗真的渲染出"是哪三项、
 *    传到哪、为什么要换、去哪换"。
 * 2. **不该提示的绝不提示**：`shouldNotify=false` 时**一个字节都不渲染**。
 * 3. **一次性**：任一出口（我知道 / 去设置）都真的调了落标记的命令。
 * 4. **跳转可用**：主按钮会先关窗、再请求打开设置（顺序反了会让弹窗压住设置面板）。
 *
 * # 为什么必须真实挂载 + mock 宿主 API
 *
 * 断言的对象全部发生在"命令返回之后"：`shouldNotify` 来自后端，标记写没写要看
 * `invoke` 的调用记录。在静态渲染上断言"没有弹窗"会恒真——那种通过的测试什么也
 * 证明不了。因此这里 mock 掉 `@tauri-apps/api/core`，让命令真的返回数据。
 *
 * # 为什么文案不走 `t` 桩
 *
 * 用**三语兜底表的真实形状**（`EXPOSURE_FALLBACK_TEXT`）而不是手写桩：手写的桩
 * 会掩盖"词条缺失"，而"弹窗里出现 `security_exposure_body` 这种内部键名"正是本组
 * 要防的一个具体失败。
 */

const { invokeMock, isTauriMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  isTauriMock: vi.fn(() => true),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

vi.mock("../../../shared/lib/tauriRuntime", () => ({
  isTauriRuntime: isTauriMock,
}));

/** 真实形状的 `t`：查不到就返回键名本身（与 `App.tsx` 的 `t` 同一约定）。 */
const zhDict = EXPOSURE_FALLBACK_TEXT.zh;
const t = (key: string) => zhDict[key] ?? key;

let container: HTMLDivElement;
let root: Root;

const flush = async () => {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
};

const NOTICE: CredentialExposureNotice = {
  shouldNotify: true,
  evidence: "confirmed_sync_history",
  acknowledged: false,
  storedCredentialKeys: ["mqtt_password", "mqtt_username"],
};

/** 只挂载**弹窗本身**：canonical 输入是 `notice`，与查找路径解耦。 */
const mountDialog = async (
  notice: CredentialExposureNotice | null,
  handlers: { onOpenSettings?: () => void; onClose?: () => void } = {}
) => {
  await act(async () => {
    root.render(
      createElement(CredentialExposureDialog, {
        notice,
        t,
        language: "zh",
        theme: "light",
        onOpenSettings: handlers.onOpenSettings ?? (() => {}),
        onClose: handlers.onClose ?? (() => {}),
      })
    );
  });
  await flush();
};

/** 端到端壳：hook（真查后端）+ 弹窗，两者接起来跑。 */
const Probe = () => {
  const { notice, dismiss } = useCredentialExposureNotice(true);
  return createElement(CredentialExposureDialog, {
    notice,
    t,
    language: "zh",
    theme: "light",
    onOpenSettings: () => {},
    onClose: dismiss,
  });
};

const mountProbe = async () => {
  await act(async () => {
    root.render(createElement(Probe));
  });
  await flush();
};

const dialog = () => document.querySelector<HTMLElement>("[data-credential-exposure-dialog]");
const click = async (selector: string) => {
  const el = document.querySelector<HTMLElement>(selector);
  if (!el) throw new Error(`要点击的元素不存在：${selector}`);
  await act(async () => {
    el.click();
  });
  await flush();
};

const callsTo = (cmd: string) =>
  invokeMock.mock.calls.filter((c) => c[0] === cmd);

beforeEach(() => {
  invokeMock.mockReset();
  isTauriMock.mockReset();
  isTauriMock.mockReturnValue(true);
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "get_credential_exposure_notice") return NOTICE;
    return undefined;
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

// ---------------------------------------------------------------------------
// 方向一：确实可能受影响 ⇒ 必须提示
// ---------------------------------------------------------------------------

describe("确实可能受影响", () => {
  it("弹窗渲染出标题、事实、原因、做法，且不残留内部键名", async () => {
    await mountDialog(NOTICE);
    expect(dialog()).not.toBeNull();

    const body = document.querySelector("[data-credential-exposure-body]")?.textContent ?? "";
    expect(body).toContain("云同步");
    // 事实句必须点明"你自己的云端存储"，否则读者会以为传给了开发者。
    expect(body).toContain("你自己配置的云端存储");
    // 不许说成"泄露到公网"这类会制造恐慌的说法。
    expect(body).not.toContain("公网");
    expect(body).not.toContain("泄露");

    expect(document.querySelector("[data-credential-exposure-why]")?.textContent).toContain("建议更换");
    expect(document.querySelector("[data-credential-exposure-how]")?.textContent).toContain("设置");

    // 三语兜底表生效 ⇒ 界面上不该出现内部键名。
    const all = container.textContent ?? "";
    expect(all).not.toContain("security_exposure_");
  });

  it("只列出本机确实还留着的那几项（不编造第三项）", async () => {
    await mountDialog(NOTICE);
    const items = Array.from(
      document.querySelectorAll("[data-credential-exposure-items] li")
    ).map((n) => n.textContent);
    expect(items).toEqual(["MQTT 密码", "MQTT 用户名"]);
    // `ai_profiles` 不在 storedCredentialKeys 里 ⇒ 不得出现在清单中。
    expect(items.join("|")).not.toContain("AI");
    // 清单项必须用**专属**词条：既有的 `mqtt_password` 词条是"密码（可选）"，
    // 那是对着输入框写的，放进"这三项被上传了"的清单里读不出是哪个密码。
    expect(items.join("|")).not.toContain("可选");
  });

  it("本机三项都还在时，清单列出三项", async () => {
    await mountDialog({
      ...NOTICE,
      storedCredentialKeys: ["mqtt_password", "mqtt_username", "ai_profiles"],
    });
    const items = document.querySelectorAll("[data-credential-exposure-items] li");
    expect(items).toHaveLength(3);
  });

  it("清单为空时不渲染空的列表块", async () => {
    await mountDialog({ ...NOTICE, storedCredentialKeys: [] });
    expect(document.querySelector("[data-credential-exposure-items]")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// 方向二：肯定没受影响 / 判据不成立 ⇒ 一个字节都不渲染
// ---------------------------------------------------------------------------

describe("不该提示时不得提示", () => {
  it("notice 为 null 时不渲染任何东西", async () => {
    await mountDialog(null);
    expect(dialog()).toBeNull();
    expect(container.textContent).toBe("");
  });

  it("后端说 shouldNotify=false 时，端到端壳里也不渲染", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_credential_exposure_notice") {
        return {
          shouldNotify: false,
          evidence: "not_configured",
          acknowledged: false,
          storedCredentialKeys: [],
        };
      }
      return undefined;
    });
    await mountProbe();
    expect(dialog()).toBeNull();
    expect(container.textContent).toBe("");
  });

  it("查询失败时同样不渲染（'不知道'不等于'要提示'）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_credential_exposure_notice") throw new Error("boom");
      return undefined;
    });
    await mountProbe();
    expect(dialog()).toBeNull();
  });

  it("非 Tauri 环境不查询、不渲染", async () => {
    isTauriMock.mockReturnValue(false);
    await mountProbe();
    expect(dialog()).toBeNull();
    expect(callsTo("get_credential_exposure_notice")).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// 一次性 + 跳转
// ---------------------------------------------------------------------------

describe("一次性与跳转", () => {
  it("端到端：查询到需要提示时渲染出来", async () => {
    await mountProbe();
    expect(dialog()).not.toBeNull();
    expect(callsTo("get_credential_exposure_notice")).toHaveLength(1);
  });

  it("点「我知道了」：落标记 + 关窗", async () => {
    let closed = 0;
    await mountDialog(NOTICE, { onClose: () => (closed += 1) });
    await click("[data-credential-exposure-dismiss]");

    expect(callsTo("mark_credential_exposure_notice_seen")).toHaveLength(1);
    expect(closed).toBe(1);
  });

  it("点右上角关闭：同样落标记（否则下次还会弹）", async () => {
    await mountDialog(NOTICE, { onClose: () => {} });
    await click("[data-credential-exposure-close]");
    expect(callsTo("mark_credential_exposure_notice_seen")).toHaveLength(1);
  });

  it("点「去设置里更换」：先关窗再打开设置（顺序反了会被弹窗压住）", async () => {
    const order: string[] = [];
    await mountDialog(NOTICE, {
      onClose: () => order.push("close"),
      onOpenSettings: () => order.push("open-settings"),
    });
    await click("[data-credential-exposure-go-settings]");
    expect(order).toEqual(["close", "open-settings"]);
    expect(callsTo("mark_credential_exposure_notice_seen")).toHaveLength(1);
  });

  it("一次性：端到端里点掉之后弹窗消失，且标记已落盘", async () => {
    await mountProbe();
    expect(dialog()).not.toBeNull();
    await click("[data-credential-exposure-dismiss]");
    expect(dialog()).toBeNull();
    expect(callsTo("mark_credential_exposure_notice_seen")).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// 文案契约（对着三语兜底表，与 autoBackupLocaleContract 同一做法）
// ---------------------------------------------------------------------------

describe("三语兜底文案契约", () => {
  const LANGS = ["zh", "en", "tw"] as const;

  it("弹窗会用到的每个键在三语里都存在", () => {
    const used = [
      EXPOSURE_LOCALE_KEYS.title,
      EXPOSURE_LOCALE_KEYS.body,
      EXPOSURE_LOCALE_KEYS.why,
      EXPOSURE_LOCALE_KEYS.how,
      EXPOSURE_LOCALE_KEYS.goSettings,
      EXPOSURE_LOCALE_KEYS.dismiss,
    ];
    for (const lang of LANGS) {
      const missing = used.filter((k) => !EXPOSURE_FALLBACK_TEXT[lang][k]);
      expect(missing, `语言 ${lang} 缺少词条`).toEqual([]);
    }
  });

  it("三语都不含占位符（本组文案是纯文本，没有任何 {x} 需要替换）", () => {
    for (const lang of LANGS) {
      for (const [key, value] of Object.entries(EXPOSURE_FALLBACK_TEXT[lang])) {
        expect(/\{[a-zA-Z]+\}/.test(value), `${lang}.${key} 含占位符`).toBe(false);
      }
    }
  });

  it("三语的事实口径一致：都点明'你自己配置的云端存储'，都不说'公网/泄露'", () => {
    expect(EXPOSURE_FALLBACK_TEXT.zh.security_exposure_body).toContain("你自己配置的云端存储");
    expect(EXPOSURE_FALLBACK_TEXT.en.security_exposure_body).toContain(
      "you configured yourself"
    );
    expect(EXPOSURE_FALLBACK_TEXT.tw.security_exposure_body).toContain("你自己設定的雲端儲存");

    for (const lang of LANGS) {
      const body = EXPOSURE_FALLBACK_TEXT[lang].security_exposure_body;
      expect(body, `${lang} 不得说成公网泄露`).not.toMatch(/公網|公网|public internet|泄露|洩露/);
    }
  });

  it("`exposureText` 查不到 locales 时退回兜底表；locales 有词条时以 locales 为准", () => {
    // 模拟"用户还没把词条填进 locales"：`t` 原样返回键名 ⇒ 退回兜底表。
    const notInLocales = (key: string) => key;
    expect(exposureText(notInLocales, "zh", "security_exposure_title")).toBe(
      EXPOSURE_FALLBACK_TEXT.zh.security_exposure_title
    );
    expect(exposureText(notInLocales, "en", "security_exposure_title")).toBe(
      EXPOSURE_FALLBACK_TEXT.en.security_exposure_title
    );
    expect(exposureText(notInLocales, "tw", "security_exposure_title")).toBe(
      EXPOSURE_FALLBACK_TEXT.tw.security_exposure_title
    );

    // 模拟"用户已经填进 locales"：以语言文件为准，兜底表不得抢答。
    const filled = (key: string) => (key === "security_exposure_title" ? "FROM_LOCALES" : key);
    expect(exposureText(filled, "zh", "security_exposure_title")).toBe("FROM_LOCALES");

    // 三语兜底都查不到时，最后退回键名（不抛异常）。
    expect(exposureText(notInLocales, "zh", "nope_not_defined")).toBe("nope_not_defined");
  });

  it("语言识别：非法/缺失时按 zh 兜底", () => {
    const notInLocales = (key: string) => key;
    expect(exposureText(notInLocales, undefined, "security_exposure_dismiss")).toBe("我知道了");
    expect(exposureText(notInLocales, "en", "security_exposure_dismiss")).toBe("Got it");
    expect(exposureText(notInLocales, "tw", "security_exposure_dismiss")).toBe("我知道了");
    expect(exposureText(notInLocales, "fr", "security_exposure_dismiss")).toBe("我知道了");
  });
});
