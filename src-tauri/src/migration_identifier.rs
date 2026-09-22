//! 应用标识符变更的数据目录迁移（`com.tiez` / `com.tiez.app` → `com.tieznext`）。
//!
//! ## 为什么需要它
//!
//! Tauri 的应用数据目录由 `tauri.conf.json` 的 `identifier` 推导。本项目从上游
//! TieZ 改名而来，`identifier` 由 `com.tiez.app`（Windows 主线 v0.3.1–v0.3.3）
//! 与 `com.tiez`（macOS/beta 分支）改为 `com.tieznext`。若不做处理，老用户升级后
//! 会看到一个空的新目录，历史剪贴板、标签、附件与表情收藏全部"消失"（实际仍在旧
//! 目录里，但应用不再读取）。
//!
//! 既有的 `perform_migration_v028`（`贴汁` → `TieZ`）只处理更早的一次改名，不覆盖
//! 标识符变更，因此单独实现本模块。
//!
//! ## 安全契约（硬约束，不得放宽）
//!
//! 用户明确要求「十分稳健，即使迁移失败也不会损失原数据」。据此：
//!
//! 1. **源目录全程只读**——不删除、不改名、不写入源内任何文件；
//! 2. **先暂存后交付**——先完整复制到独立暂存目录，校验一致后才提升为正式目录，
//!    避免半成品被当成有效数据；
//! 3. **失败即回滚**——任何一步失败都只清理暂存目录，源目录与既有目标目录保持不变；
//! 4. **目标已有数据则不迁移**——绝不覆盖既有用户数据（宁可少迁，不可覆盖）；
//! 5. **迁移成功后也不删除源目录**——保留为可回退副本，由用户自行清理。
//!
//! 第 1 与第 5 条共同保证：**本模块在最坏情况下只会"没迁成"，不会造成数据丢失**。
//!
//! ## 依赖约束
//!
//! 本模块**只依赖 `std`**，不引用 crate 内任何其他模块。这样它可以脱离 Tauri 与
//! Windows 专用代码独立编译与测试（本 crate 在 Linux 上因 Windows 代码缺 cfg 门控
//! 而无法整体编译），从而使迁移逻辑能够被真实文件操作验证。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 历史标识符对应的目录名。
///
/// 只认这个白名单，不做前缀/模糊匹配——避免误伤同级的其他应用目录。
/// - `com.tiez.app`：Windows 主线 v0.3.1–v0.3.3
/// - `com.tiez`：macOS 与 beta 分支
pub const LEGACY_IDENTIFIERS: &[&str] = &["com.tiez.app", "com.tiez"];

/// 迁移是否成功由"新目录存在可用数据库"作为最终判据。
const DB_FILE: &str = "clipboard.db";

/// 一次迁移的结果。调用方据此决定是否继续做数据库内路径重写。
#[derive(Debug)]
pub enum MigrationOutcome {
    /// 无需迁移（无旧目录、目标已有数据、路径重合等）。
    Skipped(SkipReason),
    /// 迁移成功。`source` 仍然保留，未被删除。
    Migrated {
        source: PathBuf,
        target: PathBuf,
        files: u64,
        bytes: u64,
    },
    /// 迁移失败。源目录与既有目标目录均未被破坏。
    Failed { source: PathBuf, error: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// 新目录下已有数据库——用户已在用新版本，不能覆盖。
    TargetAlreadyHasData,
    /// 未找到任何旧目录。
    NoLegacyDir,
    /// 旧目录与新目录是同一个路径（防御性检查）。
    SamePath,
    /// 旧目录存在但不是目录。
    NotADirectory,
}

/// 由新数据目录推导同名父目录下的历史目录候选。
///
/// 例如新目录为 `%APPDATA%\com.tieznext`，则返回
/// `[%APPDATA%\com.tiez.app, %APPDATA%\com.tiez]`。
pub fn legacy_dirs_for(new_dir: &Path) -> Vec<PathBuf> {
    let Some(parent) = new_dir.parent() else {
        return Vec::new();
    };
    LEGACY_IDENTIFIERS
        .iter()
        .map(|id| parent.join(id))
        .collect()
}

/// 迁移入口：扫描历史目录并把数据安全地迁到 `new_dir`。
///
/// 本函数**不返回致命错误**：迁移失败只记录并返回 [`MigrationOutcome::Failed`]，
/// 绝不阻断应用启动。调用方应把"是否成功"仅用于决定后续的可选动作（如数据库内
/// 绝对路径重写）。
pub fn migrate_legacy_identifier_data(new_dir: &Path) -> MigrationOutcome {
    // 逐个候选尝试，第一个成功的即返回。若某个候选失败，继续尝试下一个候选
    // （不同候选对应不同平台的旧标识符，互不影响）。
    let mut last_failure: Option<MigrationOutcome> = None;

    for legacy in legacy_dirs_for(new_dir) {
        match migrate_from(&legacy, new_dir) {
            Outcome::Preserve => {}
            Outcome::Skipped(r) => return MigrationOutcome::Skipped(r),
            Outcome::Migrated {
                files,
                bytes,
            } => {
                return MigrationOutcome::Migrated {
                    source: legacy,
                    target: new_dir.to_path_buf(),
                    files,
                    bytes,
                }
            }
            Outcome::Failed(error) => {
                last_failure = Some(MigrationOutcome::Failed {
                    source: legacy,
                    error,
                });
            }
        }
    }

    match last_failure {
        Some(f) => f,
        None => MigrationOutcome::Skipped(SkipReason::NoLegacyDir),
    }
}

/// 内部三态：无此候选 / 跳过 / 成功 / 失败。
enum Outcome {
    /// 该候选不存在或无需处理，继续试下一个。
    Preserve,
    Skipped(SkipReason),
    Migrated { files: u64, bytes: u64 },
    Failed(String),
}

fn migrate_from(source: &Path, target: &Path) -> Outcome {
    // ---- 前置检查：任何一项不满足都保持原状 ----
    if !source.exists() {
        return Outcome::Preserve;
    }
    if !source.is_dir() {
        return Outcome::Failed(format!("源路径不是目录: {}", source.display()));
    }
    if source == target {
        return Outcome::Skipped(SkipReason::SamePath);
    }
    // 目标已有数据库 -> 用户已经在用新版本，绝不覆盖。
    if target.join(DB_FILE).exists() {
        return Outcome::Skipped(SkipReason::TargetAlreadyHasData);
    }

    // ---- 统计源目录（只读）----
    let source_entries = match scan_tree(source) {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(format!("读取源目录失败: {}", e)),
    };
    if source_entries.is_empty() {
        // 空目录没有迁移价值，也不删它。
        return Outcome::Preserve;
    }

    // ---- 记录"迁移前目标里就已存在"的条目 ----
    // 这些条目不属于本次交付范围：既不覆盖它们，交付后也不拿它们的
    // 大小去和源比对（用户可能已在其中写入了更新的内容）。
    let preexisting: std::collections::HashSet<String> = if target.exists() {
        match scan_tree(target) {
            Ok(v) => v.into_iter().map(|(k, _)| k).collect(),
            Err(e) => return Outcome::Failed(format!("读取既有目标目录失败: {}", e)),
        }
    } else {
        std::collections::HashSet::new()
    };

    // ---- 第一步：复制到独立暂存目录 ----
    // 暂存目录放在目标同级，保证后续 rename 是同一文件系统内的原子操作。
    let staging = staging_dir(target);
    // 清理上次崩溃残留的暂存目录（它从未被提升，删掉是安全的）。
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    if let Err(e) = copy_tree(source, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Outcome::Failed(format!("复制到暂存目录失败: {}", e));
    }

    // ---- 第二步：校验一致后才允许交付 ----
    match scan_tree(&staging) {
        Ok(staged) => {
            if staged != source_entries {
                let _ = fs::remove_dir_all(&staging);
                return Outcome::Failed(format!(
                    "暂存副本与源不一致（源 {} 项 / 暂存 {} 项），已放弃本次迁移，源数据未改动",
                    source_entries.len(),
                    staged.len()
                ));
            }
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Outcome::Failed(format!("校验暂存目录失败: {}", e));
        }
    }

    // ---- 第三步：交付（提升暂存目录到目标）----
    if let Err(e) = promote(&staging, target) {
        let _ = fs::remove_dir_all(&staging);
        return Outcome::Failed(format!("提升到目标目录失败: {}", e));
    }

    // ---- 第四步：交付后复核 ----
    // 判据分两类：
    //   * 本次新交付的项：必须存在且大小与源一致；
    //   * 迁移前目标里就已存在的项：只要求仍然存在，不比对大小
    //     （用户可能已在其中写入了比源更新的内容，覆盖它才是错的）。
    match verify_delivered(target, &source_entries, &preexisting) {
        Ok(()) => {}
        Err(e) => {
            // 复核失败时不回删目标——目标里的数据是从源复制来的，删掉目标同样
            // 不合理；源目录始终未动，用户数据仍完整可回退。
            return Outcome::Failed(format!(
                "交付后复核未通过: {}（源目录保持完整，未删除任何数据）",
                e
            ));
        }
    }

    let bytes = source_entries.iter().map(|(_, s)| *s).sum();
    Outcome::Migrated {
        files: source_entries.len() as u64,
        bytes,
    }
}

/// 暂存目录路径：目标同级，名字带 pid 以便并发/崩溃区分。
fn staging_dir(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{}.migrating.{}", name, std::process::id()))
}

/// 把暂存目录交付到目标位置。
///
/// - 目标不存在：直接 `rename`（同一文件系统内为原子操作）。
/// - 目标已存在：**递归合并，逐文件判断**，已存在的文件一律保留不覆盖。
///
/// 注意必须递归进同名目录：目标里若已有 `attachments/` 目录，不能因为目录本身
/// 存在就跳过，否则该目录下源中独有的文件永远补不齐。
fn promote(staging: &Path, target: &Path) -> io::Result<()> {
    if !target.exists() {
        return fs::rename(staging, target);
    }
    merge_into(staging, target)?;
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

/// 递归合并 `src` 到 `dst`，**绝不覆盖 `dst` 中已存在的文件**。
///
/// 优先用 `rename`（同文件系统内高效且原子）；跨设备失败时退回"复制 + 删除副本"，
/// 其中删除的始终是暂存侧副本，`src` 原始数据不受影响。
fn merge_into(src: &Path, dst: &Path) -> io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;

        if ty.is_dir() {
            if to.exists() {
                // 目录已存在：递归进去继续补齐，而不是整目录跳过
                merge_into(&from, &to)?;
            } else {
                fs::rename(&from, &to).or_else(|_| {
                    copy_tree(&from, &to)?;
                    fs::remove_dir_all(&from)
                })?;
            }
        } else if ty.is_file() {
            if to.exists() {
                continue; // 绝不覆盖既有文件
            }
            fs::rename(&from, &to).or_else(|_| {
                fs::copy(&from, &to)?;
                fs::remove_file(&from)
            })?;
        }
        // 符号链接等特殊类型一律跳过：不跟随、不复制。
    }
    Ok(())
}

/// 递归复制。只读源，只写目标。
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
        }
        // 符号链接等特殊类型一律跳过：不跟随、不复制，避免把外部路径卷进来。
    }
    Ok(())
}

/// 扫描目录树，返回按相对路径排序的 `(相对路径, 字节数)` 列表。
///
/// 用于迁移前后的**内容一致性**判定：条数与每项大小都相同才认为一致。
fn scan_tree(root: &Path) -> io::Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    collect(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/"); // 统一分隔符，便于跨平台比较
        if ty.is_dir() {
            out.push((format!("{}/", rel), 0));
            collect(root, &path, out)?;
        } else if ty.is_file() {
            out.push((rel, entry.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }
    Ok(())
}

/// 交付后复核。
///
/// - `expected`：源目录的全部条目（相对路径 + 大小）。
/// - `preexisting`：迁移前目标里就已存在的条目集合；这些只验存在性，不验大小。
fn verify_delivered(
    target: &Path,
    expected: &[(String, u64)],
    preexisting: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let actual: Vec<(String, u64)> = scan_tree(target).map_err(|e| e.to_string())?;
    let map: std::collections::HashMap<&str, u64> =
        actual.iter().map(|(k, v)| (k.as_str(), *v)).collect();

    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for (rel, size) in expected {
        match map.get(rel.as_str()) {
            None => missing.push(rel.clone()),
            Some(actual_size) => {
                // 预先存在且未被本次覆盖的条目，允许与源大小不同
                if actual_size != size && !preexisting.contains(rel) {
                    mismatched.push(format!("{} (源 {} / 目标 {})", rel, size, actual_size));
                }
            }
        }
    }
    if missing.is_empty() && mismatched.is_empty() {
        return Ok(());
    }
    Err(format!(
        "缺失 {} 项{:?}，大小不符 {} 项{:?}",
        missing.len(),
        missing.iter().take(5).collect::<Vec<_>>(),
        mismatched.len(),
        mismatched.iter().take(5).collect::<Vec<_>>()
    ))
}

// ---------------------------------------------------------------------------
// 测试：使用真实文件系统操作，覆盖成功路径与各类失败路径。
// 这些测试只依赖 std，可在独立 harness 中运行（见 README/提交说明）。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-mig-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个典型的旧数据目录：数据库 + WAL + 日志 + 附件 + 表情收藏 + 重定向文件。
    fn seed_legacy(dir: &Path) {
        fs::create_dir_all(dir.join("attachments")).unwrap();
        fs::create_dir_all(dir.join("emoji_favorites")).unwrap();
        fs::write(dir.join(DB_FILE), vec![b'x'; 4096]).unwrap();
        fs::write(dir.join("clipboard.db-wal"), vec![b'w'; 512]).unwrap();
        fs::write(dir.join("clipboard.db-shm"), vec![b's'; 128]).unwrap();
        fs::write(dir.join("tiez.log"), b"log line\n").unwrap();
        fs::write(dir.join("datapath.txt"), b"").unwrap();
        fs::write(dir.join("attachments/a.png"), vec![b'a'; 1000]).unwrap();
        fs::write(dir.join("emoji_favorites/e.json"), b"[]").unwrap();
    }

    #[test]
    fn migrates_all_files_and_keeps_source() {
        let root = tmp("ok");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        let before = scan_tree(&legacy).unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        match outcome {
            MigrationOutcome::Migrated { files, .. } => assert_eq!(files, before.len() as u64),
            other => panic!("应迁移成功，实际 {:?}", other),
        }
        // 目标内容与源逐项一致
        assert_eq!(scan_tree(&target).unwrap(), before);
        // 源目录必须仍然完好（安全契约第 5 条）
        assert!(legacy.exists() && legacy.join(DB_FILE).exists());
        assert_eq!(scan_tree(&legacy).unwrap(), before);
        // 暂存目录不残留
        assert!(!staging_dir(&target).exists());
    }

    #[test]
    fn skips_when_target_already_has_data() {
        let root = tmp("skip-data");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(DB_FILE), b"existing-user-data").unwrap();
        let target_before = scan_tree(&target).unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
        ));
        // 既有目标数据一字未改
        assert_eq!(scan_tree(&target).unwrap(), target_before);
        // 源数据也一字未改
        assert!(legacy.join(DB_FILE).exists());
    }

    #[test]
    fn skips_when_no_legacy_dir() {
        let root = tmp("none");
        let target = root.join("com.tieznext");
        let outcome = migrate_legacy_identifier_data(&target);
        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::NoLegacyDir)
        ));
        assert!(!target.exists());
    }

    #[test]
    fn is_idempotent_and_never_overwrites() {
        let root = tmp("idem");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);

        assert!(matches!(
            migrate_legacy_identifier_data(&target),
            MigrationOutcome::Migrated { .. }
        ));
        let after_first = scan_tree(&target).unwrap();
        // 用户在新版本里继续写入
        fs::write(target.join(DB_FILE), b"newer-user-data-longer").unwrap();

        // 第二次运行：目标已有数据库 -> 跳过，绝不覆盖
        let outcome = migrate_legacy_identifier_data(&target);
        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
        ));
        assert_eq!(
            fs::read(target.join(DB_FILE)).unwrap(),
            b"newer-user-data-longer"
        );
        // 源目录依旧完好
        assert_eq!(scan_tree(&legacy).unwrap().len(), after_first.len());
    }

    #[test]
    fn merges_into_existing_target_without_overwriting() {
        let root = tmp("merge");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        // 目标已存在，但没有数据库（例如只有窗口状态之类的零散文件）
        fs::create_dir_all(target.join("attachments")).unwrap();
        fs::write(target.join("existing-keep.txt"), b"do-not-clobber").unwrap();
        fs::write(target.join("attachments/a.png"), b"TARGET-KEEPS-THIS").unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        // 目标原有的文件未被覆盖
        assert_eq!(
            fs::read(target.join("existing-keep.txt")).unwrap(),
            b"do-not-clobber"
        );
        assert_eq!(
            fs::read(target.join("attachments/a.png")).unwrap(),
            b"TARGET-KEEPS-THIS"
        );
        // 源中独有、目标缺失的项被补齐
        assert!(target.join(DB_FILE).exists());
        assert!(target.join("emoji_favorites/e.json").exists());
        // 源完好
        assert!(legacy.join(DB_FILE).exists());
    }

    #[test]
    fn picks_com_tiez_when_only_that_exists() {
        let root = tmp("alt");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez");
        seed_legacy(&legacy);

        let outcome = migrate_legacy_identifier_data(&target);

        match outcome {
            MigrationOutcome::Migrated { source, .. } => {
                assert_eq!(source.file_name().unwrap(), "com.tiez")
            }
            other => panic!("应迁移成功，实际 {:?}", other),
        }
        assert!(target.join(DB_FILE).exists());
    }

    #[test]
    fn ignored_when_source_is_a_file() {
        let root = tmp("file");
        let target = root.join("com.tieznext");
        fs::write(root.join("com.tiez.app"), b"not a dir").unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        // 不是目录：不迁移、不破坏，且不会把该文件删掉
        assert!(root.join("com.tiez.app").exists());
        assert!(!target.exists());
        assert!(matches!(
            outcome,
            MigrationOutcome::Failed { .. } | MigrationOutcome::Skipped(_)
        ));
    }

    #[test]
    fn failed_verification_leaves_source_intact() {
        // 用一个不可写的目标父目录制造交付失败：源必须完好。
        let root = tmp("failverify");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        let before = scan_tree(&legacy).unwrap();

        // 目标父路径是一个"文件"，无法创建目录 -> 交付必然失败
        let bogus_parent = root.join("blocked");
        fs::write(&bogus_parent, b"i am a file").unwrap();
        let target = bogus_parent.join("com.tieznext");

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(
            outcome,
            MigrationOutcome::Failed { .. } | MigrationOutcome::Skipped(_)
        ));
        // 核心断言：源数据一字未改
        assert_eq!(scan_tree(&legacy).unwrap(), before);
    }

    /// 回归测试：目标里已有同名子目录时，必须递归进该目录补齐源中独有的文件。
    /// 此前的实现"目录已存在就整目录跳过"，会导致这些文件永远补不齐。
    #[test]
    fn merges_into_preexisting_subdir_without_skipping_it() {
        let root = tmp("subdir");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);

        // 目标里已有一个同名子目录，但里面只有别的文件
        fs::create_dir_all(target.join("attachments")).unwrap();
        fs::write(target.join("attachments/other.png"), b"preexisting").unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        // 源中独有的 attachments/a.png 必须被补齐进已存在的子目录
        assert!(
            target.join("attachments/a.png").exists(),
            "同名子目录内的缺失文件必须被递归补齐"
        );
        // 目标原有的文件保留
        assert_eq!(
            fs::read(target.join("attachments/other.png")).unwrap(),
            b"preexisting"
        );
        // 源完好
        assert!(legacy.join("attachments/a.png").exists());
    }

    #[test]
    fn scan_tree_is_stable_and_uses_forward_slashes() {
        let root = tmp("scan");
        seed_legacy(&root);
        let a = scan_tree(&root).unwrap();
        let b = scan_tree(&root).unwrap();
        assert_eq!(a, b);
        assert!(a.iter().all(|(k, _)| !k.contains('\\')));
        // 目录项以 / 结尾
        assert!(a.iter().any(|(k, _)| k == "attachments/"));
    }
}
