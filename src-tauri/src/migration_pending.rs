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

/// 标记文件的格式标识（当前版本）。自描述，便于将来演进（读到不认识的 format 就拒绝
/// 处理，而不是按当前版本硬解析——那正是"格式演进后静默读错"的经典事故）。
///
/// # V1 → V2 到底加了什么（以及为什么必须加）
///
/// V1 只有一种"待办"：**接管另一份数据目录**。V2 加了第二种：**把本地暂存提升为正式
/// 数据**（备份恢复走的那条）。两者的运行期动作、暂存目录、提升函数都不同，因此标记
/// 必须能说清"这条是哪种活"，否则下次启动只能靠猜——猜错的后果是**把用户的暂存目录
/// 当成迁移源目录**，或者反过来，两条路都不会得到想要的数据。
///
/// 另外 V2 用 `stagingDone` 把"暂存骨架是否已组装完成"显式记进标记。理由见 [`PendingMigration::staging_done`]：
/// 一个组装到一半的暂存目录，比"还没有暂存目录"危险得多。
pub const FORMAT: &str = "MIGRATION_PENDING_V2";

/// 仍然认识的**历史格式**：V1 文件必须继续读得进来。
///
/// 【为什么必须兼容】V1 标记是**线上用户机器上真实存在**的文件：他们跑 0.5.3 点了迁移、
/// 看到"重启后自动完成"、然后没有重启就去装了新版本。若新版本把 V1 判为"格式不认识"，
/// 那次迁移就永远不会有第二次机会，而用户界面上明明写着"重启即完成"。
///
/// V1 记录缺的两个字段都有**安全的默认值**：`kind=takeover`（V1 只可能是迁移）、
/// `stagingDone=true`（V1 的写入方只在暂存目录**复制并逐项校验完成之后**才写标记，
/// 所以"暂存已就绪"对 V1 恒成立；默认成 false 反而会把用户那次迁移判成"未提交"而丢掉）。
pub const FORMAT_V1: &str = "MIGRATION_PENDING_V1";

/// 标记文件名。
pub const FILE_NAME: &str = "migration-pending.json";

/// 一条待办**是哪一种**：决定下次启动用哪个提升函数、要清哪些目录。
///
/// 【为什么用枚举而不是两个标记文件】两条路共享的东西远多于不同的东西：原生目录里的
/// 文件位置、原子写入与复读校验、`run_startup_takeover` 的失败语义（失败保留标记、绝不
/// 阻断启动）、"暂存没了就不许动目标"的保护、`setup.rs` 里那个必须早于 `init_db` 的
/// 调用点。分成两个模块等于把上面每一条都复制一份，而它们每一条都是踩过坑才写对的。
/// 真正不同的只有两处：**怎么判断暂存就绪**、**调用哪个提升函数**——那就只让这两处不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// 接管另一份数据目录（迁移中心那条链）。
    ///
    /// 暂存 = 源目录的整份副本；提升 = 让位目标的空库 + 逐文件搬入；成功后需按
    /// "源 → 目标"改写库内绝对路径。
    Takeover,
    /// 把暂存目录里组装好的数据提升为**当前**数据目录（备份恢复那条链）。
    ///
    /// 与 `Takeover` 的关键差别：暂存只是**受管条目的片段**（`clipboard.db`、`attachments/`…），
    /// 不是一整份数据目录；因此提升走"逐条目让位 + 逐条目放置"，绝不做整目录合并——
    /// 那会把无关文件（日志、`datapath.txt`、用户自己放进去的东西）也带进正式数据。
    LocalRestore,
}

impl PendingKind {
    /// 写进标记文件的取值（稳定的机器可读串，改动即破坏格式兼容）。
    pub fn as_str(self) -> &'static str {
        match self {
            PendingKind::Takeover => "takeover",
            PendingKind::LocalRestore => "local_restore",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "takeover" => Some(PendingKind::Takeover),
            "local_restore" => Some(PendingKind::LocalRestore),
            _ => None,
        }
    }
}

/// 一份"待接管"记录：一次已复制就绪、等着下次启动交换的活。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMigration {
    /// 这条待办属于哪一种（决定提升函数与暂存残留的处置方式）。
    pub kind: PendingKind,
    /// **只读**源目录。
    ///
    /// - `PendingKind::Takeover`：被接管的旧数据目录，接管后据此改写库内绝对路径。
    /// - `PendingKind::LocalRestore`：**等于 `target_dir`**。恢复没有"另一个源目录"，
    ///   用户的依据是那个备份包（它由导出链自己保证只读，恢复链从不写它）。
    ///   填 `target_dir` 而不是留空，是为了让现有调用方（它无条件用这个字段做路径改写）
    ///   不需要理解两种语义：对恢复来说"旧路径前缀 = 当前数据目录"，正是正确的前缀。
    pub source_dir: PathBuf,
    /// 已就绪的暂存目录（提升动作的输入）。
    pub staging_dir: PathBuf,
    /// 目标数据目录（提升动作的输出）。
    pub target_dir: PathBuf,
    /// **暂存骨架是否已组装完成**。
    ///
    /// 【这个字段解决的是一个真实的数据风险，不是记账】备份恢复在写标记**之前**要把
    /// 暂存组装完（下载/复制/校验/路径改写/云同步重置，几百毫秒到几十秒）。于是存在
    /// 一个窗口：用户点了恢复 → 组装到一半 → 进程被杀/断电 → **进程既没写标记，也没留下
    /// aside 的替换现场**，只在数据目录同级留下半个暂存目录。
    ///
    /// 此时若下次启动的清理只认"标记是否存在"，这半个暂存目录就会被判成"无主残渣"删掉
    /// ——而这本身没错（它确实没被提交）。真正危险的是反过来：若清理逻辑将来被改成
    /// "暂存目录存在就提升"，一个含**半份受管条目**的片段会被提升为正式数据，用户会看到
    /// "剪贴板记录还在、附件全丢了"。
    ///
    /// 因此本字段把"已提交"这件事写进标记本体：**只有 `stagingDone=true` 的标记才允许
    /// 提升**；`false` 一律按"未提交"处理（清掉暂存与标记，让用户重新点一次）。
    /// 写入顺序固定为"先组装、后写标记（`stagingDone=true`）"，所以正常情况下标记一旦
    /// 存在，暂存就是完整的。
    pub staging_done: bool,
    /// 写入时刻（Unix 秒）。
    pub created_at: u64,
    /// 写入时的应用版本，便于排查"哪一版留下的"。
    pub app_version: String,
}

impl PendingMigration {
    /// 组装一条**迁移接管**记录（`created_at` 取当前时间）。
    pub fn new(
        source_dir: PathBuf,
        staging_dir: PathBuf,
        target_dir: PathBuf,
        app_version: impl Into<String>,
    ) -> Self {
        Self::for_kind(
            PendingKind::Takeover,
            source_dir,
            staging_dir,
            target_dir,
            true,
            app_version,
        )
    }

    /// 组装一条**已组装就绪**的记录（暂存已完整，允许提升）。
    pub fn for_kind(
        kind: PendingKind,
        source_dir: PathBuf,
        staging_dir: PathBuf,
        target_dir: PathBuf,
        staging_done: bool,
        app_version: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            source_dir,
            staging_dir,
            target_dir,
            staging_done,
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

/// 读一个 JSON 布尔字段。**读不到就返回 `None`**（由调用方决定默认值），
/// 而不是静默当成 `false`——这两者在 `stagingDone` 上含义完全不同。
fn json_bool_field(src: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{}\"", key);
    let pos = src.find(&needle)? + needle.len();
    let rest = &src[pos..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

/// 渲染成标记文件的文本形式（`\n` 结尾，人可读）。
pub fn render(pending: &PendingMigration) -> String {
    format!(
        "{{\n  \"format\": \"{}\",\n  \"kind\": \"{}\",\n  \"sourceDir\": \"{}\",\n  \"stagingDir\": \"{}\",\n  \"targetDir\": \"{}\",\n  \"stagingDone\": {},\n  \"createdAt\": {},\n  \"appVersion\": \"{}\"\n}}\n",
        FORMAT,
        pending.kind.as_str(),
        escape_json(&pending.source_dir.to_string_lossy()),
        escape_json(&pending.staging_dir.to_string_lossy()),
        escape_json(&pending.target_dir.to_string_lossy()),
        pending.staging_done,
        pending.created_at,
        escape_json(&pending.app_version),
    )
}

/// 解析标记文件的文本形式。`format` 不认识时返回 `None`。
///
/// 同时接受 [`FORMAT_V1`]（见其文档：线上用户机器上真实存在这类文件，丢掉它等于
/// 让那些用户的迁移永远失去第二次机会）。
pub fn parse(raw: &str) -> Option<PendingMigration> {
    let format = json_string_field(raw, "format")?;
    let is_v2 = format == FORMAT;
    if !is_v2 && format != FORMAT_V1 {
        return None;
    }
    Some(PendingMigration {
        // V1 没有 kind 字段，且 V1 只可能是迁移接管。
        kind: json_string_field(raw, "kind")
            .and_then(|k| PendingKind::from_str(&k))
            // V2 里 kind 不认识 -> 整条标记不可用（返回 None 比猜一个默认值安全：
            // 猜错会让下次启动用错的提升函数去动用户的数据）。
            .or(if is_v2 {
                None
            } else {
                Some(PendingKind::Takeover)
            })?,
        source_dir: PathBuf::from(json_string_field(raw, "sourceDir")?),
        staging_dir: PathBuf::from(json_string_field(raw, "stagingDir")?),
        target_dir: PathBuf::from(json_string_field(raw, "targetDir")?),
        // V1 记录没有这个字段：它的写入方只在暂存复制并校验完成后写标记，故默认 true。
        staging_done: json_bool_field(raw, "stagingDone").unwrap_or(!is_v2),
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

    // 提交失败时**不留 .tmp 残渣**：这个文件对下一次启动毫无意义，而留下它只会让
    // "数据目录同级有一堆没人认识的临时文件"变成新的排障负担。
    if let Err(e) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
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
        "待处理（{}）：源={} 暂存={} 目标={}（v{} 于 {} 写入）",
        pending.kind.as_str(),
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
        /// 这条待办的种类（调用方据此决定要不要按"旧目录 -> 新目录"改写库内路径）。
        kind: PendingKind,
        /// 数据来源目录（`Takeover` 时据此改写库内的绝对路径）。
        source_dir: PathBuf,
        /// 提升后的目标数据目录。
        target_dir: PathBuf,
        /// 提交这一条时**被顶掉的旧暂存**（前一次未重启的提交留下的）有多少字节。
        ///
        /// 见 [`run_startup_takeover`] 关于"二次提交"的说明：它不为零就说明用户提交过
        /// 一次恢复却没有重启，而这一次顶掉了上一次。这个数字要进日志——它是事后唯一
        /// 能解释"我上次那个包为什么没生效"的证据。
        superseded_staging_bytes: u64,
    },
    /// 接管失败：**标记保留**，暂存保留，下次启动再试。
    Failed { reason: String },
}

/// 量一份目录里已有多少字节（读不到的条目按 0 算——它只用于日志与量级提示）。
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(ty) = entry.file_type() else { continue };
        if ty.is_dir() {
            total += dir_bytes(&entry.path());
        } else if ty.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// 尝试把一条标记提交掉（三段出路各自的返回点都只有一处，便于后来者阅读）。
///
/// - 判定为**无主**（从未提交、或暂存已消失）→ 清掉标记与无意义的暂存，返回 `Failed`
///   并给出人话原因（标记已不存在，所以这条不会重试）。
/// - 提交**成功** → 清标记，返回 `Promoted`。
/// - 提交**失败** → 什么都不清（标记与暂存都留着），返回 `Failed`（下次启动重试）。
fn commit_pending(
    native_data_dir: &Path,
    pending: &PendingMigration,
    promote: &mut dyn FnMut(&PendingMigration) -> Result<(), String>,
) -> TakeoverOutcome {
    // ⓪ 标记里的路径必须是**绝对路径**才允许被使用。
    //
    // 【为什么值得单独一道闸】标记文件是磁盘上一份**可被外部编辑**的普通 JSON。相对路径
    // （尤其空串）在接管里会被解释成"相对于进程当前工作目录"——那既不是用户的意图，也
    // 可能恰好是应用安装目录。于是"删除暂存""改名让位"会落在谁也没想到的地方。
    // 我们自己的写入方永远写绝对路径，因此这条闸在正常路径上永远不会触发，只在标记被
    // 手工改动/损坏时才拦下来，判据也简单到不可能是错的。
    if !pending.staging_dir.is_absolute() || !pending.target_dir.is_absolute() {
        let _ = clear(native_data_dir);
        return TakeoverOutcome::Failed {
            reason: format!(
                "待处理标记里的路径不是绝对路径（暂存={}，目标={}），已拒绝执行并清除这个无效标记；\
                 你的数据未被改动。请重新执行一次操作。",
                pending.staging_dir.display(),
                pending.target_dir.display()
            ),
        };
    }

    // ① 标记写着"暂存还没组装完" => 这条待办**从未被提交**。
    //
    // 处置是丢弃，不是重试：半成品没有安全的补完方式（我们不知道它缺哪几条），而它就
    // 从未被提升进正式数据，删掉不丢任何用户数据。保留它则更糟——每次启动都拿一个
    // 不完整的片段去提升，得到的正是"记录在、附件没了"。
    if !pending.staging_done {
        let _ = fs::remove_dir_all(&pending.staging_dir);
        let _ = clear(native_data_dir);
        return TakeoverOutcome::Failed {
            reason: format!(
                "检测到一次从未完成的备份恢复（暂存目录 {} 尚未组装完就已中止），已清理这段未提交的暂存片段；\
                 你的正式数据一个字节都没被动过，请重新执行一次恢复。",
                pending.staging_dir.display()
            ),
        };
    }

    // ② 标记指向的暂存目录必须真实存在，否则这次接管没有任何输入。
    if !pending.staging_dir.is_dir() {
        let reason = format!(
            "标记指向的暂存目录已不存在（{}），本次不做任何改动；已清除这个无效标记。",
            pending.staging_dir.display()
        );
        // 清掉无意义标记：留着只会让每次启动都白跑一遍。
        let _ = clear(native_data_dir);
        return TakeoverOutcome::Failed { reason };
    }

    // ③ 提交之前先量出"将被顶掉的旧暂存"有多大（见 `TakeoverOutcome::Promoted`）。
    let superseded_staging_bytes = dir_bytes(&pending.staging_dir);

    match promote(pending) {
        Ok(()) => {
            // 接管成功才删标记。顺序很重要：**先提升、后清标记**。
            // 反过来的话，提升失败而标记已删，下次启动就没有任何线索了。
            let _ = clear(native_data_dir);
            TakeoverOutcome::Promoted {
                kind: pending.kind,
                source_dir: pending.source_dir.clone(),
                target_dir: pending.target_dir.clone(),
                superseded_staging_bytes,
            }
        }
        Err(reason) => TakeoverOutcome::Failed {
            // 【保留标记与暂存】下次启动再试；源目录始终只读、从未被改动。
            reason: format!(
                "{}（暂存目录与标记均已保留，下次启动会自动重试：{}）",
                reason,
                describe(pending)
            ),
        },
    }
}

/// 启动期接管的**纯逻辑实现**（不依赖 Tauri，全部输入来自参数）。
///
/// 调用点必须是"数据目录已解析、logger 已初始化、**尚无任何 `Connection`**"那一刻——
/// 即 `setup::init` 里 `resolve_data_dir` 之后、`database::init_db` 之前。
/// 顺序由 `setup_tests` 的源码顺序断言锁住（迁移与恢复各有一条）。
///
/// ## 一个调用点干两件不同的事
///
/// `promote` 回调收到的是**整条标记**，它按 `pending.kind` 分派到对应的提升函数：
/// 迁移走"让位空库 + 整目录搬入"，恢复走"逐受管条目让位 + 逐受管条目放置"。
/// 两种活共用同一个标记文件、同一段失败语义、同一个"必须早于 `init_db`"的时机、
/// 同一个"暂存没了就不许动目标"的保护。真正不同的只有"怎么判断暂存就绪"和
/// "调用哪个提升函数"——那就只让这两处不同。
///
/// ## 失败绝不阻断启动
///
/// 迁移与恢复都是**附加**功能：失败最多是"这次没成，数据还是原来那份"，绝不该让用户
/// 连应用都开不了。因此这里把所有错误收成 [`TakeoverOutcome::Failed`]，只记日志、
/// **保留标记**（下次启动再试），由调用方继续走正常启动流程。
///
/// ## 为什么先查"暂存是否还在"再动手
///
/// 若标记在而暂存目录没了（用户手工清过、或磁盘写入失败），此时**不能**去动目标目录：
/// 那会把用户现有的库改名归档，却没有真数据补进来——比什么都不做糟得多。
/// 因此这种情况下只清掉标记（它已无意义），不做任何破坏性动作。
///
/// ## 二次提交：后来者取代前一次，且只留一份
///
/// 用户提交了一次恢复却没重启，然后又提交了第二个包。此时**不拒绝**：拒绝意味着用户
/// 必须先重启才能换一个包，而重启正是当下不方便做的事；而"两次提交合起来生效"更糟
/// ——A 的 `clipboard.db` 配 B 的 `attachments/` 会得到一份自相矛盾的数据。
/// 因此确定性规则是：**最后一次提交取代前一次**。前一次的暂存由提升函数在开头清场
/// （见 `backup::import::promote_staged_restore`），本函数则把"顶掉了多少字节"记进
/// 结果，让调用方写进日志——这条事实必须留下痕迹，否则用户无法事后理解发生过什么。
pub fn run_startup_takeover(
    native_data_dir: &Path,
    promote: &mut dyn FnMut(&PendingMigration) -> Result<(), String>,
) -> TakeoverOutcome {
    let Some(pending) = read(native_data_dir) else {
        return TakeoverOutcome::NotPending;
    };
    commit_pending(native_data_dir, &pending, promote)
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
        for kind in [PendingKind::Takeover, PendingKind::LocalRestore] {
            for staging_done in [true, false] {
                let pending = PendingMigration {
                    kind,
                    source_dir: PathBuf::from(r"D:\备份\旧数据"),
                    staging_dir: PathBuf::from(
                        r"C:\Users\u\AppData\Local\.com.tieznext.pending-takeover",
                    ),
                    target_dir: PathBuf::from(r"C:\Users\u\AppData\Local\com.tieznext"),
                    staging_done,
                    created_at: 1_790_000_000,
                    app_version: "0.5.3".to_string(),
                };
                let text = render(&pending);
                assert!(
                    text.contains(FORMAT),
                    "标记必须自描述（含 format 字段）：{text}"
                );
                assert!(
                    text.contains(kind.as_str()),
                    "标记必须写明这是哪一种待办（{kind:?}）：{text}"
                );
                assert_eq!(
                    parse(&text).as_ref(),
                    Some(&pending),
                    "标记必须能原样解析回来（kind={kind:?} staging_done={staging_done}）"
                );
            }
        }
    }

    /// 格式不认识时必须拒绝处理，而不是按当前版本硬解析。
    #[test]
    fn unknown_format_is_refused_not_guessed() {
        let raw = r#"{"format":"MIGRATION_PENDING_V9","sourceDir":"a","stagingDir":"b","targetDir":"c"}"#;
        assert_eq!(parse(raw), None, "未知格式必须拒绝");
        assert_eq!(parse("not json at all"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("[]"), None, "空 JSON 数组也必须拒绝");
        assert_eq!(
            parse(r#"{"format":"MIGRATION_PENDING_V2"}"#),
            None,
            "V2 缺字段必须拒绝（不能猜一个默认目录）"
        );
    }

    /// **V1 标记必须继续读得进来**，且按"迁移接管 / 暂存已就绪"解释。
    ///
    /// 【为什么这条不能省】V1 标记是线上用户机器上真实存在的文件：他们跑 0.5.3 点了迁移、
    /// 界面上看到"重启后自动完成"、然后没重启就装了新版本。若新版本把它判成"格式不认识"，
    /// 那次迁移就永远没有第二次机会——而界面上的承诺还在。
    #[test]
    fn v1_marker_written_by_an_older_version_is_still_honoured() {
        let raw = r#"{
  "format": "MIGRATION_PENDING_V1",
  "sourceDir": "D:\\old",
  "stagingDir": "C:\\new.pending-takeover",
  "targetDir": "C:\\new",
  "createdAt": 1790000000,
  "appVersion": "0.5.3"
}
"#;
        let parsed = parse(raw).expect("V1 标记必须被接受（旧版本用户靠它完成迁移）");
        assert_eq!(
            parsed.kind,
            PendingKind::Takeover,
            "V1 只可能是迁移接管"
        );
        assert!(
            parsed.staging_done,
            "V1 的写入方只在暂存复制并校验完成后才写标记，因此必须按'已就绪'解释；\
             判成未提交会把用户那次迁移直接丢掉"
        );
        assert_eq!(parsed.source_dir, PathBuf::from(r"D:\old"));
    }

    /// V2 里 `kind` 取值不认识时必须整条拒绝，不能猜一个默认值。
    #[test]
    fn unknown_kind_is_refused_because_guessing_would_pick_the_wrong_promotion() {
        let raw = r#"{"format":"MIGRATION_PENDING_V2","kind":"something_new","sourceDir":"a","stagingDir":"b","targetDir":"c","stagingDone":true}"#;
        assert_eq!(
            parse(raw),
            None,
            "kind 不认识就不能处理：猜错会用错的提升函数去动用户数据"
        );
    }

    /// **标记文件必须能被下一次启动找到**：写入成功后，用"启动期读取"的同一个入口
    /// （`read`）必须能把它读回来。
    ///
    /// 【这条守的是什么】`write` 与 `read` 都走 `marker_path`，因此只要两者对称就是自洽的。
    /// 但真机上还有一个更强的要求：**下一次启动的代码路径**必须能定位到同一个位置。若将来
    /// 有人改了文件名、或把标记挪到"当前数据目录"（那是会被这次恢复换掉的目录），
    /// `write`/`read` 这对仍然自洽，可**跨进程**的交接却断了——表现为"重启后什么都没发生"。
    ///
    /// 因此这里额外断言两件事：文件名是约定的那一个（改动即破坏跨版本的兼容），以及
    /// 标记**住在 native 目录下**（不是当前数据目录）。前者由文件名常量守住，后者由所有
    /// 调用方传 `app.path().app_data_dir()` 守住（见 `app/setup.rs` 与三处命令/工具入口）。
    #[test]
    fn the_marker_stays_discoverable_across_processes() {
        let native = tmp("discoverable-native");
        let data_dir = native.join("com.tieznext");
        fs::create_dir_all(&data_dir).unwrap();

        let pending = PendingMigration::for_kind(
            PendingKind::LocalRestore,
            data_dir.clone(),
            native.join("staging"),
            data_dir.clone(),
            true,
            "0.5.6",
        );
        let written = write(&native, &pending).unwrap();

        // ① 文件名是约定的那一个（改名会让旧版本留下的、已承诺"重启即完成"的标记失联）。
        assert_eq!(
            written.file_name().unwrap().to_string_lossy(),
            FILE_NAME,
            "标记文件名是对外契约（旧版本留下的标记必须仍被找到）"
        );
        // ② 它住在 native 目录下，**不在**被恢复替换的那个数据目录里。
        assert!(
            written.starts_with(&native) && !written.starts_with(&data_dir),
            "标记必须住在原生数据目录，绝不能住在会被这次恢复换掉的数据目录里：{}",
            written.display()
        );
        // ③ "下一次启动"用的正是这个入口，它必须读得到。
        assert_eq!(read(&native), Some(pending));
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

    /// **损坏的标记绝不能被"凑合解析"出来驱动一次提升**。
    ///
    /// # 这条守的是一个真实的窗口
    ///
    /// 标记是"暂存已组装就绪"的唯一凭据。若它写了半截就断电（或被人手工改坏），
    /// 宽容的解析器会把它补成一个字段缺失的记录，而缺失的目录一旦被默认成空路径或相对
    /// 路径，接管就会去动**错误的目录**。正确行为是：读不懂就当成"没有待办"，什么都不做。
    ///
    /// 【反向对照实测】把 `parse` 里任一字段从 `?` 改成"缺了就取默认值"，本条变红
    /// （提升闭包会被调用，而它被断言绝不允许被调用）。
    #[test]
    fn a_corrupted_marker_is_never_used_to_drive_a_promotion() {
        let root = tmp("corrupted-marker");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("clipboard.db"), b"user data").unwrap();
        let before = fs::read(target.join("clipboard.db")).unwrap();

        // 三种"坏法"各来一遍：截断、字段缺失、根本不是 JSON。
        let mut full = render(&PendingMigration::for_kind(
            PendingKind::LocalRestore,
            target.clone(),
            root.join("staging"),
            target.clone(),
            true,
            "0.5.6",
        ));
        let truncated = full[..full.len() / 2].to_string();
        full = full.replace("\"targetDir\"", "\"targetDirRenamed\"");
        for (label, raw) in [
            ("截断的 JSON", truncated),
            ("字段被改名", full.clone()),
            ("完全不是 JSON", "GARBAGE".to_string()),
        ] {
            fs::write(marker_path(&root), raw.as_bytes()).unwrap();
            let mut promote_called = false;
            let outcome = run_startup_takeover(&root, &mut |_| {
                promote_called = true;
                Ok(())
            });
            assert!(
                !promote_called,
                "[{label}] 损坏的标记绝不允许驱动一次提升（缺失字段被默认值补出来的路径可能是错的）"
            );
            assert_eq!(
                outcome,
                TakeoverOutcome::NotPending,
                "[{label}] 读不懂就按'没有待办'处理：{outcome:?}"
            );
            assert_eq!(
                fs::read(target.join("clipboard.db")).unwrap(),
                before,
                "[{label}] 目标数据必须一字未改"
            );
        }
    }

    /// 标记里的**非绝对路径**绝不允许被使用（标记是磁盘上可被外部编辑的普通 JSON）。
    ///
    /// 【为什么这条必须存在】相对路径（尤其空串）会被解释成"相对于进程当前工作目录"，
    /// 于是"删除暂存""改名让位"会落在谁也没想到的地方——比如安装目录。我们自己的写入方
    /// 永远写绝对路径，因此这道闸只在标记被手工改动/损坏时触发。
    #[test]
    fn a_marker_with_a_relative_path_is_refused_and_never_acted_on() {
        let root = tmp("relative-path");
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("clipboard.db"), b"x").unwrap();

        let raw = r#"{"format":"MIGRATION_PENDING_V2","kind":"local_restore","sourceDir":"com.tieznext","stagingDir":"staging","targetDir":"com.tieznext","stagingDone":true,"createdAt":1,"appVersion":"0.5.6"}"#;
        fs::write(marker_path(&root), raw.as_bytes()).unwrap();

        let mut promote_called = false;
        let outcome = run_startup_takeover(&root, &mut |_| {
            promote_called = true;
            Ok(())
        });
        assert!(!promote_called, "相对路径绝不允许驱动任何文件操作");
        match outcome {
            TakeoverOutcome::Failed { reason } => {
                assert!(reason.contains("绝对路径"), "原因必须说清：{reason}");
            }
            other => panic!("必须拒绝，实际 {other:?}"),
        }
        assert!(staging.is_dir(), "拦截时不得删除任何东西");
        assert!(read(&root).is_none(), "无效标记应被清除，避免每次启动白跑");
    }

    /// 标记**写不进去**时必须如实失败，且不留半截文件（半截文件等于一个假凭据）。
    #[test]
    fn a_write_that_cannot_be_committed_leaves_no_half_written_marker() {
        let native = tmp("write-fails");
        // 在标记的正式路径上占一个**目录**：改名到它上面必然失败。
        let blocker = marker_path(&native);
        fs::create_dir_all(&blocker).unwrap();
        fs::write(blocker.join("blocker"), b"x").unwrap();

        let pending = PendingMigration::for_kind(
            PendingKind::LocalRestore,
            native.clone(),
            native.join("staging"),
            native.clone(),
            true,
            "0.5.6",
        );
        assert!(
            write(&native, &pending).is_err(),
            "提交不了就必须报错，不能静默当作写成功"
        );
        assert!(
            !native.join("migration-pending.json.tmp").exists(),
            "提交失败后不得留下 .tmp（一个半截文件对下次启动毫无意义）"
        );
        assert_eq!(
            read(&native),
            None,
            "提交失败后读到的必须是'没有待办'，而不是一份假凭据"
        );
    }

    /// 没有标记时启动期接管必须什么都不做（绝大多数启动走这条）。
    #[test]
    fn startup_takeover_without_marker_is_a_noop() {
        let root = tmp("noop");
        let mut called = false;
        let outcome = run_startup_takeover(&root, &mut |_| {
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

        let outcome = run_startup_takeover(&root, &mut |_| Err("模拟改名被占用".to_string()));
        match outcome {
            TakeoverOutcome::Failed { reason } => {
                assert!(reason.contains("占用"), "必须如实回报失败原因：{reason}");
                assert!(
                    reason.contains("下次启动"),
                    "必须告诉用户这条会重试：{reason}"
                );
            }
            other => panic!("必须失败，实际 {other:?}"),
        }
        assert!(
            read(&root).is_some(),
            "失败后标记必须保留，否则用户永远等不到重试"
        );
        assert!(
            staging.is_dir(),
            "失败后暂存目录必须保留（它是重试的唯一输入）"
        );
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
        let outcome = run_startup_takeover(&root, &mut |_| {
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

    /// **未提交的暂存片段绝不允许被提升**（`stagingDone=false`）。
    ///
    /// 这条守的是"半份数据被当成正式数据"这个真实风险：备份恢复在组装到一半时被强杀，
    /// 数据目录同级会留下半个暂存目录。若启动期按"暂存存在就提升"处理，用户会拿到
    /// 一份自相矛盾的数据（记录在、附件丢了）。正确处置是清理它并如实告知。
    #[test]
    fn staging_that_was_never_committed_is_discarded_not_promoted() {
        let root = tmp("uncommitted-staging");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("clipboard.db"), b"user data").unwrap();
        let staging = root.join("half-built-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("clipboard.db"), b"half a fragment").unwrap();

        let pending = PendingMigration::for_kind(
            PendingKind::LocalRestore,
            target.clone(),
            staging.clone(),
            target.clone(),
            false, // 组装未完成
            "0.5.6",
        );
        write(&root, &pending).unwrap();

        let mut promote_called = false;
        let outcome = run_startup_takeover(&root, &mut |_| {
            promote_called = true;
            Ok(())
        });

        assert!(!promote_called, "未提交的片段绝不能被提升");
        match outcome {
            TakeoverOutcome::Failed { reason } => {
                assert!(reason.contains("从未完成"), "原因必须说清是未提交：{reason}");
            }
            other => panic!("必须失败并如实说明，实际 {other:?}"),
        }
        assert_eq!(
            fs::read(target.join("clipboard.db")).unwrap(),
            b"user data",
            "正式数据必须一字未改"
        );
        assert!(!staging.exists(), "无主的未提交暂存应被清理");
        assert!(read(&root).is_none(), "标记应被清除，避免每次启动白跑");
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
        let outcome = run_startup_takeover(&root, &mut |_| {
            // 提升动作**进行中**时，标记必须还没被删——否则提升失败就没有线索了。
            saw_marker_during_promotion = read(&root).is_some();
            Ok(())
        });

        assert!(
            saw_marker_during_promotion,
            "必须先提升、后清标记（顺序反了会让失败无法重试）"
        );
        assert!(matches!(
            outcome,
            TakeoverOutcome::Promoted {
                kind: PendingKind::Takeover,
                ..
            }
        ));
        assert!(read(&root).is_none(), "接管成功后标记必须清除");
    }

    /// 提升函数收到的是**整条标记**（含 kind），调用方才能分派到正确的提升动作。
    #[test]
    fn promotion_receives_the_whole_marker_so_caller_can_dispatch_on_kind() {
        let root = tmp("receives-marker");
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let target = root.join("target");
        let pending = PendingMigration::for_kind(
            PendingKind::LocalRestore,
            target.clone(),
            staging.clone(),
            target.clone(),
            true,
            "0.5.6",
        );
        write(&root, &pending).unwrap();

        let mut seen: Option<PendingKind> = None;
        let mut seen_target: Option<PathBuf> = None;
        let _ = run_startup_takeover(&root, &mut |p| {
            seen = Some(p.kind);
            seen_target = Some(p.target_dir.clone());
            Ok(())
        });

        assert_eq!(seen, Some(PendingKind::LocalRestore));
        assert_eq!(seen_target, Some(target));
    }

    /// **二次提交：后来者取代前一次**，且"顶掉了多少"必须有证据。
    ///
    /// 【为什么这条必须是明确的规则而不是"看情况"】用户提交了一次恢复却没重启，又提交了
    /// 第二个包。此时两种错误做法都很诱人：拒绝（用户必须先重启才能换包，而重启正是他
    /// 现在不想做的事）、或者两次合起来生效（A 的数据库配 B 的附件 = 自相矛盾的数据）。
    /// 确定性规则是"最后一次取代前一次"，且被顶掉的量要记下来，用户事后能问"为什么
    /// 我上一个包没生效"。
    #[test]
    fn a_second_submission_supersedes_the_first_and_the_size_is_reported() {
        let root = tmp("supersede");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();

        // 第一次提交：暂存里 200 字节
        let first_staging = root.join("staging-a");
        fs::create_dir_all(&first_staging).unwrap();
        fs::write(first_staging.join("clipboard.db"), vec![b'A'; 200]).unwrap();
        write(
            &root,
            &PendingMigration::for_kind(
                PendingKind::LocalRestore,
                target.clone(),
                first_staging.clone(),
                target.clone(),
                true,
                "0.5.6",
            ),
        )
        .unwrap();

        let outcome = run_startup_takeover(&root, &mut |p| {
            assert_eq!(
                p.staging_dir, first_staging,
                "提升的必须是标记指向的那一个暂存目录"
            );
            Ok(())
        });
        match outcome {
            TakeoverOutcome::Promoted {
                superseded_staging_bytes,
                kind,
                ..
            } => {
                assert_eq!(kind, PendingKind::LocalRestore);
                assert_eq!(
                    superseded_staging_bytes, 200,
                    "必须量出被顶掉的那一份有多大（它是事后唯一的解释依据）"
                );
            }
            other => panic!("必须成功，实际 {other:?}"),
        }
        assert!(read(&root).is_none(), "提交成功后标记必须清除");
    }

    /// 二次提交的**写侧**：后写的标记必须整体覆盖先写的，不能出现两条线索。
    #[test]
    fn rewriting_the_marker_replaces_it_wholesale() {
        let root = tmp("rewrite");
        let target = root.join("target");
        let a = PendingMigration::for_kind(
            PendingKind::LocalRestore,
            target.clone(),
            root.join("staging-a"),
            target.clone(),
            true,
            "0.5.5",
        );
        write(&root, &a).unwrap();
        let b = PendingMigration::for_kind(
            PendingKind::LocalRestore,
            target.clone(),
            root.join("staging-b"),
            target.clone(),
            true,
            "0.5.6",
        );
        write(&root, &b).unwrap();
        assert_eq!(read(&root), Some(b), "后写的标记必须整体取代先写的");
        // 只有一个标记文件，没有 .tmp 与副本
        let markers: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("migration-pending"))
            .collect();
        assert_eq!(markers, vec![FILE_NAME.to_string()], "只允许存在一个标记文件");
    }

    /// `clear` 幂等：没有标记时也算成功（接管路径会重复调用它）。
    #[test]
    fn clear_is_idempotent() {
        let root = tmp("clear");
        assert!(clear(&root).is_ok());
        assert!(clear(&root).is_ok());
    }
}
