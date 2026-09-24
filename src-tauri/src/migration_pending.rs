//! 「待接管」标记：让一次迁移**跨越一次应用重启**完成交接。
//!
//! # 为什么必须有这个模块（这不是设计偏好，是平台限制推出来的必然）
//!
//! 迁移入口是**应用内的界面**：用户点它的时候，应用**必定正在运行**。而应用启动时
//! 就已经打开了目标数据目录里的 `clipboard.db`（`app/setup.rs` 的 `database::init_db`），
//! 这个连接常驻在 `DbState` 里、被 3 个 repo 与 `McpStore` 多处持有，**不可能在运行期
//! 释放**。Windows **不允许给已打开的文件改名**（`ERROR_SHARING_VIOLATION`，os error 32），
//! 于是"把目标里的空库改名让位给真数据"这一步在运行期**必然失败**。
//!
//! 更糟的是失败提示本身不可执行：旧文案让用户"完全退出应用后重试"——可**退出之后
//! 就点不到迁移按钮了**，那是死循环。
//!
//! 因此唯一的正解是**两阶段**：
//!
//! ```text
//! 用户点迁移 → 运行期只做「复制源到暂存」+ 写「待接管」标记 → 提示重启
//! 下次启动   → 在 init_db 之前做改名交换（此时无人持句柄）→ 必然成功
//! ```
//!
//! # 标记文件放在哪（这里有个容易踩的坑）
//!
//! 写在**原生数据目录**（`app.path().app_data_dir()`，与 `setup.rs` 里
//! `perform_migration_v028` 同址），**不是** `AppDataDir`。
//!
//! 理由：`AppDataDir` 是**当前生效**的数据目录，而它**可能正是"待接管"的那一个**
//! （用户把数据目录指到了别处、或用了便携版时，目标目录本身就是待接管的目录）。
//! 把标记放进去会与接管动作互相踩：接管的第一步就是动那个目录里的库。
//! 原生数据目录由 identifier 推导、位置稳定，且**永远不是**被接管的那个。
//!
//! # 依赖约束
//!
//! 本模块**只依赖 `std`**，不引用 Tauri 类型。这样它能脱离整 crate 独立编译与测试
//! （本 crate 在 Linux 上因 Windows 专用代码缺 cfg 门控而无法整体编译），
//! 也让"接管顺序"这类关键性质可以被真实文件系统上的测试直接验证。
//! 序列化用手写的 JSON（只需读写自己的固定字段），不引入 serde 依赖。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 标记文件的格式标识。自描述，便于将来演进（读到不认识的 format 就拒绝处理，
/// 而不是按当前版本硬解析——那正是"格式演进后静默读错"的经典事故）。
pub const FORMAT: &str = "MIGRATION_PENDING_V1";

/// 标记文件名。
pub const FILE_NAME: &str = "migration-pending.json";

/// 一份"待接管"记录：一次已复制就绪、等着下次启动交换的迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMigration {
    /// **只读**源目录（接管后据此改写数据库里的绝对路径）。
    pub source_dir: PathBuf,
    /// 已就绪的暂存目录（接管动作的输入）。
    pub staging_dir: PathBuf,
    /// 目标数据目录（接管动作的输出）。
    pub target_dir: PathBuf,
    /// 写入时刻（Unix 秒）。
    pub created_at: u64,
    /// 写入时的应用版本，便于排查"哪一版留下的"。
    pub app_version: String,
}

impl PendingMigration {
    /// 组装一条记录（`created_at` 取当前时间）。
    pub fn new(
        source_dir: PathBuf,
        staging_dir: PathBuf,
        target_dir: PathBuf,
        app_version: impl Into<String>,
    ) -> Self {
        Self {
            source_dir,
            staging_dir,
            target_dir,
            created_at: now_unix_secs(),
            app_version: app_version.into(),
        }
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 标记文件路径：**原生数据目录**下的 `migration-pending.json`。
///
/// `native_data_dir` 必须是 `app.path().app_data_dir()` 的结果，**不是** `AppDataDir`
/// ——理由见本模块文档首部。
pub fn marker_path(native_data_dir: &Path) -> PathBuf {
    native_data_dir.join(FILE_NAME)
}

// ---------------------------------------------------------------------------
// 手写 JSON：只处理本模块自己的字段，不引入 serde
// ---------------------------------------------------------------------------

/// JSON 字符串转义（路径里出现引号、反斜杠、非 ASCII 都必须能原样往返）。
fn escape_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 从一个 JSON 对象文本里取出 `"key": "value"` 的字符串值。
///
/// 只做本模块需要的最小解析：找键、跳过冒号与空白、读带转义的字符串。
/// 解析失败返回 `None`（调用方按"标记不可用"处理，绝不猜测）。
fn json_string_field(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let mut pos = src.find(&needle)? + needle.len();
    let rest = &src[pos..];
    let colon = rest.find(':')?;
    pos += colon + 1;
    let rest = src[pos..].trim_start();
    let mut chars = rest.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => return None, // 本模块不写 \u 转义的可读字段，读到就当格式不认识
                other => out.push(other),
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

fn json_u64_field(src: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{}\"", key);
    let pos = src.find(&needle)? + needle.len();
    let rest = &src[pos..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

/// 渲染成标记文件的文本形式（`\n` 结尾，人可读）。
pub fn render(pending: &PendingMigration) -> String {
    format!(
        "{{\n  \"format\": \"{}\",\n  \"sourceDir\": \"{}\",\n  \"stagingDir\": \"{}\",\n  \"targetDir\": \"{}\",\n  \"createdAt\": {},\n  \"appVersion\": \"{}\"\n}}\n",
        FORMAT,
        escape_json(&pending.source_dir.to_string_lossy()),
        escape_json(&pending.staging_dir.to_string_lossy()),
        escape_json(&pending.target_dir.to_string_lossy()),
        pending.created_at,
        escape_json(&pending.app_version),
    )
}

/// 解析标记文件的文本形式。`format` 不认识时返回 `None`。
pub fn parse(raw: &str) -> Option<PendingMigration> {
    let format = json_string_field(raw, "format")?;
    if format != FORMAT {
        return None;
    }
    Some(PendingMigration {
        source_dir: PathBuf::from(json_string_field(raw, "sourceDir")?),
        staging_dir: PathBuf::from(json_string_field(raw, "stagingDir")?),
        target_dir: PathBuf::from(json_string_field(raw, "targetDir")?),
        created_at: json_u64_field(raw, "createdAt").unwrap_or(0),
        app_version: json_string_field(raw, "appVersion").unwrap_or_default(),
    })
}

/// 原子写入标记：先写 `.tmp`，复读校验能解析之后才改名为正式文件。
///
/// 【为什么必须原子】标记是"已复制就绪"的唯一凭据。若写了一半就断电，重启后读到的是
/// 一棵残缺的 JSON，而**暂存目录已经完整存在**——此时按"标记不可用"处理是安全的
/// （不接管、下次重新点一次迁移），按"标记存在"处理才是危险的。
/// `.tmp` 残留即代表"未提交"，这与记忆库里的同类约定一致。
pub fn write(native_data_dir: &Path, pending: &PendingMigration) -> io::Result<PathBuf> {
    fs::create_dir_all(native_data_dir)?;
    let final_path = marker_path(native_data_dir);
    let tmp_path = final_path.with_extension("json.tmp");

    let text = render(pending);
    fs::write(&tmp_path, text.as_bytes())?;

    // 复读校验：连"能解析出自己刚写的内容"都没确认过，就不能算写成功。
    let read_back = fs::read_to_string(&tmp_path)?;
    if parse(&read_back).as_ref() != Some(pending) {
        let _ = fs::remove_file(&tmp_path);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "待接管标记写入后复读校验失败",
        ));
    }

    fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// 读取标记。以下情况统一返回 `None`（按"没有待接管任务"处理，绝不猜测）：
///
/// - 文件不存在；
/// - 读不出来；
/// - 内容不是本模块认识的 `MIGRATION_PENDING_V1`。
pub fn read(native_data_dir: &Path) -> Option<PendingMigration> {
    let raw = fs::read_to_string(marker_path(native_data_dir)).ok()?;
    parse(&raw)
}

/// 删除标记（接管成功后调用）。文件本来就不存在时也算成功（幂等）。
pub fn clear(native_data_dir: &Path) -> io::Result<()> {
    let path = marker_path(native_data_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 把一条标记渲染成给用户看的一句话（日志与失败报告都用它）。
pub fn describe(pending: &PendingMigration) -> String {
    format!(
        "待接管：源={} 暂存={} 目标={}（v{} 于 {} 写入）",
        pending.source_dir.display(),
        pending.staging_dir.display(),
        pending.target_dir.display(),
        pending.app_version,
        pending.created_at
    )
}

// ---------------------------------------------------------------------------
// 启动期接管
// ---------------------------------------------------------------------------

/// 一次启动期接管的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeoverOutcome {
    /// 没有标记（绝大多数启动走这条）。
    NotPending,
    /// 接管成功：暂存已提升为正式目标，标记已清除。
    Promoted {
        /// 数据来源目录（调用方据此改写库内的绝对路径）。
        source_dir: PathBuf,
        target_dir: PathBuf,
    },
    /// 接管失败：**标记保留**，暂存保留，下次启动再试。
    Failed { reason: String },
}

/// 启动期接管的**纯逻辑实现**（不依赖 Tauri，全部输入来自参数）。
///
/// 调用点必须是"数据目录已解析、logger 已初始化、**尚无任何 `Connection`**"那一刻——
/// 即 `setup::init` 里 `resolve_data_dir` 之后、`database::init_db` 之前。
/// 顺序由 `setup_tests::takeover_runs_before_database_is_opened` 以源码顺序断言锁住。
///
/// ## 失败绝不阻断启动
///
/// 迁移是**附加**功能，失败最多是"这次没迁成、数据还在源目录里"，绝不该让用户
/// 连应用都开不了。因此这里把所有错误收成 [`TakeoverOutcome::Failed`]，只记日志、
/// **保留标记**（下次启动再试），由调用方继续走正常启动流程。
///
/// ## 为什么先查"暂存是否还在"再动手
///
/// 若标记在而暂存目录没了（用户手工清过、或磁盘写入失败），此时**不能**去动目标目录：
/// 那会把用户现有的库改名归档，却没有真数据补进来——比什么都不做糟得多。
/// 因此这种情况下只清掉标记（它已无意义），不做任何破坏性动作。
pub fn run_startup_takeover(
    native_data_dir: &Path,
    promote: &mut dyn FnMut(&Path, &Path) -> Result<(), String>,
) -> TakeoverOutcome {
    let Some(pending) = read(native_data_dir) else {
        return TakeoverOutcome::NotPending;
    };

    // 标记指向的暂存目录必须真实存在，否则这次接管没有任何输入。
    if !pending.staging_dir.is_dir() {
        let reason = format!(
            "标记指向的暂存目录已不存在（{}），本次不做任何改动；已清除这个无效标记。",
            pending.staging_dir.display()
        );
        // 清掉无意义标记：留着只会让每次启动都白跑一遍。
        let _ = clear(native_data_dir);
        return TakeoverOutcome::Failed { reason };
    }

    match promote(&pending.staging_dir, &pending.target_dir) {
        Ok(()) => {
            // 接管成功才删标记。顺序很重要：**先提升、后清标记**。
            // 反过来的话，提升失败而标记已删，下次启动就没有任何线索了。
            let _ = clear(native_data_dir);
            TakeoverOutcome::Promoted {
                source_dir: pending.source_dir,
                target_dir: pending.target_dir,
            }
        }
        Err(reason) => TakeoverOutcome::Failed {
            // 【保留标记与暂存】下次启动再试；源目录始终只读、从未被改动。
            reason,
        },
    }
}


// ---------------------------------------------------------------------------
// 测试：真实文件系统操作，只依赖 std。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-pending-test-{}-{}-{}",
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

    /// 一条记录必须能原样往返（含反斜杠与中文的 Windows 路径）。
    #[test]
    fn marker_round_trips_through_text_form() {
        let pending = PendingMigration {
            source_dir: PathBuf::from(r"D:\备份\旧数据"),
            staging_dir: PathBuf::from(r"C:\Users\u\AppData\Local\.com.tieznext.pending-takeover"),
            target_dir: PathBuf::from(r"C:\Users\u\AppData\Local\com.tieznext"),
            created_at: 1_790_000_000,
            app_version: "0.5.3".to_string(),
        };
        let text = render(&pending);
        assert!(
            text.contains(FORMAT),
            "标记必须自描述（含 format 字段）：{text}"
        );
        assert_eq!(parse(&text).as_ref(), Some(&pending), "标记必须能原样解析回来");
    }

    /// 格式不认识时必须拒绝处理，而不是按当前版本硬解析。
    #[test]
    fn unknown_format_is_refused_not_guessed() {
        let raw = r#"{"format":"MIGRATION_PENDING_V9","sourceDir":"a","stagingDir":"b","targetDir":"c"}"#;
        assert_eq!(parse(raw), None, "未知格式必须拒绝");
        assert_eq!(parse("not json at all"), None);
        assert_eq!(parse(""), None);
    }

    /// 写入是原子的：成功后有正式文件、没有 `.tmp` 残留；读回与写入一致。
    #[test]
    fn write_is_atomic_and_readable_back() {
        let root = tmp("write");
        let pending = PendingMigration::new(
            root.join("src"),
            root.join("staging"),
            root.join("target"),
            "0.5.3",
        );
        let path = write(&root, &pending).unwrap();
        assert!(path.is_file(), "正式标记文件必须存在");
        assert!(
            !root.join("migration-pending.json.tmp").exists(),
            "不得留下 .tmp 残留"
        );
        assert_eq!(read(&root).as_ref(), Some(&pending));
    }

    /// 没有标记时启动期接管必须什么都不做（绝大多数启动走这条）。
    #[test]
    fn startup_takeover_without_marker_is_a_noop() {
        let root = tmp("noop");
        let mut called = false;
        let outcome = run_startup_takeover(&root, &mut |_, _| {
            called = true;
            Ok(())
        });
        assert_eq!(outcome, TakeoverOutcome::NotPending);
        assert!(!called, "没有标记时不得触碰任何目录");
    }

    /// **失败保留标记**：接管失败时标记必须在，下次启动才可能重试。
    ///
    /// 这条是"失败不得阻断启动"这条契约的承重测试：如果失败时把标记删了，
    /// 用户永远等不到第二次机会，而界面上明明写着"重启后自动完成"。
    #[test]
    fn failed_takeover_keeps_the_marker_for_the_next_launch() {
        let root = tmp("fail-keeps-marker");
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("clipboard.db"), b"real data").unwrap();

        let pending = PendingMigration::new(
            root.join("src"),
            staging.clone(),
            root.join("target"),
            "0.5.3",
        );
        write(&root, &pending).unwrap();

        let outcome = run_startup_takeover(&root, &mut |_, _| Err("模拟改名被占用".to_string()));
        match outcome {
            TakeoverOutcome::Failed { reason } => {
                assert!(reason.contains("占用"), "必须如实回报失败原因：{reason}");
            }
            other => panic!("必须失败，实际 {other:?}"),
        }
        assert!(
            read(&root).is_some(),
            "失败后标记必须保留，否则用户永远等不到重试"
        );
        assert!(staging.is_dir(), "失败后暂存目录必须保留（它是重试的唯一输入）");
    }

    /// **暂存目录消失时不动目标**：标记还在但暂存没了，绝不能去归档用户现有的库。
    ///
    /// 【为什么这条是最高风险路径】此时若照常走"让位 + 提升"，结果是**用户现有的库被
    /// 改名归档、却没有真数据补进来**——比什么都不做糟得多：应用读到的仍是那个改名后
    /// 找不到的库，用户看到的是"数据没了"。正确处置是只清掉这个已无意义的标记。
    #[test]
    fn missing_staging_never_touches_the_target_database() {
        let root = tmp("missing-staging");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("clipboard.db"), b"user data").unwrap();
        let before = fs::read(target.join("clipboard.db")).unwrap();

        let pending = PendingMigration::new(
            root.join("src"),
            root.join("does-not-exist"),
            target.clone(),
            "0.5.3",
        );
        write(&root, &pending).unwrap();

        let mut promote_called = false;
        let outcome = run_startup_takeover(&root, &mut |_, _| {
            promote_called = true;
            Ok(())
        });

        assert!(!promote_called, "暂存不存在时绝不能去动目标");
        assert!(matches!(outcome, TakeoverOutcome::Failed { .. }));
        assert_eq!(
            fs::read(target.join("clipboard.db")).unwrap(),
            before,
            "目标库必须一字未改"
        );
        assert!(read(&root).is_none(), "无效标记应被清除，避免每次启动白跑");
    }

    /// 成功时：标记被清除，且**顺序正确**（先提升成功、后清标记）。
    #[test]
    fn successful_takeover_clears_the_marker_only_after_promotion() {
        let root = tmp("success-order");
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let pending = PendingMigration::new(
            root.join("src"),
            staging.clone(),
            root.join("target"),
            "0.5.3",
        );
        write(&root, &pending).unwrap();

        let mut saw_marker_during_promotion = false;
        let outcome = run_startup_takeover(&root, &mut |_, _| {
            // 提升动作**进行中**时，标记必须还没被删——否则提升失败就没有线索了。
            saw_marker_during_promotion = read(&root).is_some();
            Ok(())
        });

        assert!(
            saw_marker_during_promotion,
            "必须先提升、后清标记（顺序反了会让失败无法重试）"
        );
        assert!(matches!(outcome, TakeoverOutcome::Promoted { .. }));
        assert!(read(&root).is_none(), "接管成功后标记必须清除");
    }

    /// `clear` 幂等：没有标记时也算成功（接管路径会重复调用它）。
    #[test]
    fn clear_is_idempotent() {
        let root = tmp("clear");
        assert!(clear(&root).is_ok());
        assert!(clear(&root).is_ok());
    }
}
