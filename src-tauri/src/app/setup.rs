#[cfg(target_os = "windows")]
use crate::app::hooks::{keyboard_proc, mouse_proc};
#[cfg(target_os = "windows")]
use crate::app::system::tray_subclass_proc;
use crate::app::window_manager::{release_win_keys, restore_last_focus, toggle_window};
use crate::app_state::{
    AppDataDir, EncryptionQueueState, PasteQueue, SessionHistory, SettingsState,
};
use crate::database::{self, DbState};
use crate::global_state::*;
use crate::{error, info};
use crate::infrastructure::repository::clipboard_repo::SqliteClipboardRepository;
use crate::infrastructure::repository::settings_repo::{
    SettingsRepository, SqliteSettingsRepository,
};
use crate::infrastructure::repository::tag_repo::SqliteTagRepository;
use crate::infrastructure::windows_ext::WindowExt;
use crate::services::encryption_queue::init_encryption_queue;
use crate::services::sensitive_align::spawn_sensitive_alignment;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tauri::{App, AppHandle, Emitter, Manager};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HINSTANCE, HWND, POINT, RECT};
#[cfg(target_os = "windows")]
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Shell::SetWindowSubclass;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, RegisterWindowMessageW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
};

static WINDOW_SIZE_SAVE_PENDING: AtomicBool = AtomicBool::new(false);
static LAST_WINDOW_SIZE_EVENT_MS: AtomicU64 = AtomicU64::new(0);
static LAST_WINDOW_SIZE: OnceLock<Mutex<(u32, u32)>> = OnceLock::new();

/// 边缘停靠判定阈值（物理像素）：窗口边距屏幕边界在此值以内即视为「贴在边缘」。
///
/// 该值必须小于 `window_manager.rs` 中 `AUTO_PLACEMENT_EDGE_MARGIN`（程序自动摆位留白），
/// 否则程序摆位会被误判成用户拖拽到边缘（R1 现象 b/c 的根因）。
const EDGE_DOCK_THRESHOLD: i32 = 5;

/// 判定「用户主动拖拽窗口」时允许的位置抖动（物理像素）。
/// 按住左键期间窗口位移超过该值才认为窗口是被拖动的，而不是被程序或点击顺手改动。
const DRAG_POSITION_TOLERANCE: i32 = 8;

/// 用户拖拽结束后，仍允许视为「拖到边缘」的时间窗（毫秒）。
///
/// 用户把窗口拖到边缘后通常还要把鼠标挪开、等窗口自行停靠，这中间有几百毫秒到几秒，
/// 因此不能只在松手的那一帧判定，而是给一个短时间窗。
const USER_DRAG_PIN_WINDOW_MS: u64 = 4000;

static DRAG_ANCHOR: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static DRAG_MOVED_BY_USER: AtomicBool = AtomicBool::new(false);
static LAST_USER_DRAG_END_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
struct WindowRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

pub fn init(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    let app_handle = app.handle().clone();

    // Initialize GLOBAL_APP_HANDLE for Win32 hooks
    let _ = GLOBAL_APP_HANDLE.set(app_handle.clone());

    // 0. 原生数据目录（**必须在 `resolve_data_dir` 之前取**）
    //
    // 「待接管」标记写在这里，而不是 `AppDataDir`：`AppDataDir` 是**当前生效**的数据
    // 目录，而它可能正是"待接管"的那一个（用户改过数据目录或用便携版时）。把标记放进
    // 待接管的目录里，会与接管动作本身互相踩——接管的第一步就是动那个目录里的库。
    //
    // 原生数据目录由 identifier 推导、位置稳定，且永远不是被接管的那个。
    // 取不到时退化为 `None`：此时接管功能整体不可用（只记日志），**绝不用猜测的路径兜底**。
    let native_data_dir = app.path().app_data_dir().ok();

    // 本次启动是否真的完成了一次"待接管"提升（供 3.1 决定要不要改写库内路径）。
    let mut promoted: Option<std::path::PathBuf> = None;

    // 1. Data Directory & Migration
    let app_dir = resolve_data_dir(app)?;

    // 2. Logger Initialization
    crate::logger::init(app_dir.join("tiez.log"));
    info!(">>> [STARTUP] Tiez-Next starting up...");

    // 2.1 **待接管迁移的启动期执行点**（顺序敏感，勿移动）
    //
    // 这是整条迁移链的落点，位置由三个约束共同决定，缺一不可：
    //
    // 1. **必须在 `init_db`（第 3 步）之前** —— 接管的动作是给目标目录里的
    //    `clipboard.db`（及 `-wal`/`-shm`）改名让位。Windows 不允许改名已打开的
    //    文件，一旦 `init_db` 跑过，这一步就必然报 `os error 32`。在它之前执行时
    //    进程内**尚无任何 `Connection`**，交换必然成功。
    // 2. **必须在 `resolve_data_dir` 之后** —— 需要知道目标数据目录是谁。它内部调用的
    //    `perform_migration_v028` 带 `remove_dir_all`（`migration.rs:93`），
    //    排在那之前会让 V0.2.8 迁移删掉刚放好的文件。
    // 3. **必须在 logger 初始化之后** —— 失败只记日志、不阻断启动，需要有地方记。
    //
    // 失败**绝不阻断启动**：迁移是附加功能，最坏情况是"这次没迁成"，源目录与暂存
    // 目录都还在，标记也还在（下次启动再试）。因此这里只记日志，不 `?`。
    if let Some(native) = native_data_dir.as_deref() {
        promoted = run_pending_takeover(native);
    } else {
        error!(
            "[MIGRATION] 取不到原生应用数据目录，本次跳过「待接管」检查；\
             已复制就绪的迁移数据会保留到下次启动重试。"
        );
    }

    // 3. Database Initialization
    let db_path = app_dir.join("clipboard.db");
    let db_path_str = db_path.to_string_lossy();
    let conn = database::init_db(&db_path_str).map_err(|e| {
        let err_msg = format!("数据库初始化失败: {}", e);
        WindowExt::show_error_box("Tiez-Next 启动错误", &err_msg);
        e
    })?;
    let conn_arc = std::sync::Arc::new(std::sync::Mutex::new(conn));
    let settings_repo = SqliteSettingsRepository::new(conn_arc.clone());

    // 3.1 接管成功后补一次数据库内路径改写
    //
    // 【为什么必须在这里、不能在上面的 2.1 里做】`rewrite_data_paths_in_db` 自己
    // 打开数据库写字符串，必须等 `init_db` 把表建好之后。放在这里正好符合顺序，
    // 且此时连接刚刚建立、尚无任何读取。
    //
    // 【为什么必须做】接管把旧库整体搬进来了，但库里的附件/表情/自定义背景记录的仍是
    // **旧数据目录**下的绝对路径。不改写的话，用户看到的是"记录都在、图片全打不开"。
    if let Some(source) = promoted.as_deref() {
        rewrite_paths_after_takeover(source, &app_dir, &db_path);
    }

    // 4. Initial Settings & Reset Safety
    apply_startup_resets(&settings_repo);

    let settings = load_settings(&settings_repo);

    // 5. App State Management
    setup_state(app, conn_arc.clone(), &settings, app_dir.clone());
    app.manage(EncryptionQueueState(init_encryption_queue(
        app_handle.clone(),
    )));
    spawn_sensitive_alignment(app_handle.clone());

    // 6. Window Initialization (Pinned/Focus)
    setup_main_window(app, &settings);

    // 6.1 External Drag-Drop (Web Images)
    #[cfg(windows)]
    crate::infrastructure::windows_api::drag_drop::register_emoji_drag_drop(app_handle.clone());

    // 7. Background Services & Monitors
    start_services(app, &settings, app_handle.clone());

    // 8. Tray Setup
    setup_tray(app, settings.hide_tray_icon);

    // 9. Theme Initial Application
    apply_initial_theme(app);

    // 10. Win32 Hook Initialization
    #[cfg(target_os = "windows")]
    init_win32_hooks(app);

    // 11. TaskbarCreated & Subclass
    #[cfg(target_os = "windows")]
    setup_taskbar_listener(app);

    // 12. MCP 服务接线
    //
    // 接线只做两件事：把宿主的副作用实现（发事件 / 云同步 / 加解密入队）与审计
    // 日志装进 MCP 模块，然后按用户配置决定是否自动启动。**默认不启动**：写权限
    // 与监听都必须是用户显式打开的。
    crate::services::mcp::install_host(&app_handle);
    crate::services::mcp::autostart_if_configured(&app_handle);

    Ok(())
}

/// 数据目录重定向指针的文件名（用户显式指定数据目录时写它）。
///
/// 【它住在哪里】写字的是 `system_cmd::set_data_path`，位置恒为**原生漫游目录**
/// （`app_data_dir()`）。因此读取也必须只认这一处，不另找第二个指针位置——两个指针
/// 同时存在时无法裁决谁更新，只会让"数据到底在哪"变成猜谜。
const DATA_DIR_REDIRECT_FILE: &str = "datapath.txt";

// ---------------------------------------------------------------------------
// 两阶段迁移的启动期一半
// ---------------------------------------------------------------------------

/// **启动期接管**：把上次运行留下的"待接管"暂存目录提升为正式数据目录。
///
/// 返回 `Some(源目录)` 表示本次启动真的完成了一次接管（调用方据此决定要不要在
/// `init_db` 之后改写库内路径）；`None` 表示没有待接管任务，或接管失败。
///
/// ## 为什么必须在 `init_db` 之前（这一条是整个方案成立的前提）
///
/// 接管的动作是给目标目录里的 `clipboard.db`（及 `-wal`/`-shm`）**改名让位**。
/// Windows 不允许给已打开的文件改名（`ERROR_SHARING_VIOLATION`，os error 32）。
/// 应用一启动就会 `init_db` 打开那个库，连接随后常驻 `DbState`、被 3 个 repo 与
/// `McpStore` 多处持有，**运行期不可能释放**——所以这件事必须在开库之前做完。
///
/// ## 为什么不能在运行期"热替换"连接
///
/// Tauri 的 `app.manage` 对同一类型已存在的状态会**丢弃新值并 `assert!` panic**
/// （`tauri-2.10.2/src/state.rs`），而 `Arc<Mutex<Connection>>` 同时被 `DbState`、
/// 三个 repo 与 `McpStore` 持有，引用计数不可能归零。⇒ **重启是唯一正解**。
///
/// ## 失败绝不阻断启动
///
/// 迁移是附加功能。任何失败都只记日志并**保留标记**（下次启动再试）；暂存目录与源
/// 目录都不会被删。因此本函数没有返回值意义上的错误，调用方无需 `?`。
fn run_pending_takeover(
    native_data_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let outcome = crate::migration_pending::run_startup_takeover(
        native_data_dir,
        &mut |staging: &std::path::Path, target: &std::path::Path| {
            crate::migration_identifier::promote_staged_takeover_default(staging, target)
                .map(|_| ())
        },
    );

    match outcome {
        crate::migration_pending::TakeoverOutcome::NotPending => None,
        crate::migration_pending::TakeoverOutcome::Promoted {
            source_dir,
            target_dir,
        } => {
            info!(
                ">>> [MIGRATION] 已接管待迁移数据：暂存目录已提升为 {:?}（源 {:?} 保持只读、未被改动）。",
                target_dir, source_dir
            );
            Some(source_dir)
        }
        crate::migration_pending::TakeoverOutcome::Failed { reason } => {
            // 【必须只记日志、不阻断启动】迁移失败最多是"这次没迁成"，数据都还在。
            // 标记被保留，下次启动会自动重试。
            error!(
                "[MIGRATION] 待接管的数据本次未能接管（不影响正常使用，下次启动会自动重试）：{}",
                reason
            );
            None
        }
    }
}

/// 接管成功后改写数据库里的绝对路径（附件、表情收藏、自定义背景）。
///
/// 【为什么必须做】接管把旧库整体搬进来了，但库里记录的仍是**旧数据目录**下的绝对
/// 路径。不改写的话，用户看到的是"记录都在、图片全打不开"。
///
/// 【为什么必须在这里】`rewrite_data_paths_in_db` 自己开连接写字符串，必须等
/// `init_db` 把表建好之后才能跑。而接管本身必须在 `init_db` 之前，所以这两件事
/// 天然分处启动流程的两端：接管在前，改写路径在后。
///
/// 改写失败不影响数据本身（记录都已就位），只记日志。
fn rewrite_paths_after_takeover(
    source: &std::path::Path,
    app_dir: &std::path::Path,
    db_path: &std::path::Path,
) {
    match crate::app::commands::system_cmd::rewrite_data_paths_in_db(db_path, source, app_dir) {
        Ok(()) => info!(">>> [MIGRATION] 接管后已在数据库内改写绝对路径。"),
        Err(e) => error!(
            "[MIGRATION] 接管后库内路径改写失败（记录均已就位，仅引用未更新）：{}",
            e
        ),
    }
}


/// 判定"这个目录里确实有本应用的数据"的标志文件。
///
/// 与 `migration_identifier` 的判据保持一致（同为 `clipboard.db`）：只要一个目录里
/// 存在它，就说明用户在这里放过真实数据，任何情况下都不能把它当成空目录对待。
const DATA_DIR_MARKER_FILE: &str = "clipboard.db";

/// 数据目录的**来源**，即解析走到了哪一条分支。
///
/// 单独记下来是为了让"数据为什么在这里"可被日志与测试直接断言，而不是只看到一条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataDirSource {
    /// 用户在设置里显式指定了数据目录（`datapath.txt` 重定向）。**优先级最高**。
    ExplicitRedirect,
    /// 既有安装版用户：数据仍在本机漫游目录 `%APPDATA%\com.tieznext`，原地沿用。
    LegacyRoaming,
    /// 新安装 / 默认：数据放本机目录 `%LOCALAPPDATA%\com.tieznext`。
    LocalDefault,
}

/// 数据目录的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDataDir {
    /// 最终使用的数据目录。
    pub path: std::path::PathBuf,
    /// 选中它的原因。
    pub source: DataDirSource,
}

/// 数据目录解析的**唯一决策点**（纯函数：只吃路径与布尔值，不碰磁盘、不看进程）。
///
/// 优先级（自上而下，先命中者胜）：
/// 1. `datapath.txt` 的显式重定向——用户的显式选择优先于任何自动推断；
/// 2. 漫游目录里已有 `clipboard.db`——既有安装版用户原地沿用，**绝不改换位置**；
/// 3. 默认走本机目录 `%LOCALAPPDATA%\com.tieznext`。
///
/// 【为什么抽成纯函数】真正的 `resolve_data_dir` 依赖 `tauri::App`，测试里造不出来；
/// 而"数据落在哪、为什么落在那里"恰是覆盖升级中最容易伤到用户数据的判断。抽成纯函数
/// 后，全部分支都能在本机做真实断言（见 `setup_tests`）。
pub(crate) fn pick_data_dir(
    roaming: &std::path::Path,
    local: &std::path::Path,
    explicit_redirect: Option<&std::path::Path>,
    roaming_has_database: bool,
) -> ResolvedDataDir {
    if let Some(target) = explicit_redirect {
        return ResolvedDataDir {
            path: target.to_path_buf(),
            source: DataDirSource::ExplicitRedirect,
        };
    }
    if roaming_has_database {
        return ResolvedDataDir {
            path: roaming.to_path_buf(),
            source: DataDirSource::LegacyRoaming,
        };
    }
    ResolvedDataDir {
        path: local.to_path_buf(),
        source: DataDirSource::LocalDefault,
    }
}

/// 读取 `datapath.txt` 并返回**已被采纳**的重定向目标（未采纳则 `None`）。
///
/// 只有"文件存在、内容非空、目标路径真实存在"三条同时成立才采纳；否则返回 `None` 由
/// 调用方回退。**不猜测、不创建**目标目录：用户写在文件里的路径若已不存在（外接盘未插、
/// 目录被删），静默按它建一个空目录会让用户看到"数据没了"。
fn read_explicit_redirect(roaming: &std::path::Path) -> Option<std::path::PathBuf> {
    let redirect_file = roaming.join(DATA_DIR_REDIRECT_FILE);
    if !redirect_file.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&redirect_file).ok()?;
    let custom_path = content.trim();
    if custom_path.is_empty() {
        return None;
    }
    let candidate = std::path::Path::new(custom_path);
    if !candidate.exists() {
        return None;
    }
    Some(candidate.to_path_buf())
}

/// 判定 `data_dir` 是否落在程序安装目录 `program_dir` 之内。
///
/// 这是"数据与执行者分离"这条不变量的**运行时护栏**：只要数据目录被判定在程序目录里，
/// 覆盖升级（整体替换安装目录）与卸载（清理安装目录）都可能连带毁掉用户数据，因此这种
/// 局面必须在启动时就喊出来，而不是安静地接受。
///
/// 【为什么不直接用 `canonicalize`】启动早期两个目录都可能尚不存在，且 Windows 上大小写
/// 与短名（8.3）会造成同一路径的不同写法。这里按路径**组件**比较（Windows 下忽略大小写），
/// 只做"是否被包含"这一件事，不解析符号链接——保守方向是"宁可漏报也不误报"，
/// 因为误报会让用户以为自己的数据有危险。
///
/// 【根目录的例外】可执行文件直接放在盘根（`C:\tiez-next.exe`）时 `program_dir` 就是
/// `C:\`，此时"数据在 C 盘上"完全正常，不能算命中。故要求 `program_dir` 自身还有父级。
pub(crate) fn data_dir_is_inside_program_dir(
    data_dir: &std::path::Path,
    program_dir: &std::path::Path,
) -> bool {
    // 盘根/分卷根不参与判定：否则任何本机路径都会被判成"在程序目录内"。
    if program_dir.parent().is_none() {
        return false;
    }

    let normalize = |p: &std::path::Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let s = c.as_os_str().to_string_lossy().to_string();
                // Windows 路径大小写不敏感：同一目录的两种写法必须判成同一处。
                if cfg!(windows) {
                    s.to_lowercase()
                } else {
                    s
                }
            })
            .collect()
    };

    let data_parts = normalize(data_dir);
    let program_parts = normalize(program_dir);
    if program_parts.is_empty() || program_parts.len() > data_parts.len() {
        return false;
    }
    data_parts[..program_parts.len()] == program_parts[..]
}

/// 漫游目录是否已经装着本应用的数据。
///
/// 判据是**标志文件存在**，而不是"目录存在"：老版本可能在漫游目录里留下过空的目录壳或
/// 只留了日志，那种情况下没有数据要保，不该把用户永久钉在漫游位置。
fn roaming_holds_database(roaming: &std::path::Path) -> bool {
    roaming.join(DATA_DIR_MARKER_FILE).exists()
}

/// 解析本轮启动要使用的数据目录。
///
/// ## 不变量：数据与执行者分离（覆盖升级 / 卸载都不得触碰数据）
///
/// 本函数**绝不把数据目录落到程序安装目录内**，也**绝不把数据目录当成可执行文件的从属
/// 物**：
///
/// - **默认落点在最本机的位置**：`%LOCALAPPDATA%\com.tieznext`。剪贴板历史是本机资产，
///   放漫游目录既无意义（换机不带内容）又会让域环境连带同步大批附件。它与安装目录
///   （currentUser 形态为 `%LOCALAPPDATA%\Tiez-Next`）是**兄弟目录**，互不包含，因此
///   覆盖升级只整体替换安装目录，卸载只删安装目录，数据一律不动。
/// - **历史便携判定（`<exe 同级>/data`）已移除**，理由见本节末尾。
/// - **既有安装版用户的兼容**：他们的数据在 `%APPDATA%\com.tieznext`。只要那里还有
///   `clipboard.db`，本函数就**原地沿用**该目录，绝不移位——覆盖升级后老用户看到的仍是
///   自己的数据，不需要任何迁移动作。
/// - **用户显式指定的目录优先级最高**（`datapath.txt`），高于上面两条自动推断。
///
/// ## 安装流程零数据操作（已逐条审计，改动 installer 时须重新核对）
///
/// 生成产物 `installer.nsi` 里与"删/写数据"有关的动作只有三处，且都不在安装路径上：
/// - `StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"`——只拼**安装目录**（`Tiez-Next`），
///   与数据目录 `com.tieznext` 不是同一个名字，也不会嵌套；
/// - `Section Install` 只做 `SetOutPath`/复制 exe/写 `uninstall.exe`/写注册表/建快捷方式，
///   **没有一步碰数据目录**；
/// - `RmDir /r "$APPDATA\${BUNDLEID}"` 与 `RmDir /r "$LOCALAPPDATA\${BUNDLEID}"` 在
///   **卸载段**，且同时受"用户勾选删除应用数据"与"非更新模式"两个条件守卫。
///
/// 也就是说：**覆盖升级天然不碰数据**——这不是靠约定，而是因为数据目录根本不在安装目录
/// 的路径之下。改动 `tauri.conf.json` 的 `installMode`（会改安装目录）或向 `Section Install`
/// 里添加任何删除动作时，必须回头核对这条不变量。
///
/// ## 为什么不再有便携模式
///
/// 旧实现对"可执行文件同级存在 `data/` 目录"做**无条件覆盖赋值**，于是：
/// 1. 数据被放进**程序目录内**——而 NSIS 卸载器会清理安装目录，数据随程序一起消失；
/// 2. 它**覆盖用户已经显式指定的数据目录**，用户改过的设置重启后又被拽回程序目录；
/// 3. 判定只看 `存在 && 是目录`，不关心是谁创建的、里面有没有东西——用户随手
///    `mkdir data`，或把任何一个自带 `data/` 的压缩包解压进程序目录，都会静默改变
///    数据落点。
///
/// 现在数据位置只由"用户显式指定"与"既有数据在哪"两条真实事实决定。老便携版留下的
/// `<exe 同级>/data` 数据**不做自动探测**（程序无从知道用户把它放在哪个盘哪个目录），
/// 由用户在设置页「迁移中心」手动指定目录迁移；那条路径与本函数无关，保持可用。
fn resolve_data_dir(app: &App) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // 原生漫游目录：既是 v0.2.8 时代迁移的目标，也是 `datapath.txt` 指针的住址。
    let default_app_dir = app.path().app_data_dir()?;

    // Perform migration if needed
    crate::migration::perform_migration_v028(&default_app_dir);

    // 标识符变更（com.tiez / com.tiez.app -> com.tieznext）的数据目录迁移。
    //
    // 【为什么这里不再自动迁移】用户明确要求：迁移必须由用户自己在新版应用里手动
    // 选择旧数据目录后触发，以便"确保不伤害原版，并且还可以多次手动验证"。启动期
    // 自动搬数据与这个要求直接冲突——用户还没机会确认，数据就已经被复制过去了。
    //
    // 因此启动流程在此**不做任何迁移动作**，只保留迁移中心展示所需的只读盘点
    // （`list_legacy_data_dirs`）。真实迁移由设置页「迁移中心」的
    // `migrate_from_data_dir` 命令发起，与原来的自动迁移共用同一套安全契约
    // （见 `system_cmd::apply_identifier_migration`）。
    //
    // 注：上面 `perform_migration_v028` 是 v0.2.8 时代「贴汁 → TieZ」的另一次改名
    // 迁移，与本次标识符迁移是两件独立的事，不在本次范围内，保持原样。

    // Cleanup temp files
    std::thread::spawn(|| {
        let temp_dir = std::env::temp_dir();
        if let Ok(entries) = std::fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    if name.starts_with("Tiez-Next_Clip_") || name.starts_with("TieZ_Clip_") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    });

    // 本机目录：新安装的默认落点（`%LOCALAPPDATA%\com.tieznext`）。
    // 取不到时退回漫游目录——**绝不退回程序目录**：退回程序目录就等于把数据重新塞回
    // 安装目录内，正是本次要根除的布局。
    let local_app_dir = app
        .path()
        .app_local_data_dir()
        .unwrap_or_else(|_| default_app_dir.clone());

    // 程序目录只用于一项事：启动后核对"数据没被放在程序目录里"，不参与任何选择。
    let program_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

    let resolved = resolve_data_dir_impl(
        &default_app_dir,
        &local_app_dir,
        program_dir.as_deref(),
    );

    match resolved.source {
        DataDirSource::ExplicitRedirect => info!(
            ">>> [DATA_DIR] 使用用户显式指定的数据目录（datapath.txt 重定向）: {:?}",
            resolved.path
        ),
        DataDirSource::LegacyRoaming => info!(
            ">>> [DATA_DIR] 沿用既有安装版数据目录（本机漫游，含既有数据库）: {:?}",
            resolved.path
        ),
        DataDirSource::LocalDefault => info!(
            ">>> [DATA_DIR] 使用本机默认数据目录（与安装目录分离）: {:?}",
            resolved.path
        ),
    }

    std::fs::create_dir_all(&resolved.path)?;
    Ok(resolved.path)
}

/// [`resolve_data_dir`] 的**可注入实现**：全部输入来自参数，不读进程状态。
///
/// 抽出来是为了让"数据落在哪、为什么"能被真实文件系统上的测试直接验证——包括最容易
/// 出错、也最伤用户的那一条：**程序目录里存在 `data/` 时，数据目录是否会被拽进去**。
/// 老实现会，现在不会（见 `setup_tests` 的反向对照）。
fn resolve_data_dir_impl(
    roaming: &std::path::Path,
    local: &std::path::Path,
    program_dir: Option<&std::path::Path>,
) -> ResolvedDataDir {
    let explicit_redirect = read_explicit_redirect(roaming);
    let resolved = pick_data_dir(
        roaming,
        local,
        explicit_redirect.as_deref(),
        roaming_holds_database(roaming),
    );

    // 不变量自检：数据目录**不得**落在程序目录内。
    //
    // 唯一还可能命中这条的是用户自己把数据目录显式指到了程序目录里（`datapath.txt`
    // 写了安装目录下的路径）。那不是本程序能替用户决定的事——**不静默改动用户的选择，
    // 也不静默接受**：记一条明确的日志，让"数据会不会被卸载器带走"在排查时有据可查。
    // 正常情况下（本机默认位置 + currentUser 安装）两者是兄弟目录，此分支永不触发。
    if let Some(program_dir) = program_dir {
        if data_dir_is_inside_program_dir(&resolved.path, program_dir) {
            error!(
                "[DATA_DIR] 警告：数据目录位于程序目录内（{:?} ⊂ {:?}）。\
                 覆盖升级会替换程序目录、卸载会清理程序目录，此布局下数据有被一并\
                 清除的风险。若不希望如此，请在设置中把数据目录移到程序目录之外。",
                resolved.path, program_dir
            );
        }
    }

    resolved
}

/// 标识符变更迁移的结果（结构化，供「迁移中心」界面直接展示）。
///
/// 字段与 [`crate::migration_identifier::MigrationOutcome`] 一一对应，只是转成
/// 前端易读的字符串/数字，并补上稳定的原因码（便于界面按语言映射文案）。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifierMigrationReport {
    /// 迁移的最终归属状态（**v0.5.3 冻结契约 §1**）。
    ///
    /// - `"done"` —— 已完成，数据在当前进程里可用；
    /// - `"deferred"` —— 数据已复制就绪，**待下次启动接管**（v0.5.3 新增的常态路径）；
    /// - `"skipped"` —— 无需迁移（既有语义）；
    /// - `"failed"` —— 失败（源与目标均未被破坏）。
    ///
    /// 另有历史值 `"migrated"`：只有内部候选扫描入口（`migrate_legacy_identifier_data`，
    /// 当前无生产调用方）会回它，表示"当场交付成功"。两个对用户暴露的入口
    /// （界面命令与 MCP）只回契约里的四个值。
    ///
    /// 【为什么两阶段迁移是 `deferred` 而不是 `failed`】用户点迁移时应用必定在运行，
    /// 目标库已被 `init_db` 打开着，Windows 不允许给它改名——这在运行期是
    /// **不可避免**的，不是错误。把它当失败呈现会让用户以为功能坏了，而实际上一切
    /// 正常、只差一次重启。`deferred` 是同一件事的诚实表达，界面**不得**用错误样式呈现。
    pub status: String,
    /// 实际被读取的源目录（用户手选或白名单候选）。
    pub source: String,
    /// 目标数据目录。
    pub target: String,
    /// 源侧条目总数（含目录条目，与后端一致性校验口径一致）。
    ///
    /// 注意：这个数会让人**高估**实际交付量（含目录条目、也含目标里原本就有而未被
    /// 覆盖的文件）。界面展示"复制了多少"应优先用 `deliveredFiles`/`deliveredBytes`。
    pub files: u64,
    /// 源侧全部条目的字节数之和（与 `files` 同口径）。
    pub bytes: u64,
    /// 本次**新交付**的文件数（不含目录条目，也不含目标里已存在而沿用的文件）。
    pub delivered_files: u64,
    /// 本次**新交付**的字节数。
    pub delivered_bytes: u64,
    /// 目标里原本就存在、本次未覆盖而沿用的文件数。
    pub kept_existing: u64,
    /// 跳过时的机器可读原因码（见 `SkipReason::code`）。
    pub skip_reason: Option<String>,
    /// 失败原因（源目录此时仍然完好）。
    pub error: Option<String>,
    /// 数据库内绝对路径是否已改写成功（仅 `migrated` 时有意义）。
    pub paths_rewritten: bool,
    /// 改写数据库路径时的错误（若有）。数据已迁移成功，仅引用未更新。
    pub rewrite_error: Option<String>,
    /// 源目录是否全程未被改动。安全契约规定恒为 `true`；界面据此显式告知用户。
    pub source_untouched: bool,
    /// 迁移后目标是否已有可用数据库——为真时**必须重启**才能加载新数据。
    ///
    /// 应用启动时就把数据库连接建好了，迁移是运行中发生的，进程内的连接仍指向迁移前
    /// 的那份数据。因此迁移成功后必须提示用户重启，否则界面看不到刚迁入的记录。
    ///
    /// 【Windows 上的额外理由】本命令会在运行中让位/替换目标目录里的
    /// `clipboard.db`（改名或覆盖）。Windows 不允许改名或覆盖仍被打开的文件——若应用
    /// 正持有该库，这一步可能失败。失败方向是安全的（只清暂存、源与目标都保留，见
    /// `migration_identifier` 的失败路径），但用户会看到迁移未完成。因此界面在失败
    /// 提示里明确建议"先重启应用、不要在迁移刚失败时反复重试"。
    pub restart_required: bool,
    /// 目标里那个"从未使用过的空库"被改名让位后的路径（若有）。
    ///
    /// 只在本次接管了空的新版数据目录时出现；界面据此说明"新版原先的空数据已留档"。
    pub superseded_db: Option<String>,
    /// 已复制就绪、等待下次启动接管的暂存目录（仅 `deferred` 时非空）。
    ///
    /// 【为什么必须回报它】它是"重启后接管"的唯一载体。用户或支持人员需要它来核对
    /// "数据到底复制到哪了"，而在排障时最忌讳的是"后端知道但没说"。
    pub staging_dir: Option<String>,
    /// 是否处于"已复制就绪、待下次启动接管"的状态。
    ///
    /// 与 `status == "deferred"` 是同一件事的两种表达，两个字段由后端一并给出：
    /// 界面既可以按 `status` 分支，也可以直接看这个布尔量。
    pub pending_until_restart: bool,
}

/// 标识符变更迁移的**共享核心**：白名单候选与用户手选路径都走这里。
///
/// 数据安全由 [`crate::migration_identifier`] 的安全契约保证——源目录全程只读、
/// 失败只清暂存、绝不覆盖既有数据，因此最坏情况仅是"没迁成"，用户原数据仍然完整。
///
/// 迁移成功后还需重写数据库内记录着的**绝对路径**（附件、表情收藏、自定义背景），
/// 否则新目录里的数据库仍指向旧目录下的文件，表现为图片/表情丢失。路径改写走
/// `rewrite_data_paths_in_db`——它**只改写数据库里的字符串，不移动也不删除源文件**。
pub fn apply_identifier_migration(
    source: &std::path::Path,
    new_dir: &std::path::Path,
    outcome: crate::migration_identifier::MigrationOutcome,
) -> IdentifierMigrationReport {
    use crate::migration_identifier::MigrationOutcome;

    let mut report = IdentifierMigrationReport {
        source: source.to_string_lossy().to_string(),
        target: new_dir.to_string_lossy().to_string(),
        files: 0,
        bytes: 0,
        delivered_files: 0,
        delivered_bytes: 0,
        kept_existing: 0,
        skip_reason: None,
        error: None,
        paths_rewritten: false,
        rewrite_error: None,
        source_untouched: true,
        restart_required: false,
        superseded_db: None,
        staging_dir: None,
        // 冻结契约 §1 的取值域是 `done|deferred|skipped|failed`。
        // 先给 `skipped` 兜底，随后按 outcome 覆盖。
        status: "skipped".to_string(),
        pending_until_restart: false,
    };

    match &outcome {
        MigrationOutcome::Migrated {
            source,
            files,
            bytes,
            delivered_files,
            delivered_bytes,
            kept_existing,
            yielded_db,
            ..
        } => {
            // 契约 §1：成功且数据**在当前进程可用**时是 `done`。
            //
            // 这里同时保留 `"migrated"` 这个值：`migrate_legacy_identifier_data` 这条
            // 旧入口（当前无调用方，仅测试使用）走的就是它。改成 `done` 会让它无从区分
            // "当场交付成功"与"待接管"。真正对外暴露的两个入口都只回契约里的四个值。
            report.status = "migrated".to_string();
            report.source = source.to_string_lossy().to_string();
            report.files = *files;
            report.bytes = *bytes;
            report.delivered_files = *delivered_files;
            report.delivered_bytes = *delivered_bytes;
            report.kept_existing = *kept_existing;
            report.restart_required = new_dir.join("clipboard.db").exists();
            report.superseded_db =
                yielded_db.as_ref().map(|p| p.to_string_lossy().to_string());
            info!(
                ">>> [MIGRATION] 已从 {:?} 迁移 {} 项（{} 字节）到 {:?}；源目录保留未删除。",
                source, files, bytes, new_dir
            );

            // 数据库内的绝对路径仍需改写，否则引用仍指向旧目录。
            let db_path = new_dir.join("clipboard.db");
            if db_path.exists() {
                match crate::app::commands::system_cmd::rewrite_data_paths_in_db(
                    &db_path, source, new_dir,
                ) {
                    Ok(()) => {
                        report.paths_rewritten = true;
                        info!(">>> [MIGRATION] 数据库内绝对路径已改写完成。");
                    }
                    Err(e) => {
                        // 改写失败不影响数据本身已迁移成功；源目录仍在，可人工恢复。
                        report.rewrite_error = Some(e.to_string());
                        error!(
                            "[MIGRATION] 数据库内路径改写失败（数据已迁移，源仍保留）: {}",
                            e
                        );
                    }
                }
            }
        }
        MigrationOutcome::Skipped(reason) => {
            report.skip_reason = Some(reason.code().to_string());
            info!(">>> [MIGRATION] 无需迁移标识符数据：{:?}", reason);
        }
        MigrationOutcome::Deferred {
            source,
            staging,
            files,
            bytes,
            ..
        } => {
            // 【这不是失败】用户点迁移时应用必定在运行 ⇒ 目标库被 `init_db` 持有 ⇒
            // Windows 不允许给它改名 ⇒ 运行期不可能当场交付。数据已完整复制到暂存，
            // 只差"下次启动在开库之前做交换"这一步。
            report.status = "deferred".to_string();
            report.source = source.to_string_lossy().to_string();
            report.files = *files;
            report.bytes = *bytes;
            report.delivered_files = *files;
            report.delivered_bytes = *bytes;
            // 暂存目录要如实回报：它是"下次启动接管"的载体，用户与支持人员都可能需要它。
            report.staging_dir = Some(staging.to_string_lossy().to_string());
            info!(
                ">>> [MIGRATION] 两阶段迁移第一步完成：{:?} 已复制 {} 项（{} 字节）到暂存 {:?}；等待下次启动接管。",
                source,
                files,
                bytes,
                staging
            );
        }
        MigrationOutcome::Failed { source, error } => {
            report.status = "failed".to_string();
            report.source = source.to_string_lossy().to_string();
            report.error = Some(error.clone());
            // 明确告知用户数据未丢失，避免误以为数据被删。
            error!(
                "[MIGRATION] 迁移未完成（源数据完好、未被修改或删除）: 源={:?} 原因={}",
                source, error
            );
        }
    }

    // SkipReason 的 code() 已在上面的 match 中消费；此处不再额外断言。
    report
}

/// 启动期的安全复位。
///
/// ## 这里**不再**修改用户的粘贴方案设置（曾经会，那是个静默失败）
///
/// 旧实现：读到 `app.paste_method == "game_mode"` 且当前进程未提权时，**直接把设置改回
/// `shift_insert`**，只留一行 `info!` 日志。
///
/// 为什么那是错的（而不是"保守"）：
/// 1. **它是静默的**——用户没有任何途径知道自己的选择被改掉了。设置页上看到的
///    "游戏模式"在下次启动后变成了别的东西，且没有任何解释。
/// 2. **它每次启动都执行**——用户改回去、重启、又没了，表现成"设置保存不住"。
/// 3. **它把用户的选择当成可由应用单方面推翻的东西**。而提权与否是用户的环境选择，
///    不是用户表达"不要游戏模式"。
///
/// ## 现在的行为
///
/// **保留用户的选择**，把"当前未提权、游戏模式可能不生效"这个事实交给界面告知，
/// 并提供一键提权重启入口（`restart_as_admin`）。判定按需计算、不落库，
/// 因此用户在设置页里改回标准方案后，告知会自然消失，不需要额外的清理逻辑。
///
/// 【为什么不留一个"自动回退"的兜底】兜底会让"用户以为开着"与"实际生效"继续分叉，
/// 只是把分叉藏得更深：粘贴行为看着正常了，但设置页显示的仍是游戏模式。
/// 告知 + 提权入口才是把分叉摆到明面上。
fn apply_startup_resets(repo: &impl SettingsRepository) {
    let paste_method = repo
        .get("app.paste_method")
        .unwrap_or(Some("shift_insert".to_string()))
        .unwrap_or("shift_insert".to_string());
    if paste_method == "game_mode" && !crate::app::commands::system_cmd::check_is_admin() {
        // 只记录，不改设置。界面会用同一个判据把这件事告诉用户。
        info!(
            ">>> [STARTUP] 检测到粘贴方案为 game_mode 且当前未提权：\
             保持用户设置不变（不再静默改回），改由界面告知并提供一键提权重启入口。"
        );
    }
}

/// 粘贴方案的实际生效状态（供界面如实告知，不改任何设置）。
///
/// 【为什么要有这个命令】"用户选了游戏模式"与"游戏模式真的能用"是两件事，差别就在
/// 当前进程有没有提权。这个差别以前被后端悄悄抹平（改回默认方案），用户因此永远
/// 看不到真相。现在后端只回答事实，界面负责告知。
#[derive(Debug, serde::Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PasteMethodStatus {
    /// 用户当前配置的粘贴方案（原样回传，不做任何替换）。
    pub method: String,
    /// 当前进程是否已提权。
    pub is_admin: bool,
    /// 该配置在当前权限下是否按用户预期生效。
    pub effective: bool,
    /// 该方案是否**需要**提权才完整生效（目前只有 `game_mode`）。
    pub requires_admin: bool,
}

/// 计算粘贴方案状态（纯函数，便于在任意平台上断言"未提权时不改设置只报告"）。
///
/// 界面用同一套判据（`app.paste_method` + `check_is_admin` 命令）决定要不要显示告知，
/// 因此这里不新增命令、也不落任何状态：状态是**按需推导**的，用户改回标准方案后
/// 告知会自然消失。
pub fn paste_method_status_from(method: &str, is_admin: bool) -> PasteMethodStatus {
    let requires_admin = method == "game_mode";
    PasteMethodStatus {
        method: method.to_string(),
        is_admin,
        effective: !requires_admin || is_admin,
        requires_admin,
    }
}

pub struct StartupSettings {
    pub theme: String,
    pub persistent: bool,
    pub capture_files: bool,
    pub capture_rich_text: bool,
    pub deduplicate: bool,
    pub auto_copy_file: bool,
    pub silent_start: bool,
    pub delete_after_paste: bool,
    pub privacy_protection: bool,
    pub privacy_kinds: String,
    pub privacy_custom: String,
    pub cleanup_rules: String,
    pub app_cleanup_policies: String,
    pub sequential_mode: bool,
    pub sequential_hotkey: String,
    pub rich_paste_hotkey: String,
    pub search_hotkey: String,
    pub quick_paste_modifier: String,
    pub sound_enabled: bool,
    pub hide_tray_icon: bool,
    pub edge_docking: bool,
    pub follow_mouse: bool,
    pub window_pinned: bool,
    pub window_width: Option<u32>,
    pub window_height: Option<u32>,
    pub main_hotkey: String,
    pub arrow_key_selection: bool,
    pub auto_close_server: bool,
}

fn load_settings(repo: &impl SettingsRepository) -> StartupSettings {
    StartupSettings {
        theme: repo
            .get("app.theme")
            .unwrap_or(Some("retro".to_string()))
            .unwrap_or("retro".to_string()),
        persistent: repo
            .get("app.persistent")
            .unwrap_or(Some("true".to_string()))
            .map(|v| v == "true")
            .unwrap_or(true),
        capture_files: repo
            .get("app.capture_files")
            .unwrap_or(Some("true".to_string()))
            .map(|v| v == "true")
            .unwrap_or(true),
        capture_rich_text: repo
            .get("app.capture_rich_text")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        deduplicate: repo
            .get("app.deduplicate")
            .unwrap_or(Some("true".to_string()))
            .map(|v| v == "true")
            .unwrap_or(true),
        auto_copy_file: repo
            .get("file_transfer_auto_copy")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        silent_start: repo
            .get("app.silent_start")
            .unwrap_or(Some("true".to_string()))
            .map(|v| v == "true")
            .unwrap_or(true),
        delete_after_paste: repo
            .get("app.delete_after_paste")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        privacy_protection: repo
            .get("app.privacy_protection")
            .unwrap_or(Some("true".to_string()))
            .map(|v| v == "true")
            .unwrap_or(true),
        privacy_kinds: repo
            .get("app.privacy_protection_kinds")
            .unwrap_or(Some("phone,idcard,email,secret".to_string()))
            .unwrap_or("phone,idcard,email,secret".to_string()),
        privacy_custom: repo
            .get("app.privacy_protection_custom_rules")
            .unwrap_or(Some("".to_string()))
            .unwrap_or("".to_string()),
        cleanup_rules: repo
            .get("app.cleanup_rules")
            .unwrap_or(Some("".to_string()))
            .unwrap_or("".to_string()),
        app_cleanup_policies: repo
            .get("app.app_cleanup_policies")
            .unwrap_or(Some("[]".to_string()))
            .unwrap_or("[]".to_string()),
        sequential_mode: repo
            .get("app.sequential_mode")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        sequential_hotkey: repo
            .get("app.sequential_hotkey")
            .unwrap_or(Some("Alt+V".to_string()))
            .unwrap_or("Alt+V".to_string()),
        rich_paste_hotkey: repo
            .get("app.rich_paste_hotkey")
            .unwrap_or(Some("Ctrl+Shift+Z".to_string()))
            .unwrap_or("Ctrl+Shift+Z".to_string()),
        search_hotkey: repo
            .get("app.search_hotkey")
            .unwrap_or(Some("Alt+F".to_string()))
            .unwrap_or("Alt+F".to_string()),
        quick_paste_modifier: repo
            .get("app.quick_paste_modifier")
            .unwrap_or(Some("disabled".to_string()))
            .unwrap_or("disabled".to_string()),
        sound_enabled: repo
            .get("app.sound_enabled")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        hide_tray_icon: repo
            .get("app.hide_tray_icon")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        edge_docking: repo
            .get("app.edge_docking")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        follow_mouse: repo
            .get("app.follow_mouse")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        window_pinned: repo
            .get("app.window_pinned")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        window_width: repo
            .get("app.window_width")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u32>().ok()),
        window_height: repo
            .get("app.window_height")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u32>().ok()),
        main_hotkey: repo
            .get("app.hotkey")
            .unwrap_or(Some("Win+V".to_string()))
            .unwrap_or("Win+V".to_string()),
        arrow_key_selection: repo
            .get("app.arrow_key_selection")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
        auto_close_server: repo
            .get("file_transfer_auto_close")
            .unwrap_or(Some("false".to_string()))
            .map(|v| v == "true")
            .unwrap_or(false),
    }
}

fn setup_state(
    app: &App,
    conn_arc: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
    s: &StartupSettings,
    app_dir: std::path::PathBuf,
) {
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let settings_repo = SqliteSettingsRepository::new(conn_arc.clone());
    let tag_repo = SqliteTagRepository::new(conn_arc.clone());
    app.manage(DbState {
        conn: conn_arc,
        repo,
        settings_repo,
        tag_repo,
    });

    app.manage(SettingsState {
        deduplicate: AtomicBool::new(s.deduplicate),
        persistent: AtomicBool::new(s.persistent),
        file_server_auto_close: AtomicBool::new(s.auto_close_server),
        theme: std::sync::Mutex::new(s.theme.clone()),
        capture_files: AtomicBool::new(s.capture_files),
        capture_rich_text: AtomicBool::new(s.capture_rich_text),
        auto_copy_file: AtomicBool::new(s.auto_copy_file),
        silent_start: AtomicBool::new(s.silent_start),
        delete_after_paste: AtomicBool::new(s.delete_after_paste),
        privacy_protection: AtomicBool::new(s.privacy_protection),
        privacy_protection_kinds: std::sync::Mutex::new(
            s.privacy_kinds
                .split(',')
                .map(|x| x.trim().to_string())
                .collect(),
        ),
        privacy_protection_custom_rules: std::sync::Mutex::new(
            s.privacy_custom
                .lines()
                .map(|x| x.trim().to_string())
                .collect(),
        ),
        cleanup_rules: std::sync::Mutex::new(s.cleanup_rules.clone()),
        app_cleanup_policies: std::sync::Mutex::new(s.app_cleanup_policies.clone()),
        sequential_mode: AtomicBool::new(s.sequential_mode),
        sequential_paste_hotkey: std::sync::Mutex::new(s.sequential_hotkey.clone()),
        rich_paste_hotkey: std::sync::Mutex::new(s.rich_paste_hotkey.clone()),
        search_hotkey: std::sync::Mutex::new(s.search_hotkey.clone()),
        quick_paste_modifier: std::sync::Mutex::new(s.quick_paste_modifier.clone()),
        sound_enabled: AtomicBool::new(s.sound_enabled),
        hide_tray_icon: AtomicBool::new(s.hide_tray_icon),
        edge_docking: AtomicBool::new(s.edge_docking),
        follow_mouse: AtomicBool::new(s.follow_mouse),
        arrow_key_selection: AtomicBool::new(s.arrow_key_selection),
        main_hotkey: std::sync::Mutex::new(s.main_hotkey.clone()),
        monitors: std::sync::Mutex::new(Vec::new()),
    });

    app.manage(SessionHistory(std::sync::Mutex::new(
        std::collections::VecDeque::new(),
    )));
    app.manage(AppDataDir(std::sync::Mutex::new(app_dir)));
    app.manage(crate::services::file_transfer::ChatState::default());
    app.manage(crate::services::file_transfer::SharedFileState(
        std::sync::Mutex::new(std::collections::HashMap::new()),
    ));
    app.manage(crate::services::file_transfer::ServerInfo {
        port: std::sync::atomic::AtomicU16::new(0),
        ip: std::sync::Mutex::new(String::new()),
    });
    app.manage(crate::services::file_transfer::UploadSessions::default());
    app.manage(crate::services::file_transfer::ServerActivityState::default());
    app.manage(crate::services::file_transfer::WsBroadcaster(
        std::sync::Mutex::new(None),
    ));
    app.manage(crate::services::file_transfer::OnlineDevices(
        std::sync::Mutex::new(std::collections::HashMap::new()),
    ));
    app.manage(PasteQueue::default());
}

fn setup_main_window(app: &App, s: &StartupSettings) {
    let effective_pinned = s.window_pinned;
    WINDOW_PINNED.store(effective_pinned, Ordering::Relaxed);

    if let Some(window) = app.get_webview_window("main") {
        if let (Some(w), Some(h)) = (s.window_width, s.window_height) {
            if w >= 360 && h >= 240 {
                let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize {
                    width: w,
                    height: h,
                }));
            }
        }
        let _ = window.set_always_on_top(effective_pinned);
        let _ = window.set_focusable(!effective_pinned);

        #[cfg(windows)]
        if let Ok(hwnd) = window.hwnd() {
            unsafe {
                let ex_style = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                    HWND(hwnd.0),
                    GWL_EXSTYLE,
                );
                if effective_pinned {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                        HWND(hwnd.0),
                        GWL_EXSTYLE,
                        ex_style | WS_EX_NOACTIVATE.0 as isize,
                    );
                } else {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                        HWND(hwnd.0),
                        GWL_EXSTYLE,
                        ex_style & !(WS_EX_NOACTIVATE.0 as isize),
                    );
                }
            }
        }

        if repair_window_position_if_needed(&window, s.edge_docking) {
            IS_HIDDEN.store(false, Ordering::Relaxed);
            CURRENT_DOCK.store(0, Ordering::Relaxed);
        }

        // 记录启动时窗口所在的显示器，作为后续「唤起屏」判定的基线
        crate::app::window_manager::refresh_recall_monitor(&window);
    }

    schedule_window_position_repair(app.handle().clone(), s.edge_docking);

    // Handle silent start
    let args: Vec<String> = std::env::args().collect();
    let is_autostart =
        args.contains(&"--autostart".to_string()) || args.contains(&"--minimized".to_string());
    if !is_autostart && !s.silent_start {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
        }
    }
}

fn schedule_window_position_repair(app_handle: AppHandle, edge_docking_enabled: bool) {
    std::thread::spawn(move || {
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(250));

            let Some(window) = app_handle.get_webview_window("main") else {
                continue;
            };

            if repair_window_position_if_needed(&window, edge_docking_enabled) {
                IS_HIDDEN.store(false, Ordering::Relaxed);
                CURRENT_DOCK.store(0, Ordering::Relaxed);
                info!(">>> [STARTUP] Repaired off-screen window position after state restore.");
                break;
            }
        }
    });
}

fn repair_window_position_if_needed(
    window: &tauri::WebviewWindow,
    edge_docking_enabled: bool,
) -> bool {
    let Ok(position) = window.outer_position() else {
        return false;
    };
    let Ok(size) = window.outer_size() else {
        return false;
    };
    let Ok(monitors) = window.available_monitors() else {
        return false;
    };
    if monitors.is_empty() {
        return false;
    }

    let rect = WindowRect {
        x: position.x,
        y: position.y,
        width: size.width as i32,
        height: size.height as i32,
    };

    if rect.width <= 0 || rect.height <= 0 {
        return false;
    }

    let visible_enough = monitors
        .iter()
        .any(|monitor| window_rect_has_enough_visible_area(rect, monitor, edge_docking_enabled));
    if visible_enough {
        return false;
    }

    let target_monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
        .or_else(|| monitors.first().cloned());

    let Some(monitor) = target_monitor else {
        return false;
    };

    let (target_x, target_y) = clamp_window_rect_to_monitor(rect, &monitor);
    if target_x == rect.x && target_y == rect.y {
        return false;
    }

    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
        x: target_x,
        y: target_y,
    }));
    true
}

fn window_rect_has_enough_visible_area(
    rect: WindowRect,
    monitor: &tauri::Monitor,
    edge_docking_enabled: bool,
) -> bool {
    let monitor_pos = monitor.position();
    let monitor_size = monitor.size();
    let monitor_left = monitor_pos.x;
    let monitor_top = monitor_pos.y;
    let monitor_right = monitor_left + monitor_size.width as i32;
    let monitor_bottom = monitor_top + monitor_size.height as i32;

    let visible_left = rect.x.max(monitor_left);
    let visible_top = rect.y.max(monitor_top);
    let visible_right = (rect.x + rect.width).min(monitor_right);
    let visible_bottom = (rect.y + rect.height).min(monitor_bottom);
    let visible_width = (visible_right - visible_left).max(0);
    let visible_height = (visible_bottom - visible_top).max(0);

    if visible_width == 0 || visible_height == 0 {
        return false;
    }

    let min_visible_width = if edge_docking_enabled {
        24.min(rect.width)
    } else {
        1
    };
    let min_visible_height = if edge_docking_enabled {
        24.min(rect.height)
    } else {
        1
    };

    visible_width >= min_visible_width && visible_height >= min_visible_height
}

fn clamp_window_rect_to_monitor(rect: WindowRect, monitor: &tauri::Monitor) -> (i32, i32) {
    let monitor_pos = monitor.position();
    let monitor_size = monitor.size();
    let margin = 10;

    let min_x = monitor_pos.x + margin;
    let min_y = monitor_pos.y + margin;
    let max_x = (monitor_pos.x + monitor_size.width as i32 - rect.width - margin).max(min_x);
    let max_y = (monitor_pos.y + monitor_size.height as i32 - rect.height - margin).max(min_y);

    let target_x = if rect.width + margin * 2 >= monitor_size.width as i32 {
        monitor_pos.x
    } else {
        rect.x.clamp(min_x, max_x)
    };
    let target_y = if rect.height + margin * 2 >= monitor_size.height as i32 {
        monitor_pos.y
    } else {
        rect.y.clamp(min_y, max_y)
    };

    (target_x, target_y)
}

fn start_services(app: &App, s: &StartupSettings, app_handle: AppHandle) {
    crate::infrastructure::windows_api::window_tracker::start_window_tracking(app_handle.clone());
    crate::services::clipboard::start_clipboard_monitor(app_handle.clone());
    crate::services::mqtt_sub::start_mqtt_client(app_handle.clone());
    crate::services::cloud_sync::start_cloud_sync_client(app_handle.clone());
    start_edge_docking_monitor(app_handle.clone());

    let db_state = app.state::<DbState>();
    if db_state
        .settings_repo
        .get("file_server_enabled")
        .unwrap_or(Some("false".to_string()))
        == Some("true".to_string())
    {
        let port = db_state
            .settings_repo
            .get("file_server_port")
            .unwrap_or(None)
            .and_then(|x| x.parse::<u16>().ok());

        let h = app_handle.clone();
        tauri::async_runtime::spawn(async move {
            let _ = crate::services::file_transfer::toggle_file_server(h, true, port).await;
        });
    }

    // Register active hotkeys based on current settings.
    let _ = crate::app::commands::register_hotkey(app_handle.clone(), s.main_hotkey.clone());

    // Win+V 键名统一：先把历史上分叉的旧键搬到唯一真键上，再按真键执行优化。
    migrate_win_v_setting_key_once(&db_state.settings_repo);

    // Win+V Optimization
    //
    // 【为什么只认 `app.use_win_v_shortcut`】历史上这个功能有两个键名同时存在
    // （`app.use_win_v_shortcut` 与 `app.registry_win_v_enabled`，分别由后端与前端的
    // 不同年代代码读写）。两处各写各的，结果**永远是同一个功能有两份可能矛盾的状态**：
    // 界面上开关亮着，后端却按另一个键判定为关闭，于是优化分支永不执行。
    // 统一到一个键之后，"界面显示的"与"后端触发的"才可能是同一件事。
    if db_state
        .settings_repo
        .get(WIN_V_SETTING_KEY)
        .unwrap_or(Some("false".to_string()))
        == Some("true".to_string())
    {
        if !crate::app::commands::system_cmd::get_registry_win_v_optimized_status() {
            let _ = crate::app::commands::trigger_registry_win_v_optimization(true);
            crate::info!(
                ">>> [WINV] 已按设置启用 Win+V 接管（若 Win+V 仍被系统占用，需要重启资源管理器才会生效）。"
            );
        }
    }
}

/// Win+V 设置的**唯一真键**。
///
/// 前端开关、后端启动优化、云同步快照一律只认它。改这里等于改契约，需同步前端
/// `useSettingsPostInit.ts` 的读取点。
pub const WIN_V_SETTING_KEY: &str = "app.use_win_v_shortcut";

/// 历史上被另一处代码使用的分叉键名（只作为一次性迁移的**输入**，不再被读写）。
pub const WIN_V_LEGACY_SETTING_KEY: &str = "app.registry_win_v_enabled";

/// 一次性迁移：把旧键的值搬到新键（**只在新键缺失时**）。
///
/// ## 为什么必须做，且必须是"缺失才搬"
///
/// 已经用过这个版本的用户库里可能只存在旧键。不搬的话，用户先前打开的开关在升级后
/// 会**静默变成关闭**——而用户看不出任何原因（设置页上那个开关本来就是刚恢复的）。
///
/// 但反向也要防：若用户已经在新键上表达过意愿（新键存在），旧键的值就是陈旧残留，
/// **绝不能覆盖**新键——否则升级会把用户最近的选择回退成很久以前的旧值。
///
/// 迁移完成后旧键保留（不删）：删改用户数据的方向上是不可逆的，而留一个不再被读取的
/// 键没有任何行为代价。真需要清理时，它会在下一次全量重写设置时自然消失。
fn migrate_win_v_setting_key_once(repo: &impl SettingsRepository) {
    let existing_new = repo.get(WIN_V_SETTING_KEY).unwrap_or(None);
    if existing_new.is_some() {
        return; // 新键已在，用户的最近意愿优先，旧键不动也不覆盖
    }
    let legacy = match repo.get(WIN_V_LEGACY_SETTING_KEY) {
        Ok(Some(v)) => v,
        Ok(None) => return, // 两个键都没有：全新用户，无事可做
        Err(e) => {
            crate::error!("[WINV] 读取旧键失败，跳过迁移（不改任何设置）：{}", e);
            return;
        }
    };
    match repo.set(WIN_V_SETTING_KEY, &legacy) {
        Ok(()) => {
            // 写后回读：迁移也必须能被证明真的落地了。
            match repo.get(WIN_V_SETTING_KEY) {
                Ok(Some(read_back)) if read_back == legacy => crate::info!(
                    ">>> [WINV] 已把旧键 {}={} 迁移到唯一真键 {}（回读一致）。",
                    WIN_V_LEGACY_SETTING_KEY,
                    legacy,
                    WIN_V_SETTING_KEY
                ),
                other => crate::error!(
                    "[WINV] 旧键迁移后回读不一致：写入={:?} 回读={:?}（保留原状，不重试）",
                    legacy,
                    other
                ),
            }
        }
        Err(e) => crate::error!("[WINV] 旧键迁移写入失败：{}", e),
    }
}

/// 读取窗口可用的显示器列表（物理像素矩形）。
#[cfg(target_os = "windows")]
fn monitor_rects_of<W: MonitorQuery>(window: &W) -> Vec<MonitorRect> {
    window.monitor_rects()
}

/// 拖拽探测器：判断「窗口是被用户主动拖到边缘的」，而不是被程序摆位摆过去的。
///
/// 依据是 Windows 的实时按键状态与窗口位置变化：
/// - 按住左键（VK_LBUTTON）期间以按下瞬间的窗口位置为锚点；
/// - 锚点位移一旦超过 `DRAG_POSITION_TOLERANCE`，即认定窗口是被用户拖动的；
/// - 左键松开时结算：确实拖动过则标记「用户拖拽结束」并记录时刻；
/// - 随后 `USER_DRAG_PIN_WINDOW_MS` 内视为拖拽收尾阶段。
///
/// 程序用 `set_position` 摆窗时不伴随左键按下，因此永远无法进入该状态。
///
/// 注意：本函数必须在每轮轮询中**无条件调用**。若只在「窗口已处于边缘」的分支里调用，
/// 用户从屏幕中间把窗口拖到边缘的整个过程都不会被采样，到头来攒不够位移量而漏判。
#[cfg(target_os = "windows")]
fn update_user_drag_state(window_rect: &RECT) -> bool {
    let lbutton_down = unsafe { (GetAsyncKeyState(0x01) as u16 & 0x8000) != 0 };
    let mut anchor = match DRAG_ANCHOR.lock() {
        Ok(guard) => guard,
        Err(_) => return false,
    };

    if lbutton_down {
        match *anchor {
            None => {
                // 左键按下的第一帧：以当前位置为基准。若这一帧起始位置已被程序摆到边缘，
                // 位移累计从零开始，之后没有真实拖动就攒不出位移量。
                *anchor = Some((window_rect.left, window_rect.top));
            }
            Some((ax, ay)) => {
                if drag_offset_exceeds_tolerance((ax, ay), (window_rect.left, window_rect.top), DRAG_POSITION_TOLERANCE)
                {
                    // 相对锚点持续累计：一次按住期间只要出现过超过容差的总位移即判定为拖动
                    DRAG_MOVED_BY_USER.store(true, Ordering::Relaxed);
                }
            }
        }
    } else if anchor.is_some() {
        // 左键刚松开：只有真的移动过才算用户拖拽
        *anchor = None;
        if DRAG_MOVED_BY_USER.swap(false, Ordering::Relaxed) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            LAST_USER_DRAG_END_MS.store(now, Ordering::Relaxed);
            return true;
        }
    }

    false
}

/// 一次「按住左键」的位移是否已经越过容差，即窗口是否真的被用户拖过。
///
/// 从锚点到当前累计位移按曼哈顿距离判断，窗口只动 X 或只动 Y 都能识别。
pub fn drag_offset_exceeds_tolerance(
    anchor: (i32, i32),
    current: (i32, i32),
    tolerance: i32,
) -> bool {
    (current.0 - anchor.0).abs() > tolerance || (current.1 - anchor.1).abs() > tolerance
}

/// 距离上次「用户主动拖拽窗口」结束是否仍在收尾时间窗内。
#[cfg(target_os = "windows")]
fn within_user_drag_window(now: u64) -> bool {
    let last = LAST_USER_DRAG_END_MS.load(Ordering::Relaxed);
    last != 0 && now.saturating_sub(last) <= USER_DRAG_PIN_WINDOW_MS
}

/// 自动置顶的唯一开关点：只有「用户主动拖拽窗口到边缘」才允许自动置顶。
///
/// R1 现象 b 的根因是原实现只要判定贴边就置顶，而 `window_manager.rs` 的程序摆位
/// 恰好会落在与停靠阈值重合的 5px 内（主副屏交界正是主屏的一条边），于是自动摆位被
/// 误判成贴边并置顶。修复后：
/// - 程序摆位（无左键拖拽）永不触发置顶；
/// - 用户拖拽到边缘后的收尾时间窗内可触发一次置顶；
/// - 用户手动点图钉的置顶/取消置顶完全不受影响（走 `set_window_pinned` 命令）。
#[cfg(target_os = "windows")]
fn maybe_auto_pin_on_user_dock(app_handle: &AppHandle, window: &tauri::WebviewWindow, now: u64) {
    if !within_user_drag_window(now) {
        return;
    }
    if WINDOW_PINNED.load(Ordering::Relaxed) {
        return;
    }

    WINDOW_PINNED.store(true, Ordering::Relaxed);
    let _ = window.set_always_on_top(true);
    let _ = window.set_focusable(false);
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let ex_style = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                HWND(hwnd.0),
                GWL_EXSTYLE,
            );
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                HWND(hwnd.0),
                GWL_EXSTYLE,
                ex_style | WS_EX_NOACTIVATE.0 as isize,
            );
        }
    }
    let _ = app_handle.emit("window-pinned-changed", true);
}

#[cfg(target_os = "windows")]
fn start_edge_docking_monitor(app_handle: AppHandle) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(150));

            let settings = match app_handle.try_state::<SettingsState>() {
                Some(s) => s,
                None => continue,
            };

            // Drag sampling runs before every `continue` below, on purpose.
            // Those guards skip docking *decisions*, but a user drag can begin and end
            // inside their windows (the 500ms post-show grace especially), and the
            // anchor/moved state would then never advance — so a real drag to the edge
            // would silently fail to auto-pin. The function's effect is its global
            // state; the return value is informative only.
            if let Some(window) = app_handle.get_webview_window("main") {
                let mut drag_rect = RECT::default();
                if let Ok(hwnd) = window.hwnd() {
                    unsafe {
                        let _ = GetWindowRect(HWND(hwnd.0), &mut drag_rect);
                    }
                    let _is_user_drag_ended = update_user_drag_state(&drag_rect);
                }
            }

            if !settings.edge_docking.load(Ordering::Relaxed) {
                if IS_HIDDEN.load(Ordering::Relaxed) {
                    if let Some(window) = app_handle.get_webview_window("main") {
                        let _ = window.show();
                        IS_HIDDEN.store(false, Ordering::Relaxed);
                        CURRENT_DOCK.store(0, Ordering::Relaxed);
                    }
                }
                continue;
            }

            if let Some(window) = app_handle.get_webview_window("main") {
                // Skip if window is minimized
                if window.is_minimized().unwrap_or(false) {
                    continue;
                }

                let is_window_visible = window.is_visible().unwrap_or(true);
                let is_hidden_by_edge = IS_HIDDEN.load(Ordering::Relaxed);

                // Skip edge docking checks if window was hidden by other mechanisms (paste, blur, etc.)
                if !is_window_visible && !is_hidden_by_edge {
                    continue;
                }

                let last_show = LAST_SHOW_TIMESTAMP.load(Ordering::Relaxed);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                // While the clipboard window is actively shown via hotkey navigation,
                // avoid immediate auto-docking right after showing.
                if !is_hidden_by_edge
                    && NAVIGATION_ENABLED.load(Ordering::SeqCst)
                    && now.saturating_sub(last_show) < 800
                {
                    continue;
                }

                // Grace period after showing to prevent immediate re-dock
                if now.saturating_sub(last_show) < 500 {
                    continue;
                }

                let mut rect = RECT::default();
                let hwnd = match window.hwnd() {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                unsafe {
                    let _ = GetWindowRect(HWND(hwnd.0), &mut rect);
                }

                let mut point = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut point);
                }

                // Get current monitor info and validate
                let monitor = match window.current_monitor() {
                    Ok(Some(m)) => m,
                    _ => continue,
                };
                let screen_size = monitor.size();
                let screen_pos = monitor.position();

                // Calculate monitor boundaries
                let screen_left = screen_pos.x;
                let screen_top = screen_pos.y;
                let screen_right = screen_pos.x + screen_size.width as i32;
                let screen_bottom = screen_pos.y + screen_size.height as i32;

                // When hidden, check if mouse is near the edge sliver
                let threshold = EDGE_DOCK_THRESHOLD;
                let is_mouse_near_edge = if is_hidden_by_edge {
                    let current_dock = CURRENT_DOCK.load(Ordering::Relaxed);
                    match current_dock {
                        1 => {
                            point.y <= screen_top + threshold
                                && point.x >= rect.left
                                && point.x <= rect.right
                        } // Top
                        2 => {
                            point.x <= screen_left + threshold
                                && point.y >= rect.top
                                && point.y <= rect.bottom
                        } // Left
                        3 => {
                            point.x >= screen_right - threshold
                                && point.y >= rect.top
                                && point.y <= rect.bottom
                        } // Right
                        _ => false,
                    }
                } else {
                    false
                };

                let is_mouse_in = if is_hidden_by_edge {
                    is_mouse_near_edge
                } else {
                    point.x >= rect.left
                        && point.x <= rect.right
                        && point.y >= rect.top
                        && point.y <= rect.bottom
                };

                // Ensure window is actually on this monitor
                let window_center_x = (rect.left + rect.right) / 2;
                let window_center_y = (rect.top + rect.bottom) / 2;
                let is_on_current_monitor = window_center_x >= screen_left
                    && window_center_x < screen_right
                    && window_center_y >= screen_top
                    && window_center_y < screen_bottom;

                if !is_hidden_by_edge && !is_on_current_monitor {
                    if IS_HIDDEN.load(Ordering::Relaxed) {
                        IS_HIDDEN.store(false, Ordering::Relaxed);
                        CURRENT_DOCK.store(0, Ordering::Relaxed);
                    }
                    continue;
                }

                let hide_size = 3;

                let mut dock = DockPosition::None;
                if rect.top <= screen_top + threshold {
                    dock = DockPosition::Top;
                } else if rect.left <= screen_left + threshold {
                    dock = DockPosition::Left;
                } else if rect.right >= screen_right - threshold {
                    dock = DockPosition::Right;
                }

                if is_hidden_by_edge {
                    if is_mouse_in {
                        let current_dock = CURRENT_DOCK.load(Ordering::Relaxed);
                        let dock_actual = match current_dock {
                            1 => DockPosition::Top,
                            2 => DockPosition::Left,
                            3 => DockPosition::Right,
                            _ => DockPosition::None,
                        };

                        if dock_actual != DockPosition::None {
                            let _ = window.show();
                            match dock_actual {
                                DockPosition::Top => {
                                    let _ = window.set_position(tauri::Position::Physical(
                                        tauri::PhysicalPosition {
                                            x: rect.left,
                                            y: screen_top,
                                        },
                                    ));
                                }
                                DockPosition::Left => {
                                    let _ = window.set_position(tauri::Position::Physical(
                                        tauri::PhysicalPosition {
                                            x: screen_left,
                                            y: rect.top,
                                        },
                                    ));
                                }
                                DockPosition::Right => {
                                    let w = rect.right - rect.left;
                                    let _ = window.set_position(tauri::Position::Physical(
                                        tauri::PhysicalPosition {
                                            x: screen_right - w,
                                            y: rect.top,
                                        },
                                    ));
                                }
                                _ => {}
                            }

                            IS_HIDDEN.store(false, Ordering::Relaxed);
                            CURRENT_DOCK.store(0, Ordering::Relaxed);
                        }
                    }
                } else if dock != DockPosition::None {
                    // Don't dock while dragging (Left Mouse Button down)
                    let is_lbutton_down = unsafe { (GetAsyncKeyState(0x01) as u16 & 0x8000) != 0 };
                    if is_mouse_in || is_lbutton_down {
                        continue;
                    }

                    if !IS_HIDDEN.load(Ordering::Relaxed) {
                        // R1 现象 b 修复：自动置顶只在「用户主动拖拽窗口到边缘」后触发
                        // （见 `maybe_auto_pin_on_user_dock`）。窗口被程序自动摆位、
                        // 或仅因贴近主副屏交界而满足贴边判定时，WINDOW_PINNED 保持不变。
                        maybe_auto_pin_on_user_dock(&app_handle, &window, now);

                        let window_height = rect.bottom - rect.top;
                        let window_width = rect.right - rect.left;
                        match dock {
                            DockPosition::Top => {
                                let _ = window.set_position(tauri::PhysicalPosition::new(
                                    rect.left,
                                    screen_top - window_height + hide_size,
                                ));
                                CURRENT_DOCK.store(1, Ordering::Relaxed);
                            }
                            DockPosition::Left => {
                                let _ = window.set_position(tauri::PhysicalPosition::new(
                                    screen_left - window_width + hide_size,
                                    rect.top,
                                ));
                                CURRENT_DOCK.store(2, Ordering::Relaxed);
                            }
                            DockPosition::Right => {
                                let _ = window.set_position(tauri::PhysicalPosition::new(
                                    screen_right - hide_size,
                                    rect.top,
                                ));
                                CURRENT_DOCK.store(3, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                        IS_HIDDEN.store(true, Ordering::Relaxed);
                    }
                } else if IS_HIDDEN.load(Ordering::Relaxed) {
                    IS_HIDDEN.store(false, Ordering::Relaxed);
                    CURRENT_DOCK.store(0, Ordering::Relaxed);

                    // Restore pinned state based on user setting when undocked
                    let mut user_pinned = WINDOW_PINNED.load(Ordering::Relaxed);
                    if let Some(db_state) = app_handle.try_state::<DbState>() {
                        if let Ok(val) = db_state.settings_repo.get("app.window_pinned") {
                            user_pinned = val.as_deref() == Some("true");
                        }
                    }

                    let prev = WINDOW_PINNED.swap(user_pinned, Ordering::Relaxed);
                    if prev != user_pinned {
                        let _ = window.set_always_on_top(user_pinned);
                        let _ = window.set_focusable(!user_pinned);
                        #[cfg(windows)]
                        if let Ok(hwnd) = window.hwnd() {
                            unsafe {
                                let ex_style =
                                    windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                                        HWND(hwnd.0),
                                        GWL_EXSTYLE,
                                    );
                                let next = if user_pinned {
                                    ex_style | WS_EX_NOACTIVATE.0 as isize
                                } else {
                                    ex_style & !(WS_EX_NOACTIVATE.0 as isize)
                                };
                                let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                                    HWND(hwnd.0),
                                    GWL_EXSTYLE,
                                    next,
                                );
                            }
                        }
                        let _ = app_handle.emit("window-pinned-changed", user_pinned);
                    }
                }
            }
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn start_edge_docking_monitor(_app_handle: AppHandle) {}

fn setup_tray(app: &App, hide_tray: bool) {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};

    let show_i = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>).unwrap();
    let quit_i = MenuItem::with_id(app, "quit", "退出 Tiez-Next", true, None::<&str>).unwrap();
    let menu = Menu::with_items(app, &[&show_i, &quit_i]).unwrap();
    let icon =
        tauri::image::Image::from_bytes(include_bytes!("../../icons/tray-icon.png")).unwrap();

    let tray = TrayIconBuilder::with_id("main_tray")
        .icon(icon)
        .tooltip("Tiez-Next")
        .show_menu_on_left_click(false)
        .menu(&menu)
        .on_menu_event(|app, event| {
            if event.id.as_ref() == "show" {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                }
            } else if event.id.as_ref() == "quit" {
                app.exit(0);
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                if let Some(window) = tray.app_handle().get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    LAST_SHOW_TIMESTAMP.store(now, Ordering::Relaxed);
                }
            }
        })
        .build(app)
        .expect("Failed to build tray");

    let _ = tray.set_visible(!hide_tray);
    app.manage(tray);
}

fn apply_initial_theme(app: &App) {
    let db_state = app.state::<DbState>();
    let theme = db_state
        .settings_repo
        .get("app.theme")
        .unwrap_or(Some("retro".to_string()))
        .unwrap_or("retro".to_string());
    let mode = db_state
        .settings_repo
        .get("app.color_mode")
        .unwrap_or(Some("system".to_string()));

    if let Some(window) = app.get_webview_window("main") {
        let _ = crate::app::commands::set_theme(
            window,
            app.state::<SettingsState>(),
            db_state,
            theme,
            mode,
            None,
        );
    }
}

#[cfg(target_os = "windows")]
fn init_win32_hooks(_app: &App) {
    std::thread::spawn(move || {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
            UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL, WH_MOUSE_LL,
        };
        unsafe {
            HOOK_THREAD_ID.store(
                windows::Win32::System::Threading::GetCurrentThreadId(),
                Ordering::Relaxed,
            );
            let h_instance = GetModuleHandleW(None).expect("Failed to get module handle");
            let h_hook = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_proc),
                Some(HINSTANCE(h_instance.0)),
                0,
            )
            .expect("Failed to set hook");
            HOOK_HANDLE.store(h_hook.0 as _, Ordering::SeqCst);
            let h_mouse_hook = SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_proc),
                Some(HINSTANCE(h_instance.0)),
                0,
            )
            .expect("Failed to set mouse hook");
            HOOK_MOUSE_HANDLE.store(h_mouse_hook.0 as _, Ordering::SeqCst);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnhookWindowsHookEx(h_hook);
            let h_mouse = HOOK_MOUSE_HANDLE.swap(null_mut(), Ordering::SeqCst);
            if !h_mouse.is_null() {
                let _ = UnhookWindowsHookEx(windows::Win32::UI::WindowsAndMessaging::HHOOK(
                    h_mouse as _,
                ));
            }
        }
    });
}

#[cfg(target_os = "windows")]
fn setup_taskbar_listener(app: &App) {
    unsafe {
        let msg = RegisterWindowMessageW(windows::core::w!("TaskbarCreated"));
        if msg != 0 {
            TASKBAR_CREATED_MSG.store(msg, Ordering::Relaxed);
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(hwnd) = window.hwnd() {
                    let _ = SetWindowSubclass(HWND(hwnd.0), Some(tray_subclass_proc), 1337, 0);
                }
            }
        }
    }
}

pub fn handle_global_shortcut(app: &AppHandle, shortcut: &tauri_plugin_global_shortcut::Shortcut) {
    use tauri_plugin_global_shortcut::Shortcut;
    let settings = app.state::<SettingsState>();

    if let Ok(main_s) = {
        let val = settings.main_hotkey.lock().unwrap().clone();
        val.replace("Win", "Super").parse::<Shortcut>()
    } {
        if shortcut == &main_s {
            toggle_window(app);
            return;
        }
    }

    if let Ok(seq_s) = {
        let val = settings.sequential_paste_hotkey.lock().unwrap().clone();
        val.replace("Win", "Super").parse::<Shortcut>()
    } {
        if shortcut == &seq_s {
            let is_seq = settings.sequential_mode.load(Ordering::Relaxed);
            let has_items = {
                let q_notification = app.state::<PasteQueue>().inner().0.lock().unwrap();
                !q_notification.items.is_empty()
            };
            if is_seq || has_items {
                tauri::async_runtime::spawn({
                    let app = app.clone();
                    async move {
                        crate::services::paste_queue::paste_next_step(app).await;
                    }
                });
            }
        }
    }

    if let Ok(rich_s) = {
        let val = settings.rich_paste_hotkey.lock().unwrap().clone();
        val.replace("Win", "Super").parse::<Shortcut>()
    } {
        if shortcut == &rich_s {
            crate::services::clipboard_ops::paste_latest_rich(app.clone());
        }
    }

    if let Ok(search_s) = {
        let val = settings.search_hotkey.lock().unwrap().clone();
        val.replace("Win", "Super").parse::<Shortcut>()
    } {
        if shortcut == &search_s {
            toggle_window(app);
            let _ = app.emit("focus-search-input", ());
        }
    }
}

pub fn handle_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    match event {
        tauri::WindowEvent::Focused(focused) => {
            if window.label() != "main" {
                return;
            }
            IS_MAIN_WINDOW_FOCUSED.store(*focused, Ordering::Relaxed);
            if *focused {
                #[cfg(target_os = "windows")]
                unsafe {
                    let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
                    if !hwnd.0.is_null() {
                        if let Ok(h) = window.hwnd() {
                            if hwnd.0 != h.0 {
                                crate::LAST_ACTIVE_HWND.store(hwnd.0 as usize, Ordering::Relaxed);
                            }
                        }
                    }
                }
            } else {
                handle_blur(window);
            }
        }
        tauri::WindowEvent::Resized(size) => {
            if window.label() != "main" {
                return;
            }
            if window.is_minimized().unwrap_or(false) || window.is_maximized().unwrap_or(false) {
                return;
            }
            persist_window_size(window, size.width, size.height);
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            if window.label() != "main" {
                return;
            }
            api.prevent_close();
            let _ = window.hide();
            NAVIGATION_ENABLED.store(false, Ordering::SeqCst);
            NAVIGATION_MODE_ACTIVE.store(false, Ordering::SeqCst);
        }
        _ => {}
    }
}

fn persist_window_size(window: &tauri::Window, width: u32, height: u32) {
    if width < 200 || height < 200 {
        return;
    }

    let store = LAST_WINDOW_SIZE.get_or_init(|| Mutex::new((0, 0)));
    {
        let mut guard = store.lock().unwrap();
        *guard = (width, height);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    LAST_WINDOW_SIZE_EVENT_MS.store(now, Ordering::Relaxed);

    if WINDOW_SIZE_SAVE_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }

    let app_handle = window.app_handle().clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let last_event = LAST_WINDOW_SIZE_EVENT_MS.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        if now.saturating_sub(last_event) < 200 {
            continue;
        }

        let (w, h) = {
            let guard = LAST_WINDOW_SIZE.get().unwrap().lock().unwrap();
            *guard
        };

        if let Some(db_state) = app_handle.try_state::<DbState>() {
            let _ = db_state
                .settings_repo
                .set("app.window_width", &w.to_string());
            let _ = db_state
                .settings_repo
                .set("app.window_height", &h.to_string());
        }

        WINDOW_SIZE_SAVE_PENDING.store(false, Ordering::SeqCst);
        break;
    });
}

fn handle_blur(window: &tauri::Window) {
    if IGNORE_BLUR.load(Ordering::Relaxed) || WINDOW_PINNED.load(Ordering::Relaxed) {
        return;
    }

    let settings = window.app_handle().state::<SettingsState>();
    if settings.edge_docking.load(Ordering::Relaxed) {
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    if now.saturating_sub(LAST_SHOW_TIMESTAMP.load(Ordering::Relaxed)) < 500 {
        return;
    }

    if IS_MOUSE_BUTTON_DOWN.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        if (windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x01) as u16 & 0x8000)
            != 0
            || (windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x02) as u16 & 0x8000)
                != 0
        {
            return;
        }
    }

    let w = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let down = IS_MOUSE_BUTTON_DOWN.load(Ordering::SeqCst)
            || unsafe {
                (windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x01) as u16
                    & 0x8000)
                    != 0
                    || (windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x02) as u16
                        & 0x8000)
                        != 0
            };
        if !down && matches!(w.is_focused(), Ok(false)) {
            if !IGNORE_BLUR.load(Ordering::Relaxed) && !WINDOW_PINNED.load(Ordering::Relaxed) {
                // R1 现象 c 修复：鼠标已经在另一块屏幕上时，失焦不隐藏窗口。
                // 双屏用户点另一块屏继续干活会让本窗口失焦，但那是正常操作而非「离开」，
                // 隐藏它等于打断；同屏失焦仍保持既有隐藏行为。
                if cursor_is_on_other_monitor(&w) {
                    return;
                }

                let _ = w.hide();
                NAVIGATION_ENABLED.store(false, Ordering::SeqCst);
                release_win_keys();
                let _ = restore_last_focus(w.app_handle().clone());
            }
        }
    });
}

/// 光标当前是否位于「窗口所在显示器」之外的另一块显示器上。
///
/// 取不到显示器信息时返回 false，即退回既有隐藏语义，不做跨屏豁免。
#[cfg(target_os = "windows")]
fn cursor_is_on_other_monitor<W: MonitorQuery>(window: &W) -> bool {
    let monitors = monitor_rects_of(window);
    if monitors.is_empty() {
        return false;
    }

    let window_monitor = window.current_monitor_rect();
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }

    is_point_on_other_monitor(window_monitor, &monitors, point.x, point.y)
}

#[cfg(test)]
mod setup_tests {
    use super::{drag_offset_exceeds_tolerance, DRAG_POSITION_TOLERANCE};

    #[test]
    fn drag_detection_ignores_programmatic_placement_jitter() {
        // 程序用 set_position 摆窗不会伴随左键按下；即便位置变化在容差以内，
        // 也不能被判成「用户拖拽」，否则自动置顶会被程序摆位触发（R1 现象 b）。
        assert!(!drag_offset_exceeds_tolerance(
            (100, 100),
            (100 + DRAG_POSITION_TOLERANCE, 100),
            DRAG_POSITION_TOLERANCE
        ));
        assert!(!drag_offset_exceeds_tolerance(
            (100, 100),
            (100 - DRAG_POSITION_TOLERANCE, 100 - DRAG_POSITION_TOLERANCE),
            DRAG_POSITION_TOLERANCE
        ));
    }

    #[test]
    fn drag_detection_accepts_real_user_drag() {
        // 用户拖动窗口：横移、竖移、斜移都要被识别
        assert!(drag_offset_exceeds_tolerance(
            (100, 100),
            (100 + DRAG_POSITION_TOLERANCE + 1, 100),
            DRAG_POSITION_TOLERANCE
        ));
        assert!(drag_offset_exceeds_tolerance(
            (100, 100),
            (100, 100 - DRAG_POSITION_TOLERANCE - 1),
            DRAG_POSITION_TOLERANCE
        ));
        // 从屏幕中间拖到边缘的完整位移，必须远超容差
        assert!(drag_offset_exceeds_tolerance(
            (800, 500),
            (0, 500),
            DRAG_POSITION_TOLERANCE
        ));
    }

    /// **启动顺序守卫（v0.5.3 新增）**：待接管迁移必须在 `init_db` **之前**执行。
    ///
    /// # 为什么必须用源码顺序断言，而不是行为测试
    ///
    /// 这条顺序是**整个两阶段方案成立的前提**：接管的动作是给目标目录里的
    /// `clipboard.db` 改名让位，而 Windows **不允许**给已打开的文件改名
    /// （`ERROR_SHARING_VIOLATION`）。`init_db` 一跑，那个库就被打开、连接常驻
    /// `DbState`（还被 3 个 repo 与 `McpStore` 共享），**运行期不可能释放**。
    ///
    /// 而这件事没有可观察的运行时行为可供断言：在 Linux 上给已打开的文件改名照样
    /// 成功（所以即便顺序错了、本机的行为测试也会全绿），在 Windows 上则表现为
    /// "用户重启后数据没进来"——一个**只出现在真机、只在错误顺序下发生**的静默失败。
    /// 换句话说，行为测试在这个平台上**原理上抓不到它**。
    ///
    /// 因此这里直接对源码的**文本顺序**下断言：这是"锁住一个不可在本机观测的顺序
    /// 约束"唯一可靠的办法。断言很粗（比较两个字符串的位置），但它守的是一条
    /// 一旦被破坏就会让功能在真机上彻底失效的约束——粗略但有效，远好过没有。
    ///
    /// 【反向对照实测】把 `init` 里那段 `run_pending_takeover(native)` 移动到
    /// `let conn = database::init_db(...)` **之后**，本条失败。
    #[test]
    fn startup_takeover_runs_before_the_database_is_opened() {
        // 只取 `init` 函数体：它内部的顺序才是被守的对象。
        let source = include_str!("setup.rs");
        let init_start = source.find("pub fn init(app: &mut App)").expect("init 必须存在");
        let body = &source[init_start..];

        let takeover_at = body
            .find("promoted = run_pending_takeover(")
            .expect("init 必须调用 run_pending_takeover");
        let init_db_at = body
            .find("database::init_db(&db_path_str)")
            .expect("init 必须调用 database::init_db");

        assert!(
            takeover_at < init_db_at,
            "待接管迁移必须在 `database::init_db` **之前**执行：\
             一旦库被打开，Windows 就不允许给它改名（os error 32），\
             而连接会常驻 DbState、运行期不可能释放——顺序错了功能在真机上必然失效。\
             （takeover 偏移 {takeover_at}，init_db 偏移 {init_db_at}）"
        );

        // `resolve_data_dir` 必须在两者之前：它内部会调用
        // `perform_migration_v028`，后者含 `remove_dir_all`（`migration.rs:93`），
        // 排在接管之后会把刚放好的文件删掉。
        let resolve_at = body
            .find("let app_dir = resolve_data_dir(app)?")
            .expect("init 必须调用 resolve_data_dir");
        assert!(
            resolve_at < takeover_at,
            "数据目录必须在接管之前解析（perform_migration_v028 带 remove_dir_all，\
             排错顺序会让它删掉刚接管进来的数据）"
        );
    }

    #[test]
    fn dock_threshold_stays_below_auto_placement_margin() {
        // 5px 停靠阈值与 40px 自动摆位留白必须不相等，否则「程序摆到边缘」= 「用户拖到边缘」
        assert_ne!(
            super::EDGE_DOCK_THRESHOLD,
            crate::app::window_manager::AUTO_PLACEMENT_EDGE_MARGIN
        );
        assert!(super::EDGE_DOCK_THRESHOLD < crate::app::window_manager::AUTO_PLACEMENT_EDGE_MARGIN);
    }
}

/// 「数据与执行者分离」的回归测试。
///
/// ## 这一组测试在守什么
///
/// 覆盖升级会整体替换程序目录、卸载会清理程序目录。因此只要**数据目录落在程序目录内**，
/// 用户不可再生的剪贴板历史就会随程序一起消失。这组测试锁死三件事：
///
/// 1. 默认情况下数据目录**不在**程序目录内；
/// 2. 老实现那条"程序目录内有 `data/` 就把数据搬进去"的便携判定**已彻底移除**；
/// 3. 两条兼容路径（用户显式 `datapath.txt` 重定向、既有安装版用户的漫游目录）**继续可用**。
///
/// 全部基于真实文件系统，且经 `resolve_data_dir_impl` 走完整条解析链——不是只看常量。
#[cfg(test)]
mod data_dir_separation_tests {
    use super::{
        data_dir_is_inside_program_dir, pick_data_dir, read_explicit_redirect,
        resolve_data_dir_impl, DataDirSource,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    /// 造一个隔离的临时根目录；名字带 pid 与纳秒，避免并发测试互相踩。
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-datadir-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 模拟一次"全新安装"的目录形状，返回 (漫游目录, 本机目录, 程序目录)。
    fn fresh_install_layout(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let roaming = root.join("Roaming").join("com.tieznext");
        let local = root.join("Local").join("com.tieznext");
        // currentUser 形态的安装目录：%LOCALAPPDATA%\Tiez-Next（与数据目录是兄弟目录）。
        let program = root.join("Local").join("Tiez-Next");
        fs::create_dir_all(&program).unwrap();
        (roaming, local, program)
    }

    /// 造一个"目录里确实有本应用数据"的标志（用真实文件，不是空目录）。
    fn seed_database(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("clipboard.db"), b"SQLite format 3\0").unwrap();
    }

    // ---------------------------------------------------------------
    // 1. 默认落点：数据目录不在程序目录内
    // ---------------------------------------------------------------

    #[test]
    fn fresh_install_puts_data_outside_the_program_directory() {
        let root = tmp("fresh");
        let (roaming, local, program) = fresh_install_layout(&root);

        let resolved = resolve_data_dir_impl(&roaming, &local, Some(&program));

        assert_eq!(
            resolved.source,
            DataDirSource::LocalDefault,
            "全新安装且两处都没有既有数据时，应使用本机默认数据目录"
        );
        assert_eq!(resolved.path, local);
        assert!(
            !data_dir_is_inside_program_dir(&resolved.path, &program),
            "数据目录绝不能落在程序目录内：数据={:?} 程序={:?}",
            resolved.path,
            program
        );
        // 与安装目录是兄弟：同一个父目录，但不是父子关系。
        assert_eq!(resolved.path.parent(), program.parent());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn program_directory_is_treated_as_a_root_not_a_container() {
        let root = tmp("program-as-root");

        // 反向：数据**确实**在程序目录内时必须被认出来（护栏本身要有效）。
        assert!(data_dir_is_inside_program_dir(
            Path::new("/opt/Tiez-Next/data"),
            Path::new("/opt/Tiez-Next")
        ));
        assert!(data_dir_is_inside_program_dir(
            Path::new("/opt/Tiez-Next"),
            Path::new("/opt/Tiez-Next")
        ));
        // 兄弟目录不算命中。
        assert!(!data_dir_is_inside_program_dir(
            Path::new("/opt/com.tieznext"),
            Path::new("/opt/Tiez-Next")
        ));
        // 前缀相同但不同名（`Tiez-Next-old` vs `Tiez-Next`）不算命中——按路径组件比较，
        // 不做字符串前缀匹配，否则会把无关目录误判成"数据有危险"。
        assert!(!data_dir_is_inside_program_dir(
            Path::new("/opt/Tiez-Next-old"),
            Path::new("/opt/Tiez-Next")
        ));

        fs::remove_dir_all(&root).ok();
    }

    // ---------------------------------------------------------------
    // 2. 便携判定已移除（本次的关键行为变更）
    // ---------------------------------------------------------------

    /// **本次改动的核心断言**：程序目录里存在 `data/` 时，数据目录**不得**变成它。
    ///
    /// 老实现会无条件 `app_dir = <exe 同级>/data`；改动后这条判定整体移除。
    /// 反向对照记录：把 `resolve_data_dir_impl` 换回老实现（在解析末尾加回便携覆盖赋值），
    /// 本测试立即失败——见报告"反向对照"一节。
    #[test]
    fn portable_data_dir_is_ignored_even_when_it_exists() {
        let root = tmp("portable-ignored");
        let (roaming, local, program) = fresh_install_layout(&root);

        // 目录形状完全照搬便携包：程序目录里有一个装着实数据的 data/。
        let portable_data = program.join("data");
        seed_database(&portable_data);
        assert!(portable_data.join("clipboard.db").is_file());

        let resolved = resolve_data_dir_impl(&roaming, &local, Some(&program));

        assert_ne!(
            resolved.path, portable_data,
            "程序目录内的 data/ 不得再被当作数据目录（便携判定必须已移除）"
        );
        assert_ne!(resolved.source, DataDirSource::LegacyRoaming);
        assert_eq!(resolved.path, local);
        assert!(
            !data_dir_is_inside_program_dir(&resolved.path, &program),
            "即便程序目录里有 data/，数据目录也必须在程序目录之外"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 便携判定还会**覆盖用户已经显式指定的数据目录**——这条一并锁死。
    ///
    /// 用户改了数据位置、程序目录里又恰好有 `data/` 时，老实现重启后会把用户拽回程序
    /// 目录。现在显式指定优先级最高，任何自动推断都不得推翻它。
    #[test]
    fn explicit_redirect_outranks_any_automatic_inference() {
        let root = tmp("explicit-wins");
        let (roaming, local, program) = fresh_install_layout(&root);
        fs::create_dir_all(&roaming).unwrap();
        // 程序目录里有 data/，漫游目录里也有既有数据库——两个"自动推断"都在场。
        seed_database(&program.join("data"));
        seed_database(&roaming);

        let chosen = root.join("ChosenByUser");
        seed_database(&chosen);
        fs::write(
            roaming.join("datapath.txt"),
            chosen.to_string_lossy().as_bytes(),
        )
        .unwrap();

        let resolved = resolve_data_dir_impl(&roaming, &local, Some(&program));

        assert_eq!(resolved.source, DataDirSource::ExplicitRedirect);
        assert_eq!(resolved.path, chosen, "显式指定的数据目录优先级必须最高");

        fs::remove_dir_all(&root).ok();
    }

    // ---------------------------------------------------------------
    // 3. 兼容性：既有安装版用户的数据必须继续被读到
    // ---------------------------------------------------------------

    /// **兼容性核心**：老用户数据在 `%APPDATA%\com.tieznext`，升级后必须原地沿用。
    ///
    /// 这条一旦失败，用户看到的就是"升级后数据全没了"。因此它比"新默认落点"更重要：
    /// 新落点让**新用户**受益，这条让**老用户**不受损。
    #[test]
    fn existing_roaming_data_is_reused_so_old_users_keep_their_data() {
        let root = tmp("legacy-roaming");
        let (roaming, local, program) = fresh_install_layout(&root);

        // 既有安装版用户：漫游目录里有真实数据，本机目录还不存在。
        seed_database(&roaming);
        assert!(!local.exists());

        let resolved = resolve_data_dir_impl(&roaming, &local, Some(&program));

        assert_eq!(
            resolved.source,
            DataDirSource::LegacyRoaming,
            "漫游目录已有数据库时必须原地沿用，不得改换位置"
        );
        assert_eq!(resolved.path, roaming);
        // 数据并未因为"默认落点变了"而被搬走或新建空目录。
        assert!(roaming.join("clipboard.db").is_file());
        assert!(!local.exists(), "沿用既有数据时不得顺手新建另一个数据目录");

        fs::remove_dir_all(&root).ok();
    }

    /// 漫游目录只有空壳（没有数据库）时，应落到新默认位置——
    /// 判据是"有没有数据"，不是"目录存不存在"。
    #[test]
    fn empty_roaming_shell_does_not_pin_user_to_the_roaming_location() {
        let root = tmp("empty-roaming");
        let (roaming, local, program) = fresh_install_layout(&root);
        fs::create_dir_all(&roaming).unwrap();
        // 老版本可能留下的痕迹：只有日志，没有数据库。
        fs::write(roaming.join("tiez.log"), b"old log").unwrap();

        let resolved = resolve_data_dir_impl(&roaming, &local, Some(&program));

        assert_eq!(
            resolved.source,
            DataDirSource::LocalDefault,
            "没有数据库就不是\"有数据要保\"，不该把用户永久钉在漫游位置"
        );
        assert_eq!(resolved.path, local);

        fs::remove_dir_all(&root).ok();
    }

    /// 三者同时在场时的完整优先级：显式重定向 > 既有漫游数据 > 本机默认。
    #[test]
    fn priority_order_is_explicit_then_legacy_then_local_default() {
        let root = tmp("priority");
        let (roaming, local, program) = fresh_install_layout(&root);
        fs::create_dir_all(&roaming).unwrap();

        // 最底层：什么都没有 → 本机默认。
        assert_eq!(
            resolve_data_dir_impl(&roaming, &local, Some(&program)).source,
            DataDirSource::LocalDefault
        );

        // 加上既有漫游数据 → 沿用漫游。
        seed_database(&roaming);
        assert_eq!(
            resolve_data_dir_impl(&roaming, &local, Some(&program)).source,
            DataDirSource::LegacyRoaming
        );

        // 再加上显式重定向 → 重定向胜出。
        let chosen = root.join("ChosenByUser");
        seed_database(&chosen);
        fs::write(
            roaming.join("datapath.txt"),
            chosen.to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert_eq!(
            resolve_data_dir_impl(&roaming, &local, Some(&program)).source,
            DataDirSource::ExplicitRedirect
        );

        fs::remove_dir_all(&root).ok();
    }

    // ---------------------------------------------------------------
    // 4. `datapath.txt` 重定向机制本身仍然生效（不得因本次改动而破坏）
    // ---------------------------------------------------------------

    #[test]
    fn redirect_file_is_honoured_when_it_points_at_a_real_directory() {
        let root = tmp("redirect-ok");
        let (roaming, _local, _program) = fresh_install_layout(&root);
        fs::create_dir_all(&roaming).unwrap();

        let target = root.join("CustomData");
        seed_database(&target);
        fs::write(
            roaming.join("datapath.txt"),
            target.to_string_lossy().as_bytes(),
        )
        .unwrap();

        assert_eq!(read_explicit_redirect(&roaming).as_deref(), Some(target.as_path()));

        fs::remove_dir_all(&root).ok();
    }

    /// 指针指向的目标已不存在（外接盘未插、目录被删）时**不采纳**，回退到自动推断。
    ///
    /// 这条防的是"静默按一个已经不存在的路径建空目录，用户看到数据没了"：
    /// 采纳之前先确认目标真实存在。
    #[test]
    fn redirect_pointing_at_a_missing_directory_is_not_adopted() {
        let root = tmp("redirect-missing");
        let (roaming, local, program) = fresh_install_layout(&root);
        seed_database(&roaming);

        let missing = root.join("UnpluggedDrive").join("data");
        fs::write(
            roaming.join("datapath.txt"),
            missing.to_string_lossy().as_bytes(),
        )
        .unwrap();

        assert_eq!(read_explicit_redirect(&roaming), None);
        let resolved = resolve_data_dir_impl(&roaming, &local, Some(&program));
        assert_eq!(
            resolved.source,
            DataDirSource::LegacyRoaming,
            "指针失效时必须回退到既有数据，而不是按失效路径建空目录"
        );
        assert!(!missing.exists(), "不得按失效指针创建目录");

        fs::remove_dir_all(&root).ok();
    }

    /// 空文件 / 只有空白的指针同样不采纳（旧版本可能留过空文件）。
    #[test]
    fn blank_redirect_file_is_not_adopted() {
        let root = tmp("redirect-blank");
        let (roaming, local, program) = fresh_install_layout(&root);
        fs::create_dir_all(&roaming).unwrap();

        fs::write(roaming.join("datapath.txt"), b"").unwrap();
        assert_eq!(read_explicit_redirect(&roaming), None);
        assert_eq!(
            resolve_data_dir_impl(&roaming, &local, Some(&program)).source,
            DataDirSource::LocalDefault
        );

        fs::write(roaming.join("datapath.txt"), b"   \r\n  ").unwrap();
        assert_eq!(read_explicit_redirect(&roaming), None);
        assert_eq!(
            resolve_data_dir_impl(&roaming, &local, Some(&program)).source,
            DataDirSource::LocalDefault
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 指针内容前后的空白与换行必须被裁掉（用户手写文件时很常见）。
    #[test]
    fn redirect_file_content_is_trimmed() {
        let root = tmp("redirect-trim");
        let (roaming, _local, _program) = fresh_install_layout(&root);
        fs::create_dir_all(&roaming).unwrap();

        let target = root.join("CustomData");
        seed_database(&target);
        fs::write(
            roaming.join("datapath.txt"),
            format!("  {}\r\n", target.to_string_lossy()),
        )
        .unwrap();

        assert_eq!(read_explicit_redirect(&roaming).as_deref(), Some(target.as_path()));

        fs::remove_dir_all(&root).ok();
    }

    // ---------------------------------------------------------------
    // 5. 决策函数自身的边界（不依赖磁盘）
    // ---------------------------------------------------------------

    #[test]
    fn resolver_never_returns_the_program_directory_by_construction() {
        // 决策函数**根本不接收程序目录**：数据目录无法由"程序在哪"推导出来。
        // 这是"便携判定已移除"在类型层面的体现——传不进去的东西不可能被采纳。
        let roaming = Path::new(r"C:\Users\u\AppData\Roaming\com.tieznext");
        let local = Path::new(r"C:\Users\u\AppData\Local\com.tieznext");
        let program = Path::new(r"C:\Users\u\AppData\Local\Tiez-Next");

        let r = pick_data_dir(roaming, local, None, false);
        assert_eq!(r.path, Path::new(r"C:\Users\u\AppData\Local\com.tieznext"));
        assert!(!data_dir_is_inside_program_dir(&r.path, program));

        // 即便漫游目录已被判定有数据库，结果也只是漫游目录，绝不会是程序目录。
        let r2 = pick_data_dir(roaming, local, None, true);
        assert_ne!(r2.path, program);
        assert!(!data_dir_is_inside_program_dir(&r2.path, program));
    }

    /// Windows 路径大小写不敏感：同一目录的两种写法必须判成"在程序目录内"，
    /// 否则护栏会漏掉真实危险布局。
    #[test]
    fn containment_check_ignores_case_on_windows() {
        let inside = data_dir_is_inside_program_dir(
            Path::new(r"C:\Users\U\AppData\Local\TIEZ-NEXT\data"),
            Path::new(r"C:\Users\u\appdata\local\tiez-next"),
        );
        if cfg!(windows) {
            assert!(inside, "Windows 下同目录不同大小写必须判定为包含");
        } else {
            // Linux 上大小写敏感，两种写法确实是不同的目录，不算包含。
            assert!(!inside);
        }
    }
}

// ---------------------------------------------------------------------------
// 启动期行为：不改用户设置、Win+V 键名统一
//
// 【三处必须守住的契约】
// 1. 未提权时**不得**修改 `app.paste_method`（这就是"静默改用户设置"本身）；
// 2. Win+V 只认 `app.use_win_v_shortcut` 一个键；
// 3. 旧键的值要能被搬到新键上，但**新键已存在时绝不覆盖**（用户的最近意愿优先）。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod startup_safety_tests {
    use super::*;
    use crate::infrastructure::repository::settings_repo::SettingsRepository;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// 内存版 settings 仓库：让这三条契约能在不碰真库的情况下被断言。
    ///
    /// 用真实的 `SqliteSettingsRepository` 也可以，但那会把"启动期不写设置"这条断言
    /// 与 SQLite 的行为耦合在一起；这里要验证的是**调用方有没有发出写入**，
    /// 因此一个能记录写入的内存实现更直接，也更能暴露"偷偷改了一笔"。
    #[derive(Default)]
    struct MemRepo {
        rows: Arc<Mutex<HashMap<String, String>>>,
        writes: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MemRepo {
        fn with(rows: &[(&str, &str)]) -> Self {
            let me = Self::default();
            for (k, v) in rows {
                me.rows.lock().unwrap().insert(k.to_string(), v.to_string());
            }
            me
        }
        fn writes(&self) -> Vec<(String, String)> {
            self.writes.lock().unwrap().clone()
        }
        fn value(&self, key: &str) -> Option<String> {
            self.rows.lock().unwrap().get(key).cloned()
        }
    }

    impl SettingsRepository for MemRepo {
        fn set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push((key.to_string(), value.to_string()));
            self.rows
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
        fn get(&self, key: &str) -> rusqlite::Result<Option<String>> {
            Ok(self.rows.lock().unwrap().get(key).cloned())
        }
        fn get_all(&self) -> rusqlite::Result<HashMap<String, String>> {
            Ok(self.rows.lock().unwrap().clone())
        }
        fn clear(&self) -> rusqlite::Result<()> {
            self.rows.lock().unwrap().clear();
            Ok(())
        }
    }

    /// **游戏模式未提权时不得静默改设置**（本任务的核心反向对照目标）。
    ///
    /// 【为什么这条测试在非 Windows 上依然有意义】它断言的不是"是否提权"，
    /// 而是"启动路径有没有发出对 `app.paste_method` 的写入"。在 Linux 目标上
    /// `check_is_admin()` 走非 Windows 分支（返回 false），恰好等于"未提权的真机"，
    /// 因此这条断言在交叉编译目标上仍然真实覆盖到"未提权"这一侧。
    #[test]
    fn startup_never_silently_rewrites_the_paste_method() {
        let repo = MemRepo::with(&[("app.paste_method", "game_mode")]);
        apply_startup_resets(&repo);

        let writes = repo.writes();
        assert!(
            writes.is_empty(),
            "启动期**不得**写任何设置（旧实现会把 game_mode 改成 shift_insert）：{:?}",
            writes
        );
        assert_eq!(
            repo.value("app.paste_method").as_deref(),
            Some("game_mode"),
            "用户的选择必须原样保留：未提权不构成「应用替用户改设置」的理由"
        );
    }

    /// 已提权时同样不改设置（提权只影响"是否生效"的判定，不影响设置本身）。
    #[test]
    fn startup_leaves_other_methods_untouched_too() {
        for method in ["shift_insert", "ctrl_v", "game_mode"] {
            let repo = MemRepo::with(&[("app.paste_method", method)]);
            apply_startup_resets(&repo);
            assert!(
                repo.writes().is_empty(),
                "settings 写入必须为空（method={}）",
                method
            );
        }
    }

    /// 粘贴方案状态：只有 `game_mode` 是"需要提权"的方案。
    #[test]
    fn paste_method_status_marks_only_game_mode_as_admin_dependent() {
        let game = paste_method_status_from("game_mode", false);
        assert!(game.requires_admin);
        assert!(!game.effective, "未提权时游戏模式不算生效");
        assert_eq!(game.method, "game_mode", "配置原样回传，不做替换");

        let game_admin = paste_method_status_from("game_mode", true);
        assert!(game_admin.effective, "提权后游戏模式生效");
        assert!(game_admin.is_admin);

        for plain in ["shift_insert", "ctrl_v"] {
            let s = paste_method_status_from(plain, false);
            assert!(!s.requires_admin, "{} 不需要提权", plain);
            assert!(s.effective, "{} 在未提权下也应生效", plain);
        }
    }

    /// 唯一真键的常量值：前端 `useSettingsPostInit.ts` 读的是同一个字符串。
    /// 值一旦被改，前后端会立刻分叉成"界面显示的"与"后端触发的"两回事。
    #[test]
    fn win_v_key_constants_are_the_agreed_contract() {
        assert_eq!(WIN_V_SETTING_KEY, "app.use_win_v_shortcut");
        assert_eq!(WIN_V_LEGACY_SETTING_KEY, "app.registry_win_v_enabled");
    }

    /// 旧键迁移：新键缺失时把旧值搬过来，且写完要能回读一致。
    #[test]
    fn legacy_win_v_key_is_migrated_when_the_new_key_is_absent() {
        let repo = MemRepo::with(&[(WIN_V_LEGACY_SETTING_KEY, "true")]);
        migrate_win_v_setting_key_once(&repo);
        assert_eq!(
            repo.value(WIN_V_SETTING_KEY).as_deref(),
            Some("true"),
            "老用户库里的旧键值必须被搬到新键，否则升级后开关会被静默关掉"
        );
    }

    /// 新键已存在 → **绝不覆盖**（用户的最近意愿优先于陈旧残留）。
    ///
    /// 这条如果反了，升级会把用户最近关掉的开关用很久以前的 `true` 重新打开。
    #[test]
    fn existing_new_key_wins_over_the_legacy_key() {
        let repo = MemRepo::with(&[
            (WIN_V_SETTING_KEY, "false"),
            (WIN_V_LEGACY_SETTING_KEY, "true"),
        ]);
        migrate_win_v_setting_key_once(&repo);
        assert_eq!(
            repo.value(WIN_V_SETTING_KEY).as_deref(),
            Some("false"),
            "新键已存在时不得被旧键覆盖（否则用户的最近选择会被回退）"
        );
        assert!(repo.writes().is_empty(), "无需迁移时不应产生任何写入");
    }

    /// 两个键都没有（全新用户）→ 不产生任何写入，也不凭空造一个键。
    #[test]
    fn fresh_install_creates_no_win_v_key() {
        let repo = MemRepo::with(&[]);
        migrate_win_v_setting_key_once(&repo);
        assert!(repo.writes().is_empty());
        assert_eq!(repo.value(WIN_V_SETTING_KEY), None);
    }
}
