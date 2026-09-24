// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import AutostartSetting from "./AutostartSetting";
import PasteMethodSetting from "./PasteMethodSetting";

/**
 * 「系统级设置真的生效了吗」的界面行为：自启动回读 + 游戏模式不静默改设置。
 *
 * # 这里要证明的两件事，都是"看起来正常但实际坏了"的反面
 *
 * **自启动**：旧实现先乐观置位再 invoke，`.catch(console.error)` 吞掉失败——开关永远
 * 停在"已开"。所以这里必须真的挂载、真的触发 change，然后断言：
 *   - 后端失败时开关**不亮**（而不是亮了但没人知道）；
 *   - 失败原因**显示出来**（用那条零引用的 `autostart_failed` 文案）；
 *   - 成功时把**读回来的注册表命令原文**显示出来（"真的生效了"的唯一可信证据）。
 *
 * **游戏模式**：旧后端在未提权时静默把设置改回默认方案。现在前端要在**不改设置**的
 * 前提下告知"未提权，暂不生效"，并给出提权重启入口。
 *
 * # 桩文案的形状必须与 `src/locales.ts` 一致
 *
 * `t` 桩只给本文件要断言的键，且**不编造占位符**；真实键的存在性与三语一致性由
 * `systemSettingsLocaleContract.test.ts` 单独把守。
 */

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/plugin-dialog", () => ({
    ask: vi.fn(async () => false),
    message: vi.fn(async () => undefined),
}));

/** 与本组界面相关的真实词条（zh）。 */
const T: Record<string, string> = {
    autostart: "开机自启动",
    autostart_failed: "设置自启动失败: ",
    autostart_verified: "已生效（注册表回读一致）",
    autostart_registered_at: "系统将启动：",
    autostart_stale_names: "检测到旧版本残留的自启动项：",
    autostart_stale_hint: "把本开关关闭再打开一次即可顺手清理。",
    autostart_readback_failed: "已写入但无法回读注册表，因此无法确认是否真的生效。",
    paste_method: "粘贴方案",
    paste_method_hint: "全屏游戏无法粘贴时请切换到「游戏模式」",
    paste_method_game_mode_hint: "需管理员权限。",
    paste_method_shift_insert_hint: "标准粘贴方式",
    paste_method_ctrl_v_hint: "模拟常规按键",
    game_mode_needs_admin: "游戏模式需要管理员权限才能生效；当前未提权，所以本选项暂不生效。你的选择已被保留，不会被自动改掉。",
    restart_as_admin: "以管理员身份重启",
    restart_as_admin_hint_settings: "以管理员身份重启应用后，游戏模式即可生效。",
};

const t = (key: string) => T[key] ?? key;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
    invokeMock.mockReset();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
});

afterEach(() => {
    act(() => root.unmount());
    container.remove();
});

const flush = async () => {
    await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
    });
};

const checkbox = () => container.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
/**
 * 触发一次开关切换。
 *
 * 【为什么用 `click()` 而不是手改 `checked` + 派发 `change`】React 的受控复选框只在
 * 它自己合成的事件序列里恢复 DOM 与 prop 的一致性。手工改属性再派发一个裸 `change`
 * 事件，React 的受控恢复逻辑不一定会跑，于是 DOM 会停在一个**组件并不认可**的状态上，
 * 断言就测不到真实行为。`click()` 走的是浏览器原生路径（React 对 checkbox 的
 * onChange 正是由 click 触发的），因此能真实反映"用户点了一下"。
 */
const clickCheckbox = async () => {
    await act(async () => {
        checkbox().click();
    });
};

describe("开机自启动：只有回读确认才亮", () => {
    it("后端返回已生效时，开关亮起并显示注册表回读原文", async () => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "is_autostart_enabled")
                return {
                    enabled: true,
                    registeredCommand: '"C:\\App\\tiez-next.exe" --minimized',
                    currentExe: "C:\\App\\tiez-next.exe",
                    staleNames: [],
                    readable: true,
                };
            throw new Error(`unexpected ${cmd}`);
        });

        act(() => {
            root.render(<AutostartSetting t={t} initialEnabled={false} onStateChange={() => {}} />);
        });
        await flush();

        expect(checkbox().checked).toBe(true);
        // 证据：读回来的注册表路径必须在界面上，而不是只有一句"设置成功"。
        expect(container.textContent).toContain("C:\\App\\tiez-next.exe");
        expect(container.textContent).toContain("已生效（注册表回读一致）");
    });

    it("**后端失败时开关不亮**，并显示 autostart_failed 的原因", async () => {
        // 第一次读：未开启（真实状态）。
        // 用户点开：后端回读不通过 → 抛错。
        // 失败后重新读：仍然是未开启。
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "is_autostart_enabled")
                return {
                    enabled: false,
                    registeredCommand: null,
                    currentExe: "C:\\App\\tiez-next.exe",
                    staleNames: [],
                    readable: true,
                };
            if (cmd === "toggle_autostart")
                throw "自启动写入后回读未通过：期望指向 C:\\App\\tiez-next.exe，实际 （注册表里没有该值）";
            throw new Error(`unexpected ${cmd}`);
        });

        act(() => {
            root.render(<AutostartSetting t={t} initialEnabled={false} onStateChange={() => {}} />);
        });
        await flush();
        expect(checkbox().checked).toBe(false);

        expect(checkbox().checked).toBe(false);
        await clickCheckbox();
        await flush();

        // 核心断言：开关**不能**停在"已开"——那正是旧实现的静默失败形态。
        expect(
            checkbox().checked,
            "回读未通过时开关必须是关的，否则用户看到的永远是假的「已开启」"
        ).toBe(false);
        expect(container.textContent).toContain("设置自启动失败");
        expect(container.textContent).toContain("回读未通过");
    });

    it("旧版残留值不被当成已开启，而是如实列出并提示可清理", async () => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "is_autostart_enabled")
                return {
                    enabled: false,
                    registeredCommand: null,
                    currentExe: "C:\\App\\tiez-next.exe",
                    staleNames: ["TieZ", "tie-z"],
                    readable: true,
                };
            throw new Error(`unexpected ${cmd}`);
        });

        act(() => {
            root.render(<AutostartSetting t={t} initialEnabled={true} onStateChange={() => {}} />);
        });
        await flush();

        // 即便初值是 true（老判据算出来的），回读结果才是权威。
        expect(checkbox().checked).toBe(false);
        expect(container.textContent).toContain("TieZ");
        expect(container.textContent).toContain("tie-z");
        expect(container.textContent).toContain("残留");
    });

    it("回读本身失败时明确说「无法确认」，而不是静默显示为关", async () => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "is_autostart_enabled")
                return {
                    enabled: false,
                    registeredCommand: null,
                    currentExe: "",
                    staleNames: [],
                    readable: false,
                };
            throw new Error(`unexpected ${cmd}`);
        });

        act(() => {
            root.render(<AutostartSetting t={t} initialEnabled={false} onStateChange={() => {}} />);
        });
        await flush();

        expect(container.textContent).toContain("无法回读注册表");
    });

    it("成功后会把权威状态同步回应用级 state（其它地方也在读它）", async () => {
        const seen: boolean[] = [];
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "is_autostart_enabled")
                return {
                    enabled: true,
                    registeredCommand: '"C:\\App\\tiez-next.exe"',
                    currentExe: "C:\\App\\tiez-next.exe",
                    staleNames: [],
                    readable: true,
                };
            throw new Error(`unexpected ${cmd}`);
        });

        act(() => {
            root.render(
                <AutostartSetting t={t} initialEnabled={false} onStateChange={(v) => seen.push(v)} />
            );
        });
        await flush();
        expect(seen).toContain(true);
    });
});

describe("游戏模式：不改设置，只告知并提供提权入口", () => {
    const renderPaste = async (status: unknown, onChange?: (v: string) => void) => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "get_paste_method_status") return status;
            if (cmd === "save_setting") return undefined;
            throw new Error(`unexpected ${cmd}`);
        });
        act(() => {
            root.render(
                <PasteMethodSetting t={t} method="game_mode" setMethod={onChange ?? (() => {})} />
            );
        });
        await flush();
    };

    it("未提权时如实告知「暂不生效」，并说明选择已被保留", async () => {
        await renderPaste({
            method: "game_mode",
            isAdmin: false,
            effective: false,
            requiresAdmin: true,
        });

        expect(container.textContent).toContain("暂不生效");
        expect(container.textContent).toContain("已被保留");
        // 提权入口必须出现（后端命令早就就绪，此前前端零引用）。
        expect(container.textContent).toContain("以管理员身份重启");
    });

    it("提权后不再显示未生效告知，也不显示提权按钮", async () => {
        await renderPaste({
            method: "game_mode",
            isAdmin: true,
            effective: true,
            requiresAdmin: true,
        });

        expect(container.textContent).not.toContain("暂不生效");
        expect(container.textContent).not.toContain("以管理员身份重启");
    });

    it("**界面不得替用户改设置**：渲染与刷新都不产生 save_setting 调用", async () => {
        await renderPaste({
            method: "game_mode",
            isAdmin: false,
            effective: false,
            requiresAdmin: true,
        });

        const writes = invokeMock.mock.calls.filter(([cmd]) => cmd === "save_setting");
        expect(
            writes,
            "未提权的告知不能伴随一次静默改设置（那正是旧后端在做的事）"
        ).toEqual([]);
    });

    it("用户主动改选项时才写设置（且写入的是用户选择的值本身）", async () => {
        const seen: string[] = [];
        await renderPaste(
            { method: "game_mode", isAdmin: false, effective: false, requiresAdmin: true },
            (v) => seen.push(v)
        );

        const select = container.querySelector<HTMLSelectElement>("select")!;
        await act(async () => {
            select.value = "ctrl_v";
            select.dispatchEvent(new Event("change", { bubbles: true }));
        });
        await flush();

        expect(seen).toEqual(["ctrl_v"]);
        const writes = invokeMock.mock.calls.filter(([cmd]) => cmd === "save_setting");
        expect(writes.length).toBe(1);
        expect(writes[0][1]).toEqual({ key: "app.paste_method", value: "ctrl_v" });
    });

    it("提权按钮调用 restart_as_admin；UAC 被取消时如实报错", async () => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "get_paste_method_status")
                return {
                    method: "game_mode",
                    isAdmin: false,
                    effective: false,
                    requiresAdmin: true,
                };
            if (cmd === "save_setting") return undefined;
            if (cmd === "restart_as_admin") throw "User may have cancelled UAC prompt.";
            throw new Error(`unexpected ${cmd}`);
        });

        act(() => {
            root.render(<PasteMethodSetting t={t} method="game_mode" setMethod={() => {}} />);
        });
        await flush();

        const button = Array.from(container.querySelectorAll("button")).find((b) =>
            b.textContent?.includes("以管理员身份重启")
        )!;
        expect(button).toBeTruthy();

        await act(async () => {
            button.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await flush();

        expect(invokeMock.mock.calls.some(([cmd]) => cmd === "restart_as_admin")).toBe(true);
        expect(container.textContent).toContain("UAC");
    });
});
