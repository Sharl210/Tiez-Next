import { describe, expect, it } from "vitest";
import { getClipboardTypeLabel } from "./contentTypeLabel";

describe("getClipboardTypeLabel", () => {
  it("labels rich and plain text", () => {
    expect(getClipboardTypeLabel("rich_text", "<p>x</p>")).toBe("富文本");
    expect(getClipboardTypeLabel("text", "x")).toBe("纯文本");
    expect(getClipboardTypeLabel("code", "x")).toBe("纯文本");
    expect(getClipboardTypeLabel("url", "https://example.com")).toBe("纯文本");
  });
  it("labels file extensions case-insensitively", () => {
    expect(getClipboardTypeLabel("file", "C:\\temp\\Report.PDF")).toBe(".pdf文件");
    expect(getClipboardTypeLabel("image", "/tmp/photo.JPG")).toBe(".jpg文件");
  });
  it("uses 文件 for extensionless or invalid paths", () => {
    expect(getClipboardTypeLabel("file", "C:\\temp\\LICENSE")).toBe("文件");
    expect(getClipboardTypeLabel("file", "data:image/png;base64,abc")).toBe("文件");
  });
});
