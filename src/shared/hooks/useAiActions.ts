import { useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Dispatch, SetStateAction } from "react";
import type { ClipboardEntry } from "../types";
import type { AiProfile } from "../../features/settings/types";

interface UseAiActionsOptions {
  aiProfiles: AiProfile[];
  language: string;
  pushToast: (msg: string, duration?: number) => number;
  setShowSettings: Dispatch<SetStateAction<boolean>>;
  setProcessingAiId: Dispatch<SetStateAction<number | null>>;
  setHistory: Dispatch<SetStateAction<ClipboardEntry[]>>;
}

/**
 * R13：把纯文本转成可渲染的 HTML（按行 `<p>`，并转义 HTML 元字符）。
 *
 * 与后端 `clipboard_mutation::plain_text_to_html` 同一语义：AI 返回的是纯文本，
 * 而富文本条目的 `html_content` 必须与新正文一致，否则界面按 HTML 画、复制走
 * `content`，同一条目会显示两种内容。
 */
const plainTextToHtml = (text: string): string =>
  text
    .split("\n")
    .map((line) => `<p>${line.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")}</p>`)
    .join("");

export const useAiActions = ({
  aiProfiles,
  language,
  pushToast,
  setShowSettings,
  setProcessingAiId,
  setHistory
}: UseAiActionsOptions) => {
  const handleAIAction = useCallback(
    async (id: number, content: string, actionType: string) => {
      if (aiProfiles.length === 0) {
        pushToast(
          language === "zh"
            ? "请先在设置中添加 AI 模型"
            : "Please add an AI model in settings first",
          3000
        );
        setShowSettings(true);
        return;
      }

      setProcessingAiId(id);
      try {
        const aiResponse = await invoke<string>("call_ai", { id, content, actionType });

        setHistory((prev) =>
          prev.map((item) => {
            if (item.id == id) {
              const trimmedResponse = aiResponse.trim();
              const questionMatch = trimmedResponse.match(/^\[\[QUESTION:(.+?)\]\]$/);

              if (questionMatch) {
                const questionText = questionMatch[1].trim();
                return {
                  ...item,
                  isInputting: true,
                  content: questionText,
                  // 追问不是"改写结果"，保持条目原有格式载体不变；正文已换成问题文本，
                  // 因此富文本条目也要跟着换一份与新正文一致的 HTML。
                  html_content: item.content_type === 'rich_text'
                    ? plainTextToHtml(questionText)
                    : item.html_content,
                  preview:
                    questionText.length > 100
                      ? questionText.substring(0, 100).replace(/\n/g, " ") + "..."
                      : questionText.replace(/\n/g, " ")
                };
              }
              /*
               * R13：AI 改写不再把富文本条目降级成 `text`。
               *
               * AI 返回的是纯文本，所以这里给出一份与新正文一致的 HTML（按行包
               * `<p>`），而不是把类型偷走 —— 那正是"编辑/改写后坍缩成纯文本"的
               * 客户端那一份。后端 `ai_cmd` 写库时做同样的事，两边保持一致。
               */
              return {
                ...item,
                content: aiResponse,
                html_content: item.content_type === 'rich_text'
                  ? plainTextToHtml(aiResponse)
                  : item.html_content,
                isInputting: false,
                preview:
                  aiResponse.length > 100
                    ? aiResponse.substring(0, 100).replace(/\n/g, " ") + "..."
                    : aiResponse.replace(/\n/g, " ")
              };
            }
            return item;
          })
        );
      } catch (err) {
        const errorMsg = err?.toString() || "AI processing failed";
        pushToast(errorMsg, 5000);
      } finally {
        setProcessingAiId(null);
      }
    },
    [aiProfiles, language, pushToast, setHistory, setProcessingAiId, setShowSettings]
  );

  return { handleAIAction };
};


