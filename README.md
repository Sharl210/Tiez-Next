<p align="left">
  <img src="docs/images/logo.png" width="32" vertical-align="middle" />
  <b>让碎片化信息轻松流转的剪贴板工具</b>
</p>

---

<div align="center">
  <img src="docs/images/logo.png" alt="Tiez-Next Logo" width="300" />

  ### **快，且始终同步。**

  | 版本 | 许可 | 平台 |
  | :--- | :--- | :--- |
  | [![Version](https://img.shields.io/github/v/release/Sharl210/Tiez-Next?label=VERSION&style=for-the-badge&color=2196F3)](https://github.com/Sharl210/Tiez-Next/releases) | [![License](https://img.shields.io/badge/LICENSE-GPL--3.0-FF9800?style=for-the-badge)](https://www.gnu.org/licenses/gpl-3.0) | [![Platform](https://img.shields.io/badge/PLATFORM-WINDOWS-f44336?style=for-the-badge)](https://github.com/Sharl210/Tiez-Next/releases) |
</div>

---

## 这是什么

Tiez-Next 是一个以 Rust 为核心的剪贴板管理器：复制过的文字、富文本、图片、文件路径都会自动留存，随时检索、随时粘贴，并能通过标签归类、跨设备同步。

本项目基于 [TieZ](https://github.com/jimuzhe/tiez-clipboard)（GPL-3.0）演进而来，已更名并独立发展。感谢原作者的开源工作。

> **从 TieZ 升级过来？** 新版使用独立的数据目录，首次启动**不会**自动搬运旧数据。请在「设置 → 数据与迁移」中手动选择旧目录执行迁移——迁移只读取旧目录、不改动原数据，可以反复执行。

---

## 主题展示

为不同工作场景准备的主题样式：

  <table>
    <tr>
      <td align="center"><b>极简毛玻璃</b><br><img src="docs/images/毛玻璃.png" width="220" /></td>
      <td align="center"><b>笔记本风格</b><br><img src="docs/images/书.png" width="220" /></td>
      <td align="center"><b>便利贴风格</b><br><img src="docs/images/便利贴.png" width="220" /></td>
      <td align="center"><b>3D 动感</b><br><img src="docs/images/3d.png" width="220" /></td>
    </tr>
  </table>

---

## 为什么选择 Tiez-Next

| 极速性能 | 深度工作流 | 本地隐私 | 跨端流转 |
| :--- | :--- | :--- | :--- |
| **瞬间响应**<br>Rust 核心层与原生剪贴板监听，追求毫秒级响应。 | **全能管理**<br>支持富文本、多色标签、条目备注与 AI 协作。 | **本地安全**<br>数据完全本地化存储，敏感信息预览自动脱敏。 | **多端无感同步**<br>基于 WebDAV 与 MQTT，让剪贴板在设备间流动。 |

---

## 核心功能

### 基础体验
- **原生效率**：基于 Tauri 2 与 Rust 构建，内存占用低、响应流畅。
- **智能采集**：自动记录文字、富文本 (HTML)、图片、文件与目录路径；连续快速复制的多张图片不会被吞掉。
- **现代美学**：支持 云母/亚克力 背景效果与暗黑模式，内置多款调优主题。
- **贴边收纳**：支持自动停靠屏幕边缘，节省桌面空间且随时呼出。

### 管理与增强
- **标签系统**：多色标签分类整理；分组可自由新建、重命名与删除，**删除分组不会连带删除条目**。
- **条目备注**：给任意条目（含图片、文件）添加备注，标签管理与主页面均可查看。
- **任意条目可编辑**：文字类可直接改正文；富文本保存前会提示格式将转为纯文本；图片/文件类可编辑备注。
- **表情管理**：内置 Emoji 表情库，支持快捷搜索与输入。
- **隐私脱敏**：识别身份证、手机号、邮箱等隐私信息，预览时自动脱敏。

### 网络与传输
- **WebDAV 同步**：数据由你掌控，跨设备同步历史记录。
- **局域网传输**：在局域网内传输文件与内容。
- **秒传验证码**：手机端收到的短信验证码，快速同步至当前设备。
- **MQTT 协议**：轻量协议同步方案，适应不同网络环境。

### 效率提速
- **全局搜索**：按内容、来源应用、标签或日期检索；搜索时不会弹出标签面板遮挡结果。
- **顺序粘贴**：为高频办公场景设计的顺序拷贝/粘贴工作流。
- **粘贴即置顶**：从标签管理粘贴同样计入记录，并出现在主页面最新位置。
- **AI 接入（MCP）**：内置 MCP 服务，可让 AI 读取、搜索、编辑剪贴板历史与标签。**默认只读**，写操作需显式开启，并带令牌鉴权。

### 数据安全
- **备份与恢复**：一键导出全部数据为 zip（含数据库、附件、表情收藏、自定义背景），导入即完整恢复；导入前自动备份当前数据。
- **迁移中心**：集中管理旧版本数据目录，支持手动迁移与清理，删除前有明确风险提示。

---

## 系统要求

| 平台 | 运行环境要求 | 获取格式 |
| :--- | :--- | :--- |
| **Windows** | Windows 10/11 (x64)<br>*(推荐 Win11)* | `.exe` 安装包 |
| **macOS** | 代码保留相关支持，暂未发布产物 | 敬请期待 |
| **Linux** | 暂未发布产物 | 敬请期待 |

[**前往 Releases 下载最新版本 →**](https://github.com/Sharl210/Tiez-Next/releases)

---

## 从源码构建

需要 Node.js 与 Rust 工具链。

```bash
npm install
npm run tauri:build:win     # 交叉编译出 Windows 安装包
```

在 Linux/WSL2 上交叉编译 Windows 产物需要 `cargo-xwin`；产物位于 `src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/`。

---

<div align="center">
  <b>如果这个项目对你有帮助，欢迎点个 Star。</b>
</div>
