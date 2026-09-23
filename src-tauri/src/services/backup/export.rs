//! 备份**导出**：把数据目录的全部内容打成一份 zip。
//!
//! # 关键取舍一：数据库为什么必须走 `VACUUM INTO` 而不是文件复制
//!
//! 应用以 WAL 模式运行。WAL 模式下，已提交但尚未 checkpoint 的记录只存在于
//! `clipboard.db-wal` 里；此时直接复制 `clipboard.db` 会**静默丢掉**这部分数据。
//! `VACUUM INTO '目标路径'` 由 SQLite 自己在事务里导出一份**一致的完整快照**，
//! 包含 WAL 中未 checkpoint 的记录，且不会因为导出期间应用继续写入而被污染
//! （本模块的自证测试 `wal_data_written_without_checkpoint_is_captured` 就是这个断言）。
//!
//! # 关键取舍二：自定义背景图为什么要打包
//!
//! 设置项 `app.custom_background` 存的是**绝对路径**，文件可能在数据目录之外的任意
//! 位置（用户从桌面选的图）。若只导出设置项，导入后那台机器上这个路径不存在，
//! 表现为"背景图丢了，且设置里还留着一个指向不存在文件的路径"。
//!
//! 因此本模块把**数据目录之外**的背景图也打进包里（`background/<sha256>.<ext>`），
//! 并在 `background_map.json` 里记录"原路径 → 包内条目 + 文件名"。导入时把它还原到
//! 新数据目录下的 `background/` 并把设置项改写为**该新路径**——不写回原绝对路径，
//! 因为那台机器上它不一定可写、也不应该被外部程序写。
//!
//! 代价是包会变大（背景图通常几 MB）。这是值得的："完全恢复"包括视觉状态。
//! 若该文件读不到（已删除/无权限），不阻断导出，改为在 manifest 的 `notes` 里
//! 记一条说明，让用户知道自己需要重新设置背景。

use super::format::{
    sha256_file, BackupError, BackupManifest, BackgroundMapEntry, BackgroundMapFile, ManifestCounts,
    APP_ID, ENTRY_ATTACHMENTS_PREFIX, ENTRY_BACKGROUND_MAP, ENTRY_BACKGROUND_PREFIX,
    ENTRY_DATABASE, ENTRY_EMOJI_PREFIX, ENTRY_MANIFEST, ENTRY_PATH_MAP, FORMAT_VERSION_CURRENT,
};
use super::resolve::{collect_local_references, extra_file_plan};
use crate::error::AppError;
use rusqlite::Connection;
use serde::Serialize;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// 导出请求。
#[derive(Debug, Clone)]
pub struct BackupRequest {
    /// 当前数据目录（`app_data_dir` 或重定向后的自定义目录）。
    pub data_dir: PathBuf,
    /// 输出 zip 的绝对路径。
    pub output_path: PathBuf,
    /// 当前应用版本，写入 manifest 供排障。
    pub app_version: String,
}

/// 导出结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupReport {
    /// 产出的 zip 路径。
    pub output_path: String,
    /// 包内条目总数（含 manifest 与目录条目）。
    pub entries_written: u64,
    /// 打包的总字节数（未压缩）。
    pub bytes_written: u64,
    /// manifest 里记录的各项数量。
    pub counts: ManifestCounts,
    /// 导出时数据目录内被跳过的项（理论上为空；非空即提示用户）。
    pub skipped: Vec<String>,
    /// 说明性备注（例如背景图读不到）。
    pub notes: Vec<String>,
    /// 包的 sha256，供用户核对文件是否被改动。
    pub sha256: String,
}

/// 生成一份备份包。
///
/// 全程**只读**数据目录：数据库用 `VACUUM INTO` 导到临时文件，附件/表情目录只做读取。
/// 导出失败时清理临时目录与半成品 zip，不影响现有数据。
/// 对"可能尚不存在"的输出路径做规范化，用于安全检查。
///
/// `canonicalize` 要求路径存在，因此这里规范化**父目录**再把文件名拼回去。
/// 连父目录都取不到时返回 `Err`，调用方按"无法判定"处理（不拦截，避免误伤）。
fn canonicalize_for_guard(p: &std::path::Path) -> Result<std::path::PathBuf, std::io::Error> {
    if let Ok(c) = p.canonicalize() {
        return Ok(c);
    }
    let parent = p.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file = p.file_name().unwrap_or_default();
    Ok(parent.canonicalize()?.join(file))
}

/// 拒绝把备份写进数据目录内部。
///
/// 这条守卫原本只存在于界面侧的导出命令里，于是 MCP 的 `export_backup` **完全绕过**
/// 了它：AI 客户端可以让应用把一个 zip 写到任意路径，包括数据目录内部。
///
/// 危害不是"写到不该写的地方"这么轻：写目标文件会**截断同名文件**，若路径被指向
/// `.../com.tieznext/clipboard.db`，就会把正在使用的数据库截断——不可逆的用户数据
/// 破坏。而且备份包本该是"数据之外的一份副本"，落在数据目录内本身也不合理。
///
/// 放在 `create_backup` 里而不是各调用方：这里是所有导出路径的唯一汇聚点，
/// 谁调用都自动受保护，新增调用方也不必记得补这道检查。
fn guard_output_outside_data_dir(
    data_dir: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<(), BackupError> {
    let (Ok(canon_out), Ok(canon_data)) = (
        canonicalize_for_guard(output_path),
        data_dir.canonicalize(),
    ) else {
        // 无法判定时不拦截：宁可放过也不误伤（例如数据目录本身取不到 canonical）。
        return Ok(());
    };
    if canon_out.starts_with(&canon_data) {
        return Err(BackupError::Land(format!(
            "不能把备份导出到数据目录内部（{}）。请选择数据目录以外的位置，例如「文档」或桌面。",
            data_dir.display()
        )));
    }
    Ok(())
}

pub fn create_backup(req: &BackupRequest) -> Result<BackupReport, BackupError> {
    let data_dir = &req.data_dir;
    if !data_dir.is_dir() {
        return Err(BackupError::Land(format!(
            "数据目录不存在：{}",
            data_dir.display()
        )));
    }

    // 所有导出路径的唯一汇聚点，护栏放这里让界面与 MCP 同时受保护。
    guard_output_outside_data_dir(data_dir, &req.output_path)?;

    // 临时工作目录放在输出文件同级的 `.tmp` 下，保证 rename 在同一文件系统内。
    let work_dir = temp_work_dir(&req.output_path);
    if work_dir.exists() {
        let _ = std::fs::remove_dir_all(&work_dir);
    }
    std::fs::create_dir_all(&work_dir)?;

    // 【为什么不直接写 output_path】写目标文件用 File::create 会**立即截断**同名文件。
    // 若用户选的路径正好已存在一份有效备份（默认文件名只精确到秒，重复导出极易撞名），
    // 导出一旦失败，"失败即清场"就会把那份**用户已有的备份也删掉**。
    // 因此先写到同目录的 `.tmp` 文件，成功后才 `rename` 覆盖到目标——失败时只删自己的
    // 临时文件，用户的既有文件一个字节都不动。
    let staging_out = temp_output_path(&req.output_path);
    let mut staged_req = req.clone();
    staged_req.output_path = staging_out.clone();

    let result = build_package(&staged_req, &work_dir);

    match result {
        Ok(mut report) => {
            // 原子落位：rename 在同一文件系统内是原子的，因此目标路径上要么是旧文件、
            // 要么是完整的新文件，不会出现半截 zip。
            if let Err(e) = std::fs::rename(&staging_out, &req.output_path) {
                let _ = std::fs::remove_file(&staging_out);
                let _ = std::fs::remove_dir_all(&work_dir);
                return Err(BackupError::Land(format!(
                    "备份已生成但无法写入目标路径 {}（{}）。你原有的文件未被改动。",
                    req.output_path.display(),
                    e
                )));
            }
            let digest = sha256_file(&req.output_path)?;
            report.output_path = req.output_path.to_string_lossy().to_string();
            report.sha256 = digest;
            Ok(report)
        }
        Err(e) => {
            // 失败即清场：只清自己的临时文件与工作目录，**绝不碰**目标路径上的既有文件。
            let _ = std::fs::remove_dir_all(&work_dir);
            let _ = std::fs::remove_file(&staging_out);
            Err(e)
        }
    }
}

/// 导出时的临时输出路径：与目标文件同目录（保证 rename 同文件系统）。
fn temp_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "backup.zip".to_string());
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{}.writing-{}-{}", name, std::process::id(), seq))
}

fn build_package(req: &BackupRequest, work_dir: &Path) -> Result<BackupReport, BackupError> {
    let data_dir = &req.data_dir;
    let db_path = data_dir.join("clipboard.db");
    let mut notes: Vec<String> = Vec::new();
    let mut checksums: std::collections::BTreeMap<String, String> = Default::default();

    // ---------------- 1. 数据库：VACUUM INTO 在线快照 ----------------
    let snapshot_path = work_dir.join("snapshot.db");
    let schema_version;
    let counts: ManifestCounts;
    {
        let conn = Connection::open(&db_path)
            .map_err(|e| BackupError::Land(format!("无法打开数据库：{}", e)))?;
        // WAL 模式下读连接也可能需要写侧车文件；journal_mode 保持现状即可。
        // VACUUM INTO 要求目标文件不存在。
        let target = snapshot_path.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!("VACUUM INTO '{}'", target))
            .map_err(|e| BackupError::Land(format!("数据库快照失败（VACUUM INTO）：{}", e)))?;

        schema_version = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);

        // 数量统计放在**快照库**上做：这样 manifest 记录的就是包里那一份的数量，
        // 而不是"统计时"加"打包时"两个不同时刻的混合结果。
        let snap = Connection::open(&snapshot_path)
            .map_err(|e| BackupError::Land(format!("无法打开快照库：{}", e)))?;
        counts = ManifestCounts {
            entries: count_rows(&snap, "clipboard_history")?,
            tags: count_rows(&snap, "saved_tags")?,
            attachments: count_files(&data_dir.join("attachments"))?,
            emoji_favorites: count_files(&data_dir.join("emoji_favorites"))?,
            background: count_files(&data_dir.join("background"))?,
            settings: count_rows(&snap, "settings")?,
        };
    }
    checksums.insert(ENTRY_DATABASE.to_string(), sha256_file(&snapshot_path)?);

    // ---------------- 2. 数据目录之外的引用（自定义背景图）----------------
    let extra_files = plan_extra_files(&db_path, data_dir, &mut notes)?;

    // ---------------- 3. 写 zip ----------------
    let out = std::fs::File::create(&req.output_path)?;
    let mut zip = ZipWriter::new(std::io::BufWriter::new(out));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut entries_written: u64 = 0;
    let mut bytes_written: u64 = 0;

    // 3.1 先写数据库快照（已经高度压缩，用 Stored 省 CPU）
    write_file_entry(&mut zip, stored, ENTRY_DATABASE, &snapshot_path)?;
    entries_written += 1;
    bytes_written += std::fs::metadata(&snapshot_path)?.len();

    // 3.2 附件目录（递归）
    let (n, b) = write_dir_recursive(
        &mut zip,
        deflated,
        &data_dir.join("attachments"),
        ENTRY_ATTACHMENTS_PREFIX,
        &mut checksums,
    )?;
    entries_written += n;
    bytes_written += b;

    // 3.3 表情收藏目录（磁盘那一份）
    let (n, b) = write_dir_recursive(
        &mut zip,
        deflated,
        &data_dir.join("emoji_favorites"),
        ENTRY_EMOJI_PREFIX,
        &mut checksums,
    )?;
    entries_written += n;
    bytes_written += b;

    // 3.35 背景图目录（`data_dir/background/`）递归打包
    //
    // 【为什么必须有这一步】导入会把背景图还原到 `data_dir/background/`，并把设置项
    // 指向那里。若导出端不打包这个目录，就会出现"导出 → 导入 → **再导出**"的**不闭合**：
    // 第二次导出既不会把它当"数据目录之外的额外文件"（它在数据目录内），又因为缺这次
    // 递归而不进包 —— 包内没有任何背景图，而再导入时 staging 里 copy_tree 复制来的旧
    // 背景文件还在，界面照旧显示背景（**假成功**），换台机器才暴露丢失，且两侧都不报警。
    let (n, b) = write_dir_recursive(
        &mut zip,
        deflated,
        &data_dir.join("background"),
        ENTRY_BACKGROUND_PREFIX,
        &mut checksums,
    )?;
    entries_written += n;
    bytes_written += b;

    // 3.4 背景图的映射与"不在数据目录内"的额外文件（本版新增的附加条目，旧读取端会跳过）
    if !extra_files.is_empty() {
        let mut map_entries: Vec<BackgroundMapEntry> = Vec::new();
        for extra in &extra_files {
            let bytes = match std::fs::read(&extra.original) {
                Ok(b) => b,
                Err(e) => {
                    notes.push(format!(
                        "自定义背景图无法读取，未打包（导入后需重新设置背景）：{} —— {}",
                        extra.original.display(),
                        e
                    ));
                    continue;
                }
            };
            let digest = super::format::sha256_bytes(&bytes);
            // 已在 `background/` 目录递归里写过的不重复写，只登记映射
            // （条目名由 `extra_file_plan` 保证与递归结果一致）。
            if !extra.already_packed {
                zip.start_file(extra.entry.clone(), deflated)
                    .map_err(|e| BackupError::Land(format!("写入 zip 条目失败：{}", e)))?;
                zip.write_all(&bytes)
                    .map_err(|e| BackupError::Land(format!("写入 zip 数据失败：{}", e)))?;
                checksums.insert(extra.entry.clone(), digest.clone());
                entries_written += 1;
                bytes_written += bytes.len() as u64;
            }
            map_entries.push(BackgroundMapEntry {
                original_path: extra.original.to_string_lossy().to_string(),
                entry: extra.entry.clone(),
                sha256: digest,
                file_name: extra.file_name.clone(),
            });
        }
        if !map_entries.is_empty() {
            let payload = serde_json::to_vec_pretty(&BackgroundMapFile {
                map_version: 1,
                items: map_entries,
            })
            .map_err(|e| BackupError::Land(format!("序列化背景图映射失败：{}", e)))?;
            zip.start_file(ENTRY_BACKGROUND_MAP, deflated)
                .map_err(|e| BackupError::Land(format!("写入 zip 条目失败：{}", e)))?;
            zip.write_all(&payload)
                .map_err(|e| BackupError::Land(format!("写入 zip 数据失败：{}", e)))?;
            checksums.insert(ENTRY_BACKGROUND_MAP.to_string(), super::format::sha256_bytes(&payload));
            entries_written += 1;
            bytes_written += payload.len() as u64;
        }
    }

    // 3.5 路径映射表：记录"数据目录内相对路径 → 导出机器的绝对路径"，
    //     导入侧据此把数据库里的绝对路径改写到当前数据目录。
    {
        let conn = Connection::open(&snapshot_path)
            .map_err(|e| BackupError::Land(format!("无法打开快照库：{}", e)))?;
        let mappings = collect_local_references(&conn, data_dir)
            .map_err(|e| BackupError::Land(format!("收集路径引用失败：{}", e)))?;
        if !mappings.items.is_empty() {
            let payload = serde_json::to_vec_pretty(&mappings)
                .map_err(|e| BackupError::Land(format!("序列化路径映射失败：{}", e)))?;
            zip.start_file(ENTRY_PATH_MAP, deflated)
                .map_err(|e| BackupError::Land(format!("写入 zip 条目失败：{}", e)))?;
            zip.write_all(&payload)
                .map_err(|e| BackupError::Land(format!("写入 zip 数据失败：{}", e)))?;
            checksums.insert(
                ENTRY_PATH_MAP.to_string(),
                super::format::sha256_bytes(&payload),
            );
            entries_written += 1;
            bytes_written += payload.len() as u64;
        }
    }

    // 3.6 最后写 manifest（放在最后是因为它要记录前面所有条目的校验和）
    let manifest = BackupManifest {
        format_version: FORMAT_VERSION_CURRENT,
        app: APP_ID.to_string(),
        app_version: req.app_version.clone(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        schema_version,
        counts: counts.clone(),
        checksums: checksums.clone(),
        notes: notes.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| BackupError::Land(format!("序列化 manifest 失败：{}", e)))?;
    zip.start_file(ENTRY_MANIFEST, deflated)
        .map_err(|e| BackupError::Land(format!("写入 zip 条目失败：{}", e)))?;
    zip.write_all(&manifest_bytes)
        .map_err(|e| BackupError::Land(format!("写入 zip 数据失败：{}", e)))?;
    entries_written += 1;
    bytes_written += manifest_bytes.len() as u64;

    zip.finish()
        .map_err(|e| BackupError::Land(format!("收尾 zip 失败：{}", e)))?;

    // 收尾：删掉临时工作目录（里面有数据库快照副本）。
    let _ = std::fs::remove_dir_all(work_dir);

    let digest = sha256_file(&req.output_path)?;

    Ok(BackupReport {
        output_path: req.output_path.to_string_lossy().to_string(),
        entries_written,
        bytes_written,
        counts,
        skipped: Vec::new(),
        notes,
        sha256: digest,
    })
}

/// 找出"必须随包带走但不在数据目录内"的文件（当前只有自定义背景图）。
fn plan_extra_files(
    db_path: &Path,
    data_dir: &Path,
    notes: &mut Vec<String>,
) -> Result<Vec<super::resolve::ExtraFile>, BackupError> {
    let conn = Connection::open(db_path)
        .map_err(|e| BackupError::Land(format!("无法打开数据库：{}", e)))?;
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'app.custom_background'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok();
    let Some(raw) = raw else { return Ok(Vec::new()) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let path = PathBuf::from(trimmed);

    // 无论在数据目录内还是外，都要**登记映射**——否则导入端不知道设置项该指向哪。
    // 落在 `background/` 内的文件由目录递归打包（already_packed=true，不重复写）；
    // 落在数据目录其它位置（例如用户手工放进 attachments/ 的背景图）按同样规则处理，
    // 由目录递归覆盖；只有**数据目录之外**的文件才需要额外写入 zip 条目。
    if !path.is_file() {
        notes.push(format!(
            "自定义背景图文件不存在或不可读，未打包（导入后需重新设置背景）：{}",
            path.display()
        ));
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| BackupError::Land(format!("读取背景图失败：{}", e)))?;
    Ok(vec![extra_file_plan(&path, &bytes, Some(data_dir))])
}

fn write_dir_recursive<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    opts: SimpleFileOptions,
    dir: &Path,
    prefix: &str,
    checksums: &mut std::collections::BTreeMap<String, String>,
) -> Result<(u64, u64), BackupError> {
    if !dir.is_dir() {
        return Ok((0, 0));
    }
    let mut entries = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut children: Vec<PathBuf> = std::fs::read_dir(&current)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        // 排序保证同一份数据两次导出得到**条目顺序一致**的包（便于比对与测试）。
        children.sort();
        for path in children {
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue; // 符号链接等特殊类型跳过，不跟随
            }
            let rel = path
                .strip_prefix(dir)
                .map_err(|_| BackupError::Land("条目路径异常".to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            let name = format!("{}{}", prefix, rel);
            let size = std::fs::metadata(&path)?.len();
            zip.start_file(name.clone(), opts)
                .map_err(|e| BackupError::Land(format!("写入 zip 条目失败：{}", e)))?;
            let mut f = std::fs::File::open(&path)?;
            let mut buf = vec![0u8; 64 * 1024];
            let mut hasher = sha2::Sha256::new();
            use sha2::Digest;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                zip.write_all(&buf[..n])
                    .map_err(|e| BackupError::Land(format!("写入 zip 数据失败：{}", e)))?;
            }
            checksums.insert(name, format!("sha256:{:x}", hasher.finalize()));
            entries += 1;
            bytes += size;
        }
    }
    Ok((entries, bytes))
}

fn write_file_entry<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    opts: SimpleFileOptions,
    name: &str,
    path: &Path,
) -> Result<(), BackupError> {
    zip.start_file(name.to_string(), opts)
        .map_err(|e| BackupError::Land(format!("写入 zip 条目失败：{}", e)))?;
    let mut f = std::fs::File::open(path)?;
    std::io::copy(&mut f, zip)
        .map_err(|e| BackupError::Land(format!("写入 zip 数据失败：{}", e)))?;
    Ok(())
}

fn count_rows(conn: &Connection, table: &str) -> Result<u64, BackupError> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v as u64)
    .map_err(|e| BackupError::Land(format!("统计 {} 失败：{}", table, e)))
}

fn count_files(dir: &Path) -> Result<u64, BackupError> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(entry.path());
            } else if ty.is_file() {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// 临时工作目录：输出文件同级的 `.<名>.tmp-<pid>`。
fn temp_work_dir(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "backup.zip".to_string());
    parent.join(format!(".{}.tmp-{}", name, std::process::id()))
}

/// 供命令层复用：把 [`BackupError`] 转成项目统一的 [`AppError`]。
pub fn to_app_error(e: BackupError) -> AppError {
    AppError::Validation(e.to_string())
}

/// 供导入层复用的小工具：按条目名判断它属于哪一类。
pub fn classify_entry(name: &str) -> EntryKind {
    if name == ENTRY_MANIFEST {
        EntryKind::Manifest
    } else if name == ENTRY_DATABASE {
        EntryKind::Database
    } else if name == ENTRY_PATH_MAP {
        EntryKind::PathMap
    } else if name == ENTRY_BACKGROUND_MAP {
        EntryKind::BackgroundMap
    } else if name.starts_with(ENTRY_ATTACHMENTS_PREFIX) {
        EntryKind::Attachment
    } else if name.starts_with(ENTRY_EMOJI_PREFIX) {
        EntryKind::Emoji
    } else if name.starts_with(ENTRY_BACKGROUND_PREFIX) {
        EntryKind::BackgroundFile
    } else {
        EntryKind::Unknown
    }
}

/// zip 条目的归类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Manifest,
    Database,
    PathMap,
    BackgroundMap,
    Attachment,
    Emoji,
    BackgroundFile,
    Unknown,
}
#[cfg(test)]
mod guard_tests {
    use super::*;

    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("tiez-guard-{tag}-{}", std::process::id()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        (root, data)
    }

    /// 守卫函数本身：数据目录内必须被拒。
    #[test]
    fn refuses_output_inside_the_data_directory() {
        let (root, data) = scratch("in");
        assert!(guard_output_outside_data_dir(&data, &data.join("backup.zip")).is_err());
        assert!(
            guard_output_outside_data_dir(&data, &data.join("clipboard.db")).is_err(),
            "指向 clipboard.db 会截断数据库"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 守卫函数本身：数据目录之外必须放行。
    #[test]
    fn allows_output_outside_the_data_directory() {
        let (root, data) = scratch("out");
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        assert!(guard_output_outside_data_dir(&data, &docs.join("backup.zip")).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **守卫确实接线到 `create_backup`**。
    ///
    /// 前两条只测了守卫函数本身：把它们单独留下、却把 `create_backup` 里的调用删掉，
    /// 它们**依然全绿**——而"护栏没接上"正是这次要修的缺陷（界面有、MCP 没有）。
    /// 所以必须从这里走一遍真实入口，用返回值证明它被拦住了。
    #[test]
    fn create_backup_actually_applies_the_guard() {
        let (root, data) = scratch("wired");
        let req = BackupRequest {
            data_dir: data.clone(),
            output_path: data.join("backup.zip"),
            app_version: "0.0.0-test".to_string(),
        };

        let err = create_backup(&req).expect_err(
            "create_backup 必须自己拦住落在数据目录内的输出路径，而不是指望每个调用方各自记得检查（MCP 就漏了）",
        );

        // 断言**是哪一条**拒绝的，而不是"反正失败了"。
        //
        // 只断言 is_err() 是不够的：数据目录是空的，create_backup 会因为别的原因失败，
        // 于是护栏即使被删掉，这里也照样"通过"。必须认准护栏自己的那条消息。
        let msg = err.to_string();
        assert!(
            msg.contains("数据目录内部"),
            "应当由护栏拒绝，实际错误却是：{msg}"
        );
        assert!(
            !data.join("backup.zip").exists(),
            "被拒绝时不应留下任何文件"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
