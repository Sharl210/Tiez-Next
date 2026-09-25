import { useCallback, useRef } from "react";
import type { WheelEvent as ReactWheelEvent } from "react";

type UseSearchScrollOptions = {
  showSearchBox: boolean;
  setShowSearchBox: (val: boolean) => void;
  search: string;
  showSettings: boolean;
  showTagManager: boolean;
  appSettings: Record<string, string>;
};

/**
 * 搜索框的滚动触发逻辑 —— **已停用**。
 *
 * 此前搜索框靠"滚到列表最顶部再往下拉"触发打开，往下滚触发关闭。这有两个问题：
 *
 * ① 用户必须先滚到最顶部才能打开搜索 —— 在长列表里非常反直觉；
 * ② 下拉判定与标签候选列表的滚动判定**互相干扰** —— 候补列表里滚动会误触发
 *   搜索框的打开/关闭，正是用户反馈的"候选列表滚动还会触发外面搜索框"。
 *
 * 现在搜索框改为右上角放大镜按钮触发（`AppHeader.tsx`），在任何位置都能开关，
 * 不依赖滚动位置。本 hook 保留签名以避免调用方大面积改动，但 `handleMainWheel`
 * 不再做任何搜索相关的操作。
 *
 * `handleListScroll` 仍保留 `listScrollTopRef` 的记录 —— 如果将来有别的逻辑
 * 需要知道"是否在顶部"，可以复用。但不再触发搜索框。
 */
export const useSearchScroll = ({
}: UseSearchScrollOptions = {}) => {
  const listScrollTopRef = useRef(0);

  const handleListScroll = useCallback((offset: number) => {
    listScrollTopRef.current = offset;
  }, []);

  // 不再通过滚轮触发搜索框的打开/关闭 —— 改为按钮触发。
  const handleMainWheel = useCallback(
    (_e: ReactWheelEvent<HTMLElement>) => {
      // intentionally empty: search is now button-triggered, not scroll-triggered.
    },
    []
  );

  return {
    handleListScroll,
    handleMainWheel
  };
};
