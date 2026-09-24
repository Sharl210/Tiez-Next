export const open = async () => null;
/**
 * 「确认」类对话框。
 *
 * 默认返回 false —— 与真实主机上"用户没点确认"一致，这样任何一次误触发的破坏性
 * 流程都不会在验证台里真的往下走。只有显式带 `?ask=yes` 时才返回 true，
 * 用于驱动"点确认之后会渲染出什么"那类量测（例如迁移结果卡片）。
 *
 * 【这是 mock】不证明真实主机的对话框行为；取消/确认两条分支由组件测试覆盖。
 */
export const ask = async () =>
  new URLSearchParams(location.search).get("ask") === "yes";
export const confirm = async () => false;
export const message = async () => undefined;
export const save = async () => null;
