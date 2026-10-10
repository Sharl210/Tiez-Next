// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { htmlToPlainText } from "./htmlToPlainText";

/**
 * 富文本 → 纯文本时，链接保留**地址**而不是界面上的标签文字。
 *
 * # 为什么要这样
 *
 * 富文本里一条链接是 `<a href="http://host/c/T750/+/176116">T750 变更单</a>`，
 * 屏幕上显示的是带下划线的「T750 变更单」。纯文本正文代表"以纯文本形式粘贴出去
 * 会长什么样"，那时用户要的是链接本身（能点、能复制、能直接打开），不是那层标签。
 *
 * 反过来说：**文章正文里的行内链接必须原样保留人话**，否则把整段文字里插进一个
 * 网址就毁掉了正文。这两条一起才是完整契约，缺一条都会被改坏。
 *
 * 后端 `derive_rich_text_content` 是权威实现（写进 `content` 列的就是它），本模块
 * 是同一口径的界面侧实现，两边规则必须一致 —— 对应的后端测试在
 * `src-tauri/src/services/clipboard/utils.rs` 的 `utils::tests`。
 */

const URL = "http://192.168.23.98:8888/c/T750/+/176116";

describe("整条内容就是一个链接时取地址", () => {
  it("锚文本与地址不同 → 得到地址", () => {
    const html = `<a href="${URL}">T750 变更单</a>`;

    const plain = htmlToPlainText(html);

    expect(plain).toBe(URL);
    expect(plain).not.toContain("T750 变更单");
  });

  it("链接被块级元素包着也照样取地址", () => {
    const html = `<div><p><span><a href="${URL}">T750 变更单</a></span></p></div>`;

    expect(htmlToPlainText(html)).toBe(URL);
  });

  it("锚文本本身就是地址 → 不会出现两遍", () => {
    const html = `<a href="${URL}">${URL}</a>`;

    expect(htmlToPlainText(html)).toBe(URL);
  });

  it("单引号写法的 href 同样识别", () => {
    const html = `<a href='${URL}'>看这里</a>`;

    expect(htmlToPlainText(html)).toBe(URL);
  });
});

describe("行内链接保留人话（反向）", () => {
  it("正文里的链接不会被替换成网址", () => {
    const html = `<p>详见 <a href="${URL}">T750 变更单</a> 里的说明</p>`;

    const plain = htmlToPlainText(html);

    expect(plain).not.toContain("192.168.23.98");
    expect(plain).toContain("T750 变更单");
  });

  it("多个链接不当作单链接（不会挑其中一个地址）", () => {
    const html =
      '<p><a href="http://a.example/1">变更单</a> 与 <a href="http://b.example/2">版本说明</a></p>';

    const plain = htmlToPlainText(html);

    expect(plain).not.toContain("a.example");
    expect(plain).not.toContain("b.example");
  });
});

describe("不可导航的 href 保留原文（反向）", () => {
  it("javascript: 不会被当成地址", () => {
    const html = '<a href="javascript:void(0)">点我</a>';

    expect(htmlToPlainText(html)).toBe("点我");
  });

  it("data: 不会被当成地址", () => {
    const html = '<a href="data:text/html,hi">看这里</a>';

    expect(htmlToPlainText(html)).toBe("看这里");
  });
});

describe("链接之外的基础行为不变", () => {
  it("块级元素之间保留换行", () => {
    expect(htmlToPlainText("<p>第一行</p><p>第二行</p>")).toBe("第一行\n第二行");
  });

  it("<br> 转成换行", () => {
    expect(htmlToPlainText("甲<br>乙")).toBe("甲\n乙");
  });

  it("实体被解码", () => {
    expect(htmlToPlainText("<p>a &amp; b</p>")).toBe("a & b");
  });

  it("空输入返回空串", () => {
    expect(htmlToPlainText("")).toBe("");
  });
});
