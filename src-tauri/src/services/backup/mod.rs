//! 备份导出与导入恢复（zip 容器 + manifest 契约）。
//!
//! # 这一层解决什么问题
//!
//! 用户要的是"**导入即完全恢复**"：导入一份包之后，应用的状态必须与导出那一刻一致。
//! 这个模块把这件事拆成三段各自可验证的责任：
//!
//! 1. [`format`]：包的**格式契约**——manifest 字段、版本号、兼容规则、校验和。
//! 2. [`export`]：把数据目录与设置项打成一份 zip（数据库走 `VACUUM INTO` 在线快照）。
//! 3. [`import`]：把 zip 落回数据目录，走"先备份 → 暂存 → 校验 → 原子替换"的安全链。
//!
//! # 与记忆库/迁移模块的关系
//!
//! 本模块**不依赖 Tauri**（只依赖 `std` + `rusqlite` + `zip` + `serde`），以便备份逻辑
//! 可以在不启动整个应用的情况下被单元测试直接驱动——"导入是否真的完全恢复"必须有
//! 程序化证据，而不是靠人工点界面确认。

pub mod export;
pub mod format;
pub mod import;
pub mod resolve;

pub use export::{create_backup, BackupRequest};
pub use format::{
    BackupError, BackupManifest, ManifestCounts, APP_ID, FORMAT_VERSION_CURRENT, FORMAT_VERSION_MIN,
};
pub use import::{restore_backup, RestoreRequest, RestoreReport};
