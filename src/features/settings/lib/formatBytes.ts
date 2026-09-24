/**
 * 把字节数格式化为人类可读形式。
 *
 * # 为什么抽成独立模块
 *
 * 这段实现原本内联在 `DataSettingsGroup.tsx` 里，只有那一个使用者。自动备份的
 * **备份列表**同样要展示"每份备份多大"，于是出现了第二个使用者。与其复制一份
 * （两份实现会在"该不该四舍五入 100 以上"这类细节上慢慢分叉），不如原地提出来
 * 让两边共用同一份。
 *
 * 行为与抽取前**逐字一致**：`0` 走 `0 B`；单位按 1024 进制取整到 `TB`；
 * 数值 ≥ 100 或已是字节时取整，否则保留一位小数。
 */
export const formatBytes = (bytes: number): string => {
    if (!bytes) return "0 B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
    const value = bytes / Math.pow(1024, i);
    return `${value >= 100 || i === 0 ? Math.round(value) : value.toFixed(1)} ${units[i]}`;
};

export default formatBytes;
