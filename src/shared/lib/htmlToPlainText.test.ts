// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { htmlToPlainText } from "./htmlToPlainText";

/**
 * 富文本 → 纯文本时，**所有**超链接都换成网址本身，而不是界面上的标签文字。
 *
 * # 为什么要这样
 *
 * 富文本里一条链接是 `<a href="http://host/c/T750/+/176116">T750 变更单</a>`，
 * 屏幕上显示的是带下划线的「T750 变更单」。纯文本正文代表"以纯文本形式粘贴或转换
 * 出去长什么样"，那时链接的真实目标是 `href`（能点、能复制、能直接打开），
 * 不是屏幕上的装饰文字。
 *
 * **不论链接在正文的什么位置、是不是唯一一个**，一律换成地址 —— 早期实现只在
 * "整条内容恰好就是一个链接"时才换，用户反馈正文里夹带的链接仍然看不到地址。
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

describe("块级换行与后端保持同一口径", () => {
  /*
   * 行结构必须与后端 `extract_plain_text_from_htmlish` 一致。否则同一条目
   * "点按钮转换"与"直接纯文本粘贴"会给出不同结果（后端是粘贴那条路的权威）。
   */
  it("两段之间保留一个空行", () => {
    expect(htmlToPlainText("<p>第一段</p><p>第二段</p>")).toBe("第一段\n\n第二段");
  });

  it("表格单元格不会被粘成一串", () => {
    const out = htmlToPlainText("<table><tr><td>项目</td><td>值</td></tr></table>");

    expect(out).toContain("项目");
    expect(out).toContain("值");
    expect(out).not.toBe("项目值");
  });

  it("<br> 仍是单个换行", () => {
    expect(htmlToPlainText("上行<br>下行")).toContain("上行\n下行");
  });
});

describe("正文里的链接同样换成网址", () => {
  it("行内链接换成地址，周围文字保留", () => {
    const html = `<p>详见 <a href="${URL}">T750 变更单</a> 里的说明</p>`;

    const plain = htmlToPlainText(html);

    expect(plain).toContain(URL);
    expect(plain).toContain("详见");
    expect(plain).toContain("里的说明");
  });

  it("多个链接**各自**换成自己的地址", () => {
    const html =
      '<p><a href="http://a.example/1">变更单</a> 与 <a href="http://b.example/2">版本说明</a></p>';

    const plain = htmlToPlainText(html);

    expect(plain).toContain("http://a.example/1");
    expect(plain).toContain("http://b.example/2");
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
  it("块级元素之间保留换行（与后端同为空行）", () => {
    // 后端把块标签的开闭都换成换行，所以相邻段落之间是**空行**。
    // 这条原先期望单个换行，那会与后端粘贴结果不一致。
    expect(htmlToPlainText("<p>第一行</p><p>第二行</p>")).toBe("第一行\n\n第二行");
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
