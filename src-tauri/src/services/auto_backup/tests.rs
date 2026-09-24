//! 自动容灾备份的**自证测试**。
//!
//! 这些测试要证明的不是"代码看起来对"，而是用户提的每一条约束在**真实文件系统**上成立：
//! 超限删最老未固定、固定的绝不被删、固定上限被明确拒绝、边界（最大=1 / 全固定 / 把份数
//! 调小）行为确定、固定状态**重启后仍在**。
//!
//! # 桩数据为什么用手写文件名而不是真的打包
//!
//! 轮换/固定这些逻辑只依赖文件名与文件是否存在，跟 zip 内容无关。真去打包 60 次要走
//! `VACUUM INTO` + 完整数据目录，一套测试要几十秒。因此：**轮换与固定用桩文件**
//! （[`seed_entries`]），**"生成的备份确实是一份可恢复的真包"另用一个独立测试**
//! （[`created_backup_is_a_real_restorable_package`]）覆盖。两者合起来才是完整的证据链。
//!
//! # 反向对照
//!
//! 本模块里最关键的一条保护是"轮换跳过固定项"。为了让"测试通过"不等于"恰好通过"，
//! [`reverse_control_naive_rotation_would_delete_the_pinned_one`] 用**去掉跳过逻辑的朴素
//! 算法**跑同一份输入，断言它会删掉固定那份——证明上面那条测试确实有区分力。
//! （该反向对照已在实现中实测通过：把 `plan_rotation` 里的 `if entry.pinned { continue; }`
//! 注释掉后，`rotation_never_deletes_a_pinned_backup` 立刻变红。见任务报告。）

use super::*;
use super::store::*;
use super::config::AutoBackupConfig;
use chrono::NaiveDate;
use std::path::{Path, PathBuf};

fn tmp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tiez-autobak-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 造一个最小可用的数据目录：`<root>/com.tieznext/` + 一个真实 schema 的库。
fn data_dir(root: &Path) -> PathBuf {
    let data = root.join("com.tieznext");
    std::fs::create_dir_all(&data).unwrap();
    let db = data.join("clipboard.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
    crate::database::seed_defaults(&conn).unwrap();
    data
}

fn open_store(root: &Path) -> (PathBuf, AutoBackupStore) {
    let data = data_dir(root);
    let store = AutoBackupStore::open_for_data_dir(&data).unwrap();
    let dir = store.dir.clone();
    // 自动备份必须落在数据目录**之外**——这是本模块存在的意义，顺手每次都断言。
    assert!(
        !dir.starts_with(&data),
        "自动备份目录 {:?} 不得位于数据目录 {:?} 内",
        dir,
        data
    );
    (data, store)
}

/// 在备份目录里放一个桩备份文件（内容任意，逻辑只关心名字与存在性）。
fn touch(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// 按 `(来源, 秒, 序号, 是否固定)` 造一批桩备份。
fn seed_entries(dir: &Path, specs: &[(BackupOrigin, &str, u32, bool)]) -> Vec<String> {
    let mut names = Vec::new();
    for (origin, hms, seq, pinned) in specs {
        let stamp = NaiveDate::from_ymd_opt(2026, 5, 1)
            .unwrap()
            .and_hms_opt(
                hms[0..2].parse().unwrap(),
                hms[2..4].parse().unwrap(),
                hms[4..6].parse().unwrap(),
            )
            .unwrap();
        let pin = if *pinned { "-p" } else { "" };
        let name = format!(
            "{}{}-{}-{:02}{}{}",
            NAME_PREFIX,
            origin.tag(),
            stamp.format("%Y%m%dT%H%M%S"),
            seq,
            pin,
            NAME_EXT
        );
        touch(dir, &name, b"stub");
        names.push(name);
    }
    names
}

/// 列表里还剩哪些名字。
fn names_in(store: &AutoBackupStore) -> Vec<String> {
    store
        .list()
        .unwrap()
        .into_iter()
        .map(|e| e.archive_name)
        .collect()
}

// ===========================================================================
// 1. 轮换：超限删最老的**未固定**备份
// ===========================================================================

#[test]
fn rotation_deletes_the_oldest_unpinned_ones() {
    let root = tmp_root("rotate-oldest");
    let (_data, mut store) = open_store(&root);

    // 5 份，全部未固定，时间递增。
    seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, false),
            (BackupOrigin::Scheduled, "020000", 1, false),
            (BackupOrigin::Scheduled, "030000", 1, false),
            (BackupOrigin::Scheduled, "040000", 1, false),
            (BackupOrigin::Scheduled, "050000", 1, false),
        ],
    );

    let outcome = store.enforce_rotation(3).unwrap();

    assert_eq!(outcome.deleted.len(), 2, "5 份留 3 份，应删 2 份");
    assert!(
        outcome.deleted[0].contains("20260501T010000"),
        "必须先删最老的（实际删了 {:?}）",
        outcome.deleted
    );
    assert!(outcome.deleted[1].contains("20260501T020000"));
    assert!(!outcome.has_undelatable_excess());

    let left = names_in(&store);
    assert_eq!(left.len(), 3);
    assert!(left.iter().any(|n| n.contains("T030000")));
    assert!(left.iter().any(|n| n.contains("T050000")));
}

// ===========================================================================
// 2. 固定的一律不删（核心保护）
// ===========================================================================

#[test]
fn rotation_never_deletes_a_pinned_backup() {
    let root = tmp_root("keep-pinned");
    let (_data, mut store) = open_store(&root);

    // 最老的那份被固定住。朴素实现会先删它。
    let names = seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, true), // 最老，但固定
            (BackupOrigin::Scheduled, "020000", 1, false),
            (BackupOrigin::Scheduled, "030000", 1, false),
            (BackupOrigin::Scheduled, "040000", 1, false),
        ],
    );

    let outcome = store.enforce_rotation(2).unwrap();

    assert!(
        !outcome.deleted.contains(&names[0]),
        "被固定的最老备份绝不能被轮换删除（实际删除 {:?}）",
        outcome.deleted
    );
    assert!(
        std::path::Path::new(&store.dir.join(&names[0])).is_file(),
        "固定那份文件必须还在磁盘上"
    );
    // 超出的两份从**次老**开始删。
    assert_eq!(outcome.deleted.len(), 2);
    assert!(outcome.deleted[0].contains("T020000"));
    assert!(outcome.deleted[1].contains("T030000"));
    assert_eq!(names_in(&store).len(), 2);
}

/// **反向对照**：证明上面那条测试确实有区分力。
///
/// 这里用一个"不跳过固定项"的朴素轮换算法跑同一份输入。若它也保住固定那份，说明输入
/// 本身就没有区分力（测试是恰好通过）；它必须删掉固定那份，才能证明
/// `rotation_never_deletes_a_pinned_backup` 真的在测"跳过固定"这件事。
#[test]
fn reverse_control_naive_rotation_would_delete_the_pinned_one() {
    let root = tmp_root("reverse-control");
    let (_data, store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, true),
            (BackupOrigin::Scheduled, "020000", 1, false),
            (BackupOrigin::Scheduled, "030000", 1, false),
            (BackupOrigin::Scheduled, "040000", 1, false),
        ],
    );
    let entries = store.list().unwrap();

    // 朴素算法：按时间从新到旧，超出的直接从最老开始删，不看 pinned。
    let mut sorted = entries.clone();
    sorted.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then(b.seq.cmp(&a.seq))
            .then(b.archive_name.cmp(&a.archive_name))
    });
    let excess = sorted.len() - 2;
    let naive_doomed: Vec<String> = sorted
        .iter()
        .rev()
        .take(excess)
        .map(|e| e.archive_name.clone())
        .collect();

    assert!(
        naive_doomed.contains(&names[0]),
        "反向对照失败：朴素算法竟然也保住了固定项，说明本组测试输入没有区分力（实际 {:?}）",
        naive_doomed
    );

    // 同一份输入交给真实实现，结果必须与朴素算法**不同**。
    let (real_doomed, _) = plan_rotation(&entries, 2);
    let real: Vec<String> = real_doomed.iter().map(|e| e.archive_name.clone()).collect();
    assert_ne!(
        real, naive_doomed,
        "真实实现必须与朴素算法给出不同结果，否则'跳过固定'的逻辑没起作用"
    );
    assert!(!real.contains(&names[0]));
}

/// `plan_rotation` 是纯函数：不碰文件系统，只按输入算。
#[test]
fn plan_rotation_is_pure_and_deterministic() {
    let root = tmp_root("pure-plan");
    let (_data, store) = open_store(&root);
    seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, true),
            (BackupOrigin::Startup, "020000", 1, true),
            (BackupOrigin::Scheduled, "030000", 1, false),
            (BackupOrigin::Scheduled, "040000", 1, false),
            (BackupOrigin::Scheduled, "050000", 1, false),
        ],
    );
    let entries = store.list().unwrap();

    // 上限 2，5 份（3 个未固定）→ 需要删 3 份，未固定的刚好够。
    let (doomed, deficit) = plan_rotation(&entries, 2);
    assert_eq!(doomed.len(), 3);
    assert_eq!(deficit, 0);
    assert!(doomed.iter().all(|e| !e.pinned), "计划里不得出现固定项");

    // 调用两次结果一致（顺序确定，不看文件系统枚举顺序）。
    let (again, _) = plan_rotation(&entries, 2);
    assert_eq!(
        doomed.iter().map(|e| &e.archive_name).collect::<Vec<_>>(),
        again.iter().map(|e| &e.archive_name).collect::<Vec<_>>()
    );
}

// ===========================================================================
// 3. A11：固定上限 = 最大留存 − 1，超上限返回**可区分**的原因
// ===========================================================================

#[test]
fn pinning_beyond_the_limit_is_rejected_with_a_specific_reason() {
    let root = tmp_root("pin-limit");
    let (_data, mut store) = open_store(&root);

    // 用户原话的场景：最大留存 50，已有 49 个固定，试图固定第 50 个。
    let specs: Vec<(BackupOrigin, String, u32, bool)> = (0..50)
        .map(|i| {
            (
                BackupOrigin::Scheduled,
                format!("{:02}{:02}00", i / 60, i % 60),
                i as u32 + 1,
                false,
            )
        })
        .collect();
    let refs: Vec<(BackupOrigin, &str, u32, bool)> = specs
        .iter()
        .map(|(o, s, q, p)| (*o, s.as_str(), *q, *p))
        .collect();
    let names = seed_entries(&store.dir, &refs);

    let cfg = AutoBackupConfig {
        max_keep: 50,
        ..AutoBackupConfig::default()
    };
    assert_eq!(cfg.max_pinned(), 49);

    // 固定前 49 个：全部成功。
    for name in names.iter().take(49) {
        assert!(store.set_pinned(name, true, &cfg).unwrap());
    }
    let err = store.set_pinned(&names[49], true, &cfg).unwrap_err();

    // 必须是**特定的**原因，不是泛泛的失败。
    assert_eq!(err.code(), "auto_backup_pinned_limit_reached");
    let payload = err.payload();
    assert_eq!(payload["maxKeep"], 50);
    assert_eq!(payload["maxPinned"], 49);
    assert_eq!(payload["currentPinned"], 49);

    // 文案就是用户要求的那段提示（含三个数）。
    let text = err.to_string();
    assert!(text.contains("50"), "提示里必须写明当前最大留存量：{}", text);
    assert!(text.contains("49"), "提示里必须写明已固定数：{}", text);
    assert!(
        text.contains("轮换"),
        "提示里必须说明为什么不能全固定：{}",
        text
    );

    // 被拒之后状态没变：第 50 个仍未固定。
    let entries = store.list().unwrap();
    let target = entries
        .iter()
        .find(|e| e.archive_name == names[49])
        .unwrap();
    assert!(!target.pinned, "被拒绝的操作不能产生副作用");
    assert_eq!(entries.iter().filter(|e| e.pinned).count(), 49);
}

/// 上限放行到正好 `max_keep - 1`，再多一个都不行（不多不少）。
#[test]
fn the_limit_is_exact_not_approximate() {
    let root = tmp_root("pin-exact");
    let (_data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, false),
            (BackupOrigin::Scheduled, "020000", 1, false),
            (BackupOrigin::Scheduled, "030000", 1, false),
        ],
    );
    let cfg = AutoBackupConfig {
        max_keep: 3,
        ..AutoBackupConfig::default()
    };
    assert_eq!(cfg.max_pinned(), 2);
    assert!(store.set_pinned(&names[0], true, &cfg).is_ok());
    assert!(store.set_pinned(&names[1], true, &cfg).is_ok());
    assert_eq!(
        store.set_pinned(&names[2], true, &cfg).unwrap_err().code(),
        "auto_backup_pinned_limit_reached"
    );
}

/// 取消固定后位置释放：能再固定别人（上限不该是"一次性"的）。
#[test]
fn unpinning_frees_a_slot() {
    let root = tmp_root("pin-free");
    let (_data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, false),
            (BackupOrigin::Scheduled, "020000", 1, false),
        ],
    );
    let cfg = AutoBackupConfig {
        max_keep: 2,
        ..AutoBackupConfig::default()
    };
    assert!(store.set_pinned(&names[0], true, &cfg).is_ok());
    assert_eq!(
        store.set_pinned(&names[1], true, &cfg).unwrap_err().code(),
        "auto_backup_pinned_limit_reached"
    );
    // 【接口契约】固定会改文件名（尾部加 `-p`），因此取消固定必须用 `list()` 返回的
    // **当前**名字，而不是固定之前那个。界面按列表里的名字调用，天然满足这一点。
    let renamed = names_in(&store)
        .into_iter()
        .find(|n| n.contains("T010000"))
        .unwrap();
    assert!(renamed.contains("-p"));
    assert!(!store.set_pinned(&renamed, false, &cfg).unwrap());
    assert!(
        store.set_pinned(&names[1], true, &cfg).is_ok(),
        "取消固定之后位置必须能被别人用"
    );
}

// ===========================================================================
// 4. 边界情况
// ===========================================================================

/// **最大留存 = 1**：按公式能固定 0 个，且第 1 个就被明确拒绝（不是静默无效）。
#[test]
fn max_keep_one_allows_zero_pinned_and_says_so() {
    let root = tmp_root("max-one");
    let (_data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[(BackupOrigin::Scheduled, "010000", 1, false)],
    );
    let cfg = AutoBackupConfig {
        max_keep: 1,
        ..AutoBackupConfig::default()
    };
    assert_eq!(cfg.max_pinned(), 0, "只剩一个位置时必须全部留给轮换");

    let err = store.set_pinned(&names[0], true, &cfg).unwrap_err();
    assert_eq!(err.code(), "auto_backup_pinned_limit_reached");
    assert_eq!(err.payload()["maxPinned"], 0);

    // 上限 1 时轮换仍然正常：新增一份会把最老那份顶掉。
    seed_entries(
        &store.dir,
        &[(BackupOrigin::Scheduled, "020000", 1, false)],
    );
    let outcome = store.enforce_rotation(1).unwrap();
    assert_eq!(outcome.deleted.len(), 1);
    assert!(outcome.deleted[0].contains("T010000"));
}

/// **全部备份都被固定**：轮换删不动，必须如实报告而不是静默吞掉。
///
/// 正常路径到不了这里（A11 卡住了固定数），但它必须被定义：用户可以手工往目录里放文件、
/// 也可以把 `max_keep` 调小，两种情况下"全是固定项"都真实可达。
#[test]
fn rotation_reports_undelatable_excess_when_everything_is_pinned() {
    let root = tmp_root("all-pinned");
    let (_data, mut store) = open_store(&root);
    seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, true),
            (BackupOrigin::Scheduled, "020000", 1, true),
            (BackupOrigin::Scheduled, "030000", 1, true),
        ],
    );

    let outcome = store.enforce_rotation(1).unwrap();

    assert!(outcome.deleted.is_empty(), "固定项一个都不能删");
    assert_eq!(outcome.undelatable_excess, 2, "多出来的 2 份应被如实报出");
    assert_eq!(names_in(&store).len(), 3, "文件必须都还在");
    assert!(
        outcome.warnings.iter().any(|w| w.contains("固定")),
        "必须给出可读的解释，而不是静默超限：{:?}",
        outcome.warnings
    );
}

/// **把最大留存从 200 调小到 10（当前有 50 份）**：真实边界，行为必须确定。
///
/// 定义：调小**立即生效**，但只在"下一次轮换"（下一次生成备份时）执行删除，且**永不删
/// 固定项**。本测试同时验证两件事——手动调用轮换会删到 10；以及固定项在这些删除中被跳过。
#[test]
fn shrinking_max_keep_deletes_the_oldest_unpinned_only() {
    let root = tmp_root("shrink");
    let (_data, mut store) = open_store(&root);

    // 50 份：最老的 3 份固定住。
    let specs: Vec<(BackupOrigin, String, u32, bool)> = (0..50)
        .map(|i| {
            (
                BackupOrigin::Scheduled,
                format!("{:02}{:02}00", i / 60, i % 60),
                i as u32 + 1,
                i < 3,
            )
        })
        .collect();
    let refs: Vec<(BackupOrigin, &str, u32, bool)> = specs
        .iter()
        .map(|(o, s, q, p)| (*o, s.as_str(), *q, *p))
        .collect();
    let names = seed_entries(&store.dir, &refs);

    let cfg = AutoBackupConfig {
        max_keep: 200,
        ..AutoBackupConfig::default()
    };
    assert_eq!(store.list().unwrap().len(), 50, "上限 200 时一份都不该删");
    assert!(store.enforce_rotation(cfg.max_keep).unwrap().deleted.is_empty());

    // 用户把上限改成 10。
    let shrunk = AutoBackupConfig {
        max_keep: 10,
        ..cfg
    };
    let outcome = store.enforce_rotation(shrunk.max_keep).unwrap();

    assert_eq!(outcome.deleted.len(), 40, "50 → 10，应删 40 份");
    let left = names_in(&store);
    assert_eq!(left.len(), 10);
    for pinned in names.iter().take(3) {
        assert!(
            left.contains(pinned),
            "被固定的 {:?} 在把上限调小后也必须留下（剩余 {:?}）",
            pinned,
            left
        );
    }
    // 留下的是"3 个固定的 + 7 个最新的"。
    assert!(
        left.iter().any(|n| n.contains("T004900")),
        "最新那份必须留下：{:?}",
        left
    );
    assert!(
        !left.iter().any(|n| n.contains("T000300")),
        "最老的未固定项应被删：{:?}",
        left
    );
    assert!(!outcome.has_undelatable_excess(), "固定的只占 3 个名额，删得下");
}

/// 上限调小到比"已固定数"还小的极端：删除无法完成，必须如实报告（不是静默超限）。
#[test]
fn shrinking_below_the_pinned_count_is_reported_not_silently_ignored() {
    let root = tmp_root("shrink-below");
    let (_data, mut store) = open_store(&root);
    seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, true),
            (BackupOrigin::Scheduled, "020000", 1, true),
            (BackupOrigin::Scheduled, "030000", 1, false),
            (BackupOrigin::Scheduled, "040000", 1, false),
        ],
    );
    // 用户把上限从 4 改成 1（固定数 2 > 上限 1）。真实可达：手工改库或从旧配置继承。
    let outcome = store.enforce_rotation(1).unwrap();

    assert_eq!(outcome.deleted.len(), 2, "两份未固定的应被删");
    assert_eq!(names_in(&store).len(), 2, "两份固定的必须留下");
    assert_eq!(
        outcome.undelatable_excess, 1,
        "总数 2 仍超过上限 1，这 1 份删不掉必须如实报出"
    );
}

/// 目录里**不是本模块命名**的文件：不列表、不轮换、不删除。
///
/// 轮换会删文件，所以"看不懂就跳过"是这类代码里最便宜也最必要的保险。
#[test]
fn foreign_files_are_never_listed_or_deleted() {
    let root = tmp_root("foreign");
    let (_data, mut store) = open_store(&root);
    seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, false),
            (BackupOrigin::Scheduled, "020000", 1, false),
        ],
    );
    let keep_me = touch(&store.dir, "用户的说明.txt", b"do not delete");
    let also_keep = touch(&store.dir, "Tiez-Next-backup-0.5.0-20260101-000000.zip", b"manual");

    let listed = names_in(&store);
    assert_eq!(listed.len(), 2, "只应列出本模块管理的备份");
    assert!(!listed.contains(&"用户的说明.txt".to_string()));

    // 上限压到 1，强制轮换尽量删。
    let outcome = store.enforce_rotation(1).unwrap();
    assert_eq!(outcome.deleted.len(), 1);
    assert!(keep_me.is_file(), "外来文件不得被删");
    assert!(also_keep.is_file(), "同名规则的**手动**备份也不得被自动轮换删掉");
}

// ===========================================================================
// 5. 命名与元数据：固定状态**重启后仍能读到**
// ===========================================================================

/// 关闭再打开（= 重启应用）后，固定状态必须还在。
///
/// 这是最容易错的一条：若固定状态只存在内存里，重启就丢，用户固定过的备份会被下一次
/// 轮换删掉。本测试**真的重新构造一个 store**（等价于新进程），而不是复用原对象。
#[test]
fn pinned_state_survives_a_restart() {
    let root = tmp_root("restart");
    let (data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, false),
            (BackupOrigin::Scheduled, "020000", 1, false),
            (BackupOrigin::Scheduled, "030000", 1, false),
        ],
    );
    let cfg = AutoBackupConfig {
        max_keep: 3,
        ..AutoBackupConfig::default()
    };
    store.set_pinned(&names[0], true, &cfg).unwrap();
    let pinned_after_write = names_in(&store);
    let pinned_file_name = pinned_after_write
        .iter()
        .find(|n| n.contains("T010000"))
        .unwrap()
        .clone();
    assert!(pinned_file_name.contains("-p"), "文件名里应带固定标记：{}", pinned_file_name);

    // ===== 等价于"重启应用"：丢掉整个 store，从零构造 =====
    drop(store);
    let reopened = AutoBackupStore::open_for_data_dir(&data).unwrap();

    let entries = reopened.list().unwrap();
    let target = entries
        .iter()
        .find(|e| e.archive_name == pinned_file_name)
        .expect("固定那份必须还在列表里");
    assert!(target.pinned, "重启后固定状态必须仍然是已固定");

    // 并且它真的不会被下一次轮换删掉。
    let mut reopened = reopened;
    seed_entries(
        &reopened.dir,
        &[(BackupOrigin::Scheduled, "040000", 1, false)],
    );
    let outcome = reopened.enforce_rotation(3).unwrap();
    assert!(
        !outcome.deleted.contains(&pinned_file_name),
        "重启后第一次轮换就把固定项删了（删了 {:?}）",
        outcome.deleted
    );
    assert!(reopened.dir.join(&pinned_file_name).is_file());
}

/// 索引文件损坏/丢失时，**文件名里的标记**仍能恢复固定状态（两份冗余的意义）。
#[test]
fn pin_state_falls_back_to_the_file_name_when_the_index_is_gone() {
    let root = tmp_root("index-lost");
    let (data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[(BackupOrigin::Scheduled, "010000", 1, false)],
    );
    let cfg = AutoBackupConfig {
        max_keep: 5,
        ..AutoBackupConfig::default()
    };
    store.set_pinned(&names[0], true, &cfg).unwrap();
    let pinned_file = names_in(&store).pop().unwrap();
    assert!(store.dir.join(PIN_INDEX_NAME).is_file(), "索引应已落盘");

    // 模拟索引丢失（用户清理目录、复制时漏了这个文件）。
    std::fs::remove_file(store.dir.join(PIN_INDEX_NAME)).unwrap();
    drop(store);

    let reopened = AutoBackupStore::open_for_data_dir(&data).unwrap();
    let entries = reopened.list().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].pinned,
        "索引丢了也必须从文件名恢复固定状态——否则用户的固定会被静默取消"
    );
    assert_eq!(entries[0].archive_name, pinned_file);

    // 索引内容坏掉时同样：退回按文件名，并**如实告知**（不静默）。
    let (data2, store2) = open_store(&root.join("corrupt"));
    seed_entries(
        &store2.dir,
        &[(BackupOrigin::Scheduled, "010000", 1, true)],
    );
    std::fs::write(store2.dir.join(PIN_INDEX_NAME), b"{ this is not json").unwrap();
    let dir2 = store2.dir.clone();
    drop(store2);

    let reopened2 = AutoBackupStore::open_for_data_dir(&data2).unwrap();
    assert!(reopened2.list().unwrap()[0].pinned);
    assert!(
        !reopened2.load_warnings.is_empty(),
        "索引损坏必须留下可读的提示（目录 {:?}）",        dir2
    );
}

/// **口径一致性**：`list()` 与 `set_pinned()` 对"是否已固定"必须给出同一个答案。
///
/// 这条是被一个真实缺陷逼出来的：`list()` 用「文件名标记 ∪ 索引」判定，而 `set_pinned()`
/// 原先只看索引。于是一个"名字带 `-p`、索引里没有"的备份（用户手工把固定过的备份拷进来，
/// 或索引被删过）在列表里显示已固定，点"固定"却会去生成**同名**文件、撞上重名守卫而报错
/// ——用户的动作本应是幂等的空操作，却失败了。
#[test]
fn list_and_set_pinned_agree_on_the_pinned_state() {
    let root = tmp_root("agree");
    let (data, mut store) = open_store(&root);
    // 桩文件名字里带 `-p`，但**不写索引**（模拟手工拷入 / 索引丢失）。
    let names = seed_entries(
        &store.dir,
        &[(BackupOrigin::Scheduled, "010000", 1, true)],
    );
    assert!(
        !store.dir.join(PIN_INDEX_NAME).exists(),
        "前提：此刻没有索引文件"
    );
    let cfg = AutoBackupConfig {
        max_keep: 5,
        ..AutoBackupConfig::default()
    };

    // list 说它是已固定。
    assert!(store.list().unwrap()[0].pinned);

    // 于是"再固定一次"必须是幂等的成功，而不是重名报错。
    assert!(
        store.set_pinned(&names[0], true, &cfg).is_ok(),
        "已固定的再点固定应当是幂等成功"
    );
    // 并且顺手把索引补齐（自愈），下次启动不用再靠文件名兜底。
    let idx = std::fs::read_to_string(store.dir.join(PIN_INDEX_NAME)).unwrap();
    assert!(idx.contains(&names[0]), "幂等路径应把索引补齐：{}", idx);

    // 取消固定后再看：两边都必须说"未固定"。
    store.set_pinned(&names[0], false, &cfg).unwrap();
    assert!(!store.list().unwrap()[0].pinned);
    drop(store);
    let reopened = AutoBackupStore::open_for_data_dir(&data).unwrap();
    assert!(!reopened.list().unwrap()[0].pinned);
}

/// 取消固定要**两处一起**去掉：文件名标记与索引都不能残留。
#[test]
fn unpinning_clears_both_the_name_token_and_the_index() {
    let root = tmp_root("unpin-both");
    let (data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[(BackupOrigin::Scheduled, "010000", 1, false)],
    );
    let cfg = AutoBackupConfig {
        max_keep: 5,
        ..AutoBackupConfig::default()
    };
    store.set_pinned(&names[0], true, &cfg).unwrap();
    let pinned_file = names_in(&store).pop().unwrap();

    store.set_pinned(&pinned_file, false, &cfg).unwrap();
    let rest = names_in(&store);
    assert_eq!(rest.len(), 1);
    assert!(
        !rest[0].contains("-p"),
        "取消固定后文件名里不该还有标记：{}",
        rest[0]
    );

    let idx = std::fs::read_to_string(store.dir.join(PIN_INDEX_NAME)).unwrap();
    assert!(
        !idx.contains(&rest[0]),
        "取消固定后索引里不该再列出它：{}",
        idx
    );

    drop(store);
    let reopened = AutoBackupStore::open_for_data_dir(&data).unwrap();
    assert!(!reopened.list().unwrap()[0].pinned, "重启后也应保持未固定");
}

/// 命名规则本身：能解析出来源、秒级时间、序号、固定标记；别的名字一律不认。
#[test]
fn name_round_trips_origin_stamp_seq_and_pin_token() {
    let parsed = parse_name("Tiez-Next-auto-timed-20260501T013045-02.zip").unwrap();
    assert_eq!(parsed.origin, BackupOrigin::Scheduled);
    assert_eq!(parsed.stamp.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-05-01 01:30:45");
    assert_eq!(parsed.seq, 2);
    assert!(!parsed.pinned_token);

    let pinned = parse_name("Tiez-Next-auto-startup-20260501T013045-01-p.zip").unwrap();
    assert_eq!(pinned.origin, BackupOrigin::Startup);
    assert!(pinned.pinned_token, "`-p` 必须被认成固定标记");

    // 时间精确到秒：同一分钟的不同秒必须解析成不同时刻。
    let a = parse_name("Tiez-Next-auto-timed-20260501T013045-01.zip").unwrap();
    let b = parse_name("Tiez-Next-auto-timed-20260501T013046-01.zip").unwrap();
    assert_eq!((b.stamp - a.stamp).num_seconds(), 1);

    for foreign in [
        "Tiez-Next-backup-0.5.0-20260101-000000.zip", // 手动导出
        "clipboard.db",
        "Tiez-Next-auto-timed-2026-05-01T01:30:45-01.zip", // 分隔符不对
        "Tiez-Next-auto-unknown-20260501T013045-01.zip",  // 来源不认识
        "Tiez-Next-auto-timed-20260501T013045-01-x.zip",  // 尾巴不认识
    ] {
        assert!(parse_name(foreign).is_none(), "不该被认成自动备份：{}", foreign);
    }
}

/// 前端传来的名字是**不可信输入**：路径穿越与非法名必须被挡在文件操作之前。
#[test]
fn archive_name_is_validated_before_touching_the_filesystem() {
    for bad in [
        "../../clipboard.db",
        "..\\..\\clipboard.db",
        "sub/Tiez-Next-auto-timed-20260501T013045-01.zip",
        "C:Tiez-Next-auto-timed-20260501T013045-01.zip",
        "",
        "..",
        "Tiez-Next-auto-timed-20260501T013045-01.zip.bak",
    ] {
        let err = validate_archive_name(bad).unwrap_err();
        assert!(
            matches!(
                err.code(),
                "auto_backup_invalid_name" | "auto_backup_not_found"
            ),
            "非法名 {:?} 应被拒绝，实际 {:?}",
            bad,
            err.code()
        );
    }
    assert!(validate_archive_name("Tiez-Next-auto-timed-20260501T013045-01.zip").is_ok());
}

// ===========================================================================
// 6. 真实打包：生成的确实是一份可被既有导入链识别的包
// ===========================================================================

/// 用**真的** `create_backup` 生成一份，断言它是合法 zip 且带本应用的 manifest。
///
/// 与上面那些桩文件测试互补：桩文件证明轮换/固定逻辑，本测试证明"自动备份确实复用既有
/// 导出链、产出的是可恢复的真包"。
#[test]
fn created_backup_is_a_real_restorable_package() {
    let root = tmp_root("real-package");
    let (data, mut store) = open_store(&root);

    let entry = store
        .create(&data, BackupOrigin::Scheduled, "0.5.0")
        .unwrap();

    assert!(entry.archive_name.starts_with(NAME_PREFIX));
    assert!(entry.archive_name.contains("-timed-"));
    assert!(entry.size_bytes > 0, "备份不应是空文件");
    assert!(!entry.pinned, "新备份默认未固定");
    assert_eq!(entry.origin, "scheduled");
    // 时间精确到秒：字符串里必须带完整的秒。
    assert_eq!(entry.created_at_local.len(), "2026-05-01 01:30:45".len());

    // 包内必须有本应用的 manifest（复用既有导出链的直接证据）。
    let file = std::fs::File::open(&entry.path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    let mut manifest = String::new();
    {
        use std::io::Read;
        let mut e = zip.by_name("manifest.json").unwrap();
        e.read_to_string(&mut manifest).unwrap();
    }
    assert!(manifest.contains("com.tieznext.backup"), "manifest: {}", manifest);

    // 列表里的秒级时间与文件名的可解析性。
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].archive_name, entry.archive_name);
    assert_eq!(listed[0].size_bytes, entry.size_bytes);
}

/// 同一秒内连续备份不会互相覆盖：得到两份而不是一份。
#[test]
fn two_backups_in_the_same_second_do_not_overwrite_each_other() {
    let root = tmp_root("same-second");
    let (data, mut store) = open_store(&root);
    let stamp = chrono::NaiveDate::from_ymd_opt(2026, 5, 1)
        .unwrap()
        .and_hms_opt(1, 2, 3)
        .unwrap();

    let a = store
        .create_at(&data, BackupOrigin::Manual, "0.5.0", stamp)
        .unwrap();
    let b = store
        .create_at(&data, BackupOrigin::Manual, "0.5.0", stamp)
        .unwrap();

    assert_ne!(a.archive_name, b.archive_name);
    assert_eq!(store.list().unwrap().len(), 2, "两份都必须留下");
}

/// 数据目录不存在时明确失败，而不是产出一个空包。
#[test]
fn missing_data_dir_fails_loudly() {
    let root = tmp_root("missing-data");
    let (_data, mut store) = open_store(&root);
    let err = store
        .create(&root.join("nope"), BackupOrigin::Scheduled, "0.5.0")
        .unwrap_err();
    // 前置条件（数据目录不存在）在碰导出链之前就被判掉，因此是 io 码而不是导出失败码。
    assert_eq!(err.code(), "auto_backup_io");
    assert!(err.to_string().contains("数据目录不存在"));
    assert!(store.list().unwrap().is_empty(), "失败不该留下半成品");
}

// ===========================================================================
// 7. 目录位置
// ===========================================================================

/// 备份目录与数据目录**同级**（兄弟关系），而不是数据目录内部或系统别的盘。
#[test]
fn backup_dir_is_a_sibling_of_the_data_dir() {
    let root = tmp_root("dir-layout");
    let data = data_dir(&root);
    let dir = auto_backup_dir(&data);

    assert_eq!(
        dir,
        root.join(AUTO_DIR_SIBLING_NAME).join(AUTO_DIR_LEAF_NAME)
    );
    assert!(!dir.starts_with(&data), "不得落在数据目录内");
    assert_eq!(dir.parent().unwrap().parent().unwrap(), root);

    // 用户改了数据目录，自动备份跟着走（不会留在老位置）。
    let other_root = root.join("d-drive");
    std::fs::create_dir_all(&other_root).unwrap();
    let other_data = data_dir(&other_root);
    assert!(auto_backup_dir(&other_data).starts_with(&other_root));
}

/// 显式构造一个"落在数据目录内"的自动备份目录时必须被拒绝，而不是先建目录再报错。
#[test]
fn store_refuses_a_dir_inside_the_data_dir() {
    let root = tmp_root("inside");
    let data = data_dir(&root);
    let inside = data.join("auto_backups");

    let err = AutoBackupStore::open(inside.clone(), &data).unwrap_err();
    assert_eq!(err.code(), "auto_backup_dir_inside_data_dir");
    assert!(
        !inside.exists(),
        "拒绝之后不该在数据目录里留下一个空目录"
    );
}

// ===========================================================================
// 8. 删除
// ===========================================================================

/// 删除：文件真的没了，且固定项也能被用户显式删除（界面负责二次确认）。
#[test]
fn delete_removes_the_file_and_can_delete_a_pinned_one() {
    let root = tmp_root("delete");
    let (data, mut store) = open_store(&root);
    let names = seed_entries(
        &store.dir,
        &[
            (BackupOrigin::Scheduled, "010000", 1, true),
            (BackupOrigin::Scheduled, "020000", 1, false),
        ],
    );
    let cfg = AutoBackupConfig {
        max_keep: 5,
        ..AutoBackupConfig::default()
    };
    let pinned_name = names_in(&store)
        .into_iter()
        .find(|n| n.contains("T010000"))
        .unwrap();
    assert!(
        pinned_name.contains("-p"),
        "桩文件本来就带固定标记，应被读成已固定：{}",
        pinned_name
    );
    assert!(store.list().unwrap().iter().any(|e| e.archive_name == pinned_name && e.pinned));
    // 上限内重复固定是幂等的（不报错、不产生第二份）。
    assert!(store.set_pinned(&pinned_name, true, &cfg).unwrap());
    assert_eq!(store.list().unwrap().len(), 2);

    store.delete(&names[1]).unwrap();
    assert!(!store.dir.join(&names[1]).exists());
    assert!(store.delete(&names[1]).is_err(), "删不存在的必须报错而不是静默成功");

    store.delete(&pinned_name).unwrap();
    assert!(!store.dir.join(&pinned_name).exists());
    drop(store);

    // 重启后删除状态保持（files 与索引一致）。
    let reopened = AutoBackupStore::open_for_data_dir(&data).unwrap();
    assert!(reopened.list().unwrap().is_empty());
    assert!(reopened.pinned_names().is_empty(), "索引里不该残留已删除的项");
}

/// 全部边界算一遍的汇总不变量：**任何时刻，固定项的数量不会超过 max_keep − 1**。
///
/// 这条不变量是 A11 的最终形态——单点校验都过了，但"连续操作之后是否仍然成立"是另一个
/// 问题（例如取消固定时算错、索引与内存不一致）。
#[test]
fn pinned_count_never_exceeds_the_limit_across_a_long_sequence() {
    let root = tmp_root("invariant");
    let (_data, mut store) = open_store(&root);
    let max_keep = 6u32;
    let cfg = AutoBackupConfig {
        max_keep,
        ..AutoBackupConfig::default()
    };

    // 造 20 份。
    let specs: Vec<(BackupOrigin, String, u32, bool)> = (0..20)
        .map(|i| {
            (
                BackupOrigin::Scheduled,
                format!("{:02}{:02}00", i / 60, i % 60),
                i as u32 + 1,
                false,
            )
        })
        .collect();
    let refs: Vec<(BackupOrigin, &str, u32, bool)> = specs
        .iter()
        .map(|(o, s, q, p)| (*o, s.as_str(), *q, *p))
        .collect();
    seed_entries(&store.dir, &refs);

    // 轮换到上限。
    store.enforce_rotation(max_keep).unwrap();
    assert_eq!(store.list().unwrap().len(), 6);

    // 逐个尝试固定全部 6 份：只能成功 5 次。
    let all: Vec<String> = names_in(&store);
    let mut ok = 0;
    for name in &all {
        if store.set_pinned(name, true, &cfg).is_ok() {
            ok += 1;
        }
        let pinned = store.list().unwrap().iter().filter(|e| e.pinned).count() as u32;
        assert!(
            pinned <= max_keep - 1,
            "不变量被破坏：已固定 {} 份，上限 {}",
            pinned,
            cfg.max_pinned()
        );
    }
    assert_eq!(ok, 5, "6 份里最多只能固定 5 份");
}

// ===========================================================================
// 待生效恢复所用的备份包，不能被轮换删掉
// ===========================================================================
//
// 【这组测试防的是一个真实的数据风险，不是记账】
//
// 恢复是两阶段的：点「恢复」时只做"组装暂存 + 写待接管标记"，真正的数据替换发生在
// **下次启动**。这中间可能隔很久（用户可能过几天才重启），而轮换会在窗口里继续跑。
//
// 于是会出现：用户点恢复 → 没重启 → 后台触发一次备份 → 轮换把那份包当作"最老的、
// 未固定的"删掉 ⇒ 用户**既没有可重来的包**（连"再点一次恢复"都做不到），
// 而且万一提升失败就**没有任何退路**。
//
// 下面第一条断言就是"用户此刻最需要的那份包被保留了"；第二条是它的反向对照。

mod pending_restore_protection {
    use super::*;

    /// 基本情形：受保护的那份是最老的、本该第一个被删 —— 它必须活下来，
    /// 而**其余**该删的仍然要删（否则就成了"保护一个 = 停止整个轮换"）。
    #[test]
    fn protected_backup_survives_rotation_while_others_are_still_deleted() {
        let root = tmp_root("protect-basic");
        let (_data, mut store) = open_store(&root);
        seed_entries(
            &store.dir,
            &[
                (BackupOrigin::Scheduled, "010000", 1, false),
                (BackupOrigin::Scheduled, "020000", 1, false),
                (BackupOrigin::Scheduled, "030000", 1, false),
                (BackupOrigin::Scheduled, "040000", 1, false),
                (BackupOrigin::Scheduled, "050000", 1, false),
            ],
        );

        // 最老的那份（01:00）正是"用户点了恢复要回到的时刻"，且它**没有**被固定。
        let protected = vec![
            store
                .list()
                .unwrap()
                .iter()
                .find(|e| e.archive_name.contains("20260501T010000"))
                .expect("种子里应有 01:00 那份")
                .archive_name
                .clone(),
        ];

        let outcome = store
            .enforce_rotation_protecting(3, &protected)
            .expect("轮换应当成功");

        assert!(
            !outcome.deleted.iter().any(|n| n == &protected[0]),
            "正被一次待生效的恢复使用的包**不能**被删（实际删了 {:?}）",
            outcome.deleted
        );
        assert!(
            store.dir.join(&protected[0]).exists(),
            "受保护的包必须仍然在磁盘上，否则用户连重来一次都做不到"
        );
        // 保护一个不等于停止轮换：另外两份最老的仍要删掉。
        assert_eq!(
            outcome.deleted.len(),
            2,
            "超出 2 份，受保护的那份之外仍应删掉 2 份（实际 {:?}）",
            outcome.deleted
        );
    }

    /// **反向对照**：同样的种子、同样的上限，**不传**保护名单 → 那份包会被删掉。
    ///
    /// 这条是上一条的判别力来源：没有它，"受保护的包还在"可能只是因为轮换根本没删任何
    /// 东西（例如上限算错、或排序反了），而不是因为保护起了作用。
    #[test]
    fn without_protection_the_same_backup_is_deleted() {
        let root = tmp_root("protect-reverse");
        let (_data, mut store) = open_store(&root);
        seed_entries(
            &store.dir,
            &[
                (BackupOrigin::Scheduled, "010000", 1, false),
                (BackupOrigin::Scheduled, "020000", 1, false),
                (BackupOrigin::Scheduled, "030000", 1, false),
                (BackupOrigin::Scheduled, "040000", 1, false),
                (BackupOrigin::Scheduled, "050000", 1, false),
            ],
        );
        let oldest = store
            .list()
            .unwrap()
            .iter()
            .find(|e| e.archive_name.contains("20260501T010000"))
            .expect("种子里应有 01:00 那份")
            .archive_name
            .clone();

        let outcome = store.enforce_rotation(3).expect("轮换应当成功");

        assert!(
            outcome.deleted.iter().any(|n| n == &oldest),
            "不传保护名单时，最老的那份**应当**被删 —— 否则前一条测试证明不了保护起了作用"
        );
    }

    /// 全部受保护且都超量时，如实报 `deficit`，而不是静默删掉一个。
    ///
    /// 这与"全部已固定"的处理一致：宁可让用户知道"超了但删不掉"，
    /// 也不能为了让数字好看而删掉他正在依赖的数据。
    #[test]
    fn all_protected_reports_deficit_instead_of_deleting() {
        let root = tmp_root("protect-deficit");
        let (_data, mut store) = open_store(&root);
        seed_entries(
            &store.dir,
            &[
                (BackupOrigin::Scheduled, "010000", 1, false),
                (BackupOrigin::Scheduled, "020000", 1, false),
                (BackupOrigin::Scheduled, "030000", 1, false),
            ],
        );
        let all: Vec<String> = store.list().unwrap().into_iter().map(|e| e.archive_name).collect();

        let outcome = store.enforce_rotation_protecting(1, &all).expect("轮换应当成功");

        assert!(outcome.deleted.is_empty(), "全部受保护时不应删任何一份");
        assert_eq!(
            outcome.undelatable_excess, 2,
            "应如实报告「还有 2 份超量但删不掉」，而不是假装轮换完成了"
        );
        assert!(
            outcome.warnings.iter().any(|w| w.contains("重启后生效")),
            "提示里应说明这些包是**因为待生效的恢复**才没删，用户才知道该做什么（实际 {:?}）",
            outcome.warnings
        );
    }

    /// 保护名单里的名字若已不存在（用户手工删了包），轮换照常进行、不报错。
    #[test]
    fn protection_for_a_missing_name_is_harmless() {
        let root = tmp_root("protect-missing");
        let (_data, mut store) = open_store(&root);
        seed_entries(
            &store.dir,
            &[
                (BackupOrigin::Scheduled, "010000", 1, false),
                (BackupOrigin::Scheduled, "020000", 1, false),
                (BackupOrigin::Scheduled, "030000", 1, false),
            ],
        );

        let outcome = store
            .enforce_rotation_protecting(1, &["20260101T000000-scheduled-1.zip".to_string()])
            .expect("名单里有个不存在的名字不应导致轮换失败");

        assert_eq!(outcome.deleted.len(), 2, "应正常按上限删掉 2 份");
    }
}

// ===========================================================================
// 端到端：`pending_restore_protection` 真的读到了标记里的包名
// ===========================================================================
//
// 上面那组测试验证的是**轮换会尊重保护名单**；这一组验证**名单真的来自标记文件**。
//
// 两者缺一不可：只有前者，一个"名单永远是空的"的实现也能全绿 ——
// 那正是本次要修的那个缺陷（用户点了恢复，轮换照删不误）。
mod pending_protection_is_read_from_marker {
    use super::*;

    /// 标记里写了 `protectedBackup` → 读出来就是它。
    #[test]
    fn marker_protected_backup_is_surfaced() {
        let root = tmp_root("marker-read");
        let native_dir = root.join("native-com.tieznext");
        std::fs::create_dir_all(&native_dir).unwrap();

        let pending = crate::migration_pending::PendingMigration::for_kind_protecting(
            crate::migration_pending::PendingKind::LocalRestore,
            root.join("com.tieznext"),
            root.join(".com.tieznext.pending-restore-1"),
            root.join("com.tieznext"),
            true,
            "0.5.9",
            Some("20260925T010000-scheduled-1.zip".to_string()),
        );
        crate::migration_pending::write(&native_dir, &pending).unwrap();

        let read_back = crate::migration_pending::read(&native_dir)
            .expect("刚写下的标记必须能读回");
        assert_eq!(
            read_back.protected_backup.as_deref(),
            Some("20260925T010000-scheduled-1.zip"),
            "标记必须记住这次恢复用的是哪份包 —— 否则轮换无从得知该保护谁"
        );
    }

    /// 迁移那条链写的标记没有这个字段 → 读回是 `None`（不该凭空造出一个包名）。
    #[test]
    fn takeover_marker_has_no_protected_backup() {
        let root = tmp_root("marker-read-migration");
        let native_dir = root.join("native-com.tieznext");
        std::fs::create_dir_all(&native_dir).unwrap();

        let pending = crate::migration_pending::PendingMigration::for_kind(
            crate::migration_pending::PendingKind::Takeover,
            root.join("旧目录"),
            root.join(".com.tieznext.pending-takeover"),
            root.join("com.tieznext"),
            true,
            "0.5.9",
        );
        crate::migration_pending::write(&native_dir, &pending).unwrap();

        let read_back = crate::migration_pending::read(&native_dir).unwrap();
        assert_eq!(
            read_back.protected_backup, None,
            "迁移没有'源备份包'这个概念，不该凭空写一个名字进去"
        );
    }
}

// ===========================================================================
// 保护名单的读取内核（`protection_from_marker_dir`）
// ===========================================================================
//
// 【这组测试是补上的，起因是一次"全绿却没覆盖"】
//
// 接线完成后做反向对照：把读取逻辑整个改成"永远返回空列表"，
// **544 个测试依然全绿** —— 说明从"标记文件"到"轮换的保护名单"这一段
// 当时没有任何守卫。而它恰好是最容易悄悄写错的地方。
//
// 拆出纯函数内核后，下面每条都有判别力。
mod protection_from_marker_dir_reads_the_marker {
    use super::*;

    fn write_marker(dir: &std::path::Path, protected: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();
        let pending = crate::migration_pending::PendingMigration::for_kind_protecting(
            crate::migration_pending::PendingKind::LocalRestore,
            dir.join("data"),
            dir.join(".pending"),
            dir.join("data"),
            true,
            "0.5.9",
            protected.map(|s| s.to_string()),
        );
        crate::migration_pending::write(dir, &pending).unwrap();
    }

    /// 标记里记了包名 → 名单里有它（这是让轮换"知道该保护谁"的唯一通道）。
    #[test]
    fn returns_the_name_recorded_in_the_marker() {
        let dir = tmp_root("pv-read");
        write_marker(&dir, Some("20260925T010000-scheduled-1.zip"));

        assert_eq!(
            protection_from_marker_dir(&dir),
            vec!["20260925T010000-scheduled-1.zip".to_string()],
            "读不到名字，轮换就会把用户正等着恢复用的那份包删掉"
        );
    }

    /// 标记存在但没有这个字段（迁移留下的）→ 空名单，不是"一个空字符串"。
    #[test]
    fn no_field_yields_empty_not_a_blank_placeholder() {
        let dir = tmp_root("pv-none");
        write_marker(&dir, None);

        assert!(
            protection_from_marker_dir(&dir).is_empty(),
            "没有保护对象时必须是空名单 —— 返回一个空串会让保护判定永远不成立，\
             而读代码的人会以为有东西被保护着"
        );
    }

    /// 没有标记文件 → 空名单（没有任何待生效的恢复，也就没有要额外保护的东西）。
    #[test]
    fn missing_marker_yields_empty() {
        let dir = tmp_root("pv-missing");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(protection_from_marker_dir(&dir).is_empty());
    }

    /// 标记文件损坏 → 空名单且**不 panic**（轮换不能因为一个坏文件而停摆）。
    #[test]
    fn corrupted_marker_yields_empty_without_panicking() {
        let dir = tmp_root("pv-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::migration_pending::marker_path(&dir),
            b"{ this is not the json you are looking for",
        )
        .unwrap();

        assert!(
            protection_from_marker_dir(&dir).is_empty(),
            "坏标记按'没有待办'处理：轮换照常进行，而不是让整个自动备份卡住"
        );
    }
}
