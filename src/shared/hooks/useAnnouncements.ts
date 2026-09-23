import { useEffect, useState } from "react";
import type { Announcement } from "../types";

export type { Announcement } from "../types";

/**
 * 公告提示。
 *
 * # 为什么这里不再请求任何服务器
 *
 * 原实现会在启动时、以及每 6 小时一次，向原项目（本项目 fork 的来源）的服务器
 * 发一个带设备标识与版本号的请求来拉取"公告"。那个服务器不属于本项目，请求注定
 * 失败；而它把设备标识发往第三方，属于不必要的数据外发。
 *
 * 因此这条对外通道整体移除：本 hook 只维持"有一份公告列表"这个接口形状，内容恒为
 * 空。调用方（`App` 与 `AnnouncementSystem`）无需改动——空列表时它们本来就不渲染。
 *
 * 若将来确实要发布公告，应改为经由 git 仓库或用户显式配置的渠道获取，而不是再次
 * 引入一个隐式的后台轮询。
 */
export function useAnnouncements() {
  const [announcements] = useState<Announcement[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    // 没有远端可拉，立即结束加载态；保留这段是为了接口与调用方兼容。
    setLoading(false);
  }, []);

  /** 保留既有签名：调用方用它关闭单条公告。列表恒空时它是无操作。 */
  const dismissAnnouncement = (id: string, forever: boolean = true) => {
    void id;
    void forever;
  };

  return { announcements, loading, dismissAnnouncement };
}
