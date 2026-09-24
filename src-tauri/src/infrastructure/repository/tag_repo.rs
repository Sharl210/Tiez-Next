use crate::database::ENCRYPT_PREFIX;
use crate::domain::models::ClipboardEntry;
use crate::infrastructure::encryption;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub trait TagRepository {
    fn set_color(&self, name: &str, color: Option<String>) -> Result<(), String>;
    fn get_colors(&self) -> Result<HashMap<String, String>, String>;
    fn get_all_with_counts(&self) -> Result<HashMap<String, i32>, String>;
    /// 标签及其可用于排序的统计量。
    ///
    /// 与 [`Self::get_all_with_counts`] 并存而不是替换它：那个返回的
    /// `name -> count` 形状已被多处调用方依赖，改签名会牵动无关代码。
    fn get_all_with_stats(&self) -> Result<Vec<TagStats>, String>;
    fn create(&self, name: &str) -> Result<(), String>;
    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), String>;
    fn delete_globally(&self, name: &str, data_dir: Option<&std::path::Path>)
        -> Result<(), String>;
    fn get_entries_by_tag(&self, tag: &str) -> Result<Vec<ClipboardEntry>, String>;
    fn update_entry_tags(&self, id: i64, tags: Vec<String>) -> Result<(), String>;
}

/// 一个标签的统计量，供界面排序使用。
///
/// 这些字段全部能从既有数据推导，不需要新增数据库列：
/// * `count` 来自 `entry_tags` 的行数；
/// * `last_used_at` 来自该标签所关联条目的**最大时间戳**（毫秒）；
/// * `total_bytes` 是该标签所关联条目的正文长度之和——用来区分"少量长文本"与
///   "大量短文本"，单看条数看不出来。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TagStats {
    pub name: String,
    pub count: i32,
    /// 最近一次使用（该标签关联条目的最大 `timestamp`）。无关联条目时为 0。
    pub last_used_at: i64,
    /// 关联条目的正文长度之和。
    pub total_bytes: i64,
}

pub struct SqliteTagRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteTagRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    fn maybe_decrypt_text(&self, value: &str) -> String {
        if value.starts_with(ENCRYPT_PREFIX) {
            encryption::decrypt_value(value).unwrap_or_else(|| value.to_string())
        } else {
            value.to_string()
        }
    }

    fn refresh_entry_tags_json(conn: &Connection, entry_id: i64) -> Result<(), String> {
        let mut stmt = conn
            .prepare("SELECT tag FROM entry_tags WHERE entry_id = ? ORDER BY tag")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![entry_id], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;

        let mut tags: Vec<String> = Vec::new();
        for row in rows {
            if let Ok(tag) = row {
                if !tag.trim().is_empty() {
                    tags.push(tag);
                }
            }
        }

        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "UPDATE clipboard_history SET tags = ? WHERE id = ?",
            params![tags_json, entry_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl TagRepository for SqliteTagRepository {
    fn set_color(&self, name: &str, color: Option<String>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        if let Some(c) = color {
            conn.execute(
                "INSERT INTO saved_tags (name, color) VALUES (?1, ?2) 
                 ON CONFLICT(name) DO UPDATE SET color = ?2",
                params![name, c],
            )
            .map_err(|e| e.to_string())?;
        } else {
            conn.execute(
                "UPDATE saved_tags SET color = NULL WHERE name = ?1",
                params![name],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn get_colors(&self) -> Result<HashMap<String, String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT name, color FROM saved_tags WHERE color IS NOT NULL AND color != ''")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;

        let mut map = HashMap::new();
        for row in rows {
            if let Ok((name, color)) = row {
                map.insert(name, color);
            }
        }
        Ok(map)
    }

    fn get_all_with_counts(&self) -> Result<HashMap<String, i32>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT tag, COUNT(*) FROM entry_tags GROUP BY tag")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
            })
            .map_err(|e| e.to_string())?;

        let mut tag_counts: HashMap<String, i32> = HashMap::new();
        for row in rows {
            if let Ok((tag, count)) = row {
                tag_counts.insert(tag, count);
            }
        }

        // Also include saved tags with 0 count if not present
        let mut stmt_saved = conn
            .prepare("SELECT name FROM saved_tags")
            .map_err(|e| e.to_string())?;
        let saved_rows = stmt_saved
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;

        for row in saved_rows {
            if let Ok(name) = row {
                tag_counts.entry(name).or_insert(0);
            }
        }

        Ok(tag_counts)
    }

    fn get_all_with_stats(&self) -> Result<Vec<TagStats>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // 一次 join 取齐三个量。`LEFT JOIN` 让"有颜色但还没条目的标签"也出现，
        // 否则它们在界面上会凭空消失（与 `get_all_with_counts` 的行为保持一致）。
        let mut stats: HashMap<String, TagStats> = HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT et.tag, COUNT(h.id), COALESCE(MAX(h.timestamp), 0), \
                            COALESCE(SUM(LENGTH(h.content)), 0) \
                     FROM entry_tags et \
                     LEFT JOIN clipboard_history h ON h.id = et.entry_id \
                     GROUP BY et.tag",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i32>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows.flatten() {
                let (name, count, last_used_at, total_bytes) = row;
                stats.insert(
                    name.clone(),
                    TagStats { name, count, last_used_at, total_bytes },
                );
            }
        }

        // 只有颜色、尚无条目的标签也要列出来。
        let mut stmt_saved = conn
            .prepare("SELECT name FROM saved_tags")
            .map_err(|e| e.to_string())?;
        let saved_rows = stmt_saved
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for name in saved_rows.flatten() {
            stats.entry(name.clone()).or_insert(TagStats {
                name,
                count: 0,
                last_used_at: 0,
                total_bytes: 0,
            });
        }

        Ok(stats.into_values().collect())
    }

    fn create(&self, name: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR IGNORE INTO saved_tags (name) VALUES (?)",
            params![name],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // Update saved_tags table: merge color info if exists
        let old_color: Option<String> = conn
            .query_row(
                "SELECT color FROM saved_tags WHERE name = ?",
                params![old_name],
                |row| row.get(0),
            )
            .ok();

        conn.execute(
            "INSERT OR IGNORE INTO saved_tags (name, color) VALUES (?1, ?2)",
            params![new_name, old_color],
        )
        .map_err(|e| e.to_string())?;

        let _ = conn.execute("DELETE FROM saved_tags WHERE name = ?", params![old_name]);

        // Update entry_tags and refresh JSON cache
        let mut stmt = conn
            .prepare("SELECT entry_id FROM entry_tags WHERE tag = ?")
            .map_err(|e| e.to_string())?;
        let ids: Vec<i64> = stmt
            .query_map(params![old_name], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();

        for id in ids {
            conn.execute(
                "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
                params![id, new_name],
            )
            .map_err(|e| e.to_string())?;
            conn.execute(
                "DELETE FROM entry_tags WHERE entry_id = ? AND tag = ?",
                params![id, old_name],
            )
            .map_err(|e| e.to_string())?;
            Self::refresh_entry_tags_json(&conn, id)?;
        }
        Ok(())
    }

    /// R3: delete a tag *group*, never the entries that carried it.
    ///
    /// History: this used to iterate `entry_tags` and `DELETE FROM clipboard_history`
    /// for every member id, so removing a group silently destroyed the user's
    /// clipboard entries (and their attachment files). R3 requires the opposite
    /// contract: the group disappears, `entry_tags` links are cleaned up, and the
    /// entries themselves survive with only that one tag removed. Anything else
    /// makes "delete group" indistinguishable from "delete data".
    fn delete_globally(
        &self,
        name: &str,
        _data_dir: Option<&std::path::Path>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // Collect the affected entries *before* dropping their links, so the
        // denormalized `clipboard_history.tags` JSON cache can be refreshed for
        // each of them. Tag matching is case-insensitive here to mirror the
        // deduplication and sensitive-tag checks elsewhere; a tag stored as
        // "Sensitive" must not survive as a ghost.
        let mut stmt = conn
            .prepare("SELECT DISTINCT entry_id FROM entry_tags WHERE tag = ? COLLATE NOCASE")
            .map_err(|e| e.to_string())?;
        let ids: Vec<i64> = stmt
            .query_map(params![name], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        conn.execute(
            "DELETE FROM entry_tags WHERE tag = ? COLLATE NOCASE",
            params![name],
        )
        .map_err(|e| e.to_string())?;
        let _ = conn.execute(
            "DELETE FROM saved_tags WHERE name = ? COLLATE NOCASE",
            params![name],
        );

        // Rebuild the JSON tag cache from the authoritative join table. Entries
        // that lost their last tag end up with `[]`, which the list, filter,
        // search and paste paths all already handle.
        for id in ids {
            Self::refresh_entry_tags_json(&conn, id)?;
        }
        Ok(())
    }

    fn get_entries_by_tag(&self, tag: &str) -> Result<Vec<ClipboardEntry>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn.prepare(
            "SELECT ch.id, ch.content_type, ch.content, ch.html_content, ch.source_app, ch.timestamp, ch.preview, ch.is_pinned, ch.tags, ch.use_count, ch.is_external, ch.pinned_order, ch.source_app_path, ch.note 
             FROM clipboard_history ch
             INNER JOIN entry_tags et ON ch.id = et.entry_id
             WHERE et.tag = ? 
             ORDER BY ch.is_pinned DESC, ch.pinned_order DESC, ch.timestamp DESC",
        ).map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map([tag], |row| {
                let tags_str: String = row.get(8).unwrap_or_else(|_| "[]".to_string());
                let tags: Vec<String> = serde_json::from_str(&tags_str).unwrap_or_default();
                let content_raw: String = row.get(2)?;
                let html_raw: Option<String> = row.get(3).ok();
                let preview_raw: String = row.get(6)?;
                let content = self.maybe_decrypt_text(&content_raw);
                let preview = self.maybe_decrypt_text(&preview_raw);
                let html_content = html_raw.map(|v| self.maybe_decrypt_text(&v));

                Ok(ClipboardEntry {
                    id: row.get(0)?,
                    content_type: row.get(1)?,
                    content,
                    html_content,
                    source_app: row.get(4)?,
                    timestamp: row.get(5)?,
                    preview,
                    is_pinned: row.get::<_, i32>(7)? == 1,
                    tags,
                    use_count: row.get(9).unwrap_or(0),
                    is_external: row.get::<_, i32>(10)? == 1,
                    pinned_order: row.get(11).unwrap_or(0),
                    source_app_path: row.get(12).unwrap_or(None),
                    note: row.get::<_, String>(13).unwrap_or_default(),
                    file_preview_exists: true, // simplified
                })
            })
            .map_err(|e| e.to_string())?;

        let mut history = Vec::new();
        for row in rows {
            if let Ok(entry) = row {
                history.push(entry);
            }
        }
        Ok(history)
    }

    fn update_entry_tags(&self, id: i64, tags: Vec<String>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut cleaned: Vec<String> = Vec::new();
        for tag in tags {
            let t = tag.trim();
            if t.is_empty() {
                continue;
            }
            let t_owned = t.to_string();
            if seen.insert(t_owned.clone()) {
                cleaned.push(t_owned);
            }
        }

        conn.execute("DELETE FROM entry_tags WHERE entry_id = ?", params![id])
            .map_err(|e| e.to_string())?;
        for tag in &cleaned {
            conn.execute(
                "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )
            .map_err(|e| e.to_string())?;
        }

        let tags_json = serde_json::to_string(&cleaned).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "UPDATE clipboard_history SET tags = ? WHERE id = ?",
            params![tags_json, id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `search` / `get_history` are trait methods, so the trait must be in scope.
    use crate::infrastructure::repository::clipboard_repo::{
        ClipboardRepository, SqliteClipboardRepository,
    };

    /// Minimal schema for the paths under test, mirroring the columns
    /// `delete_globally` and the note/body paths actually touch. Deliberately not
    /// the full production schema: these tests assert tag-unlinking behaviour, and a
    /// wider fixture would couple them to unrelated migrations.
    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE clipboard_history (
                id INTEGER PRIMARY KEY,
                content_type TEXT NOT NULL,
                content TEXT NOT NULL,
                html_content TEXT,
                source_app TEXT NOT NULL,
                source_app_path TEXT,
                timestamp INTEGER NOT NULL,
                preview TEXT NOT NULL,
                is_pinned INTEGER NOT NULL DEFAULT 0,
                content_hash INTEGER NOT NULL DEFAULT 0,
                tags TEXT NOT NULL DEFAULT '[]',
                use_count INTEGER NOT NULL DEFAULT 0,
                is_external INTEGER NOT NULL DEFAULT 0,
                pinned_order INTEGER NOT NULL DEFAULT 0,
                note TEXT NOT NULL DEFAULT ''
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE saved_tags (name TEXT PRIMARY KEY, color TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE entry_tags (
                entry_id INTEGER NOT NULL,
                tag TEXT NOT NULL,
                PRIMARY KEY (entry_id, tag)
            )",
            [],
        )
        .unwrap();
        conn
    }

// ------------------------------------------------------------------
// 标签统计量：界面排序依赖这些数字，算错会让排序静默失真
// ------------------------------------------------------------------

/// 插入一行并指定时间戳与正文长度，用于验证统计量。
fn seed_row_with(
    conn_arc: &Arc<Mutex<Connection>>,
    content: &str,
    timestamp: i64,
    tags: &[&str],
) -> i64 {
    let conn = conn_arc.lock().unwrap();
    let tags_json = serde_json::to_string(&tags).unwrap();
    conn.execute(
        "INSERT INTO clipboard_history
            (content_type, content, html_content, source_app, timestamp, preview,
             is_pinned, content_hash, tags, use_count, is_external, pinned_order)
         VALUES ('text', ?1, NULL, 'TestApp', ?2, ?1, 0, 0, ?3, 0, 0, 0)",
        rusqlite::params![content, timestamp, tags_json],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    for tag in tags {
        conn.execute(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
            rusqlite::params![id, tag],
        )
        .unwrap();
    }
    id
}

fn stats_of(stats: &[TagStats], name: &str) -> TagStats {
    stats
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("统计量里应包含标签 {name}"))
        .clone()
}

/// 条目数、最近使用时间、总字节数三个量都要算对。
#[test]
fn stats_report_count_recent_use_and_total_bytes() {
    let conn = Arc::new(Mutex::new(setup_test_db()));
    let repo = SqliteTagRepository::new(conn.clone());

    seed_row_with(&conn, "aaa", 100, &["工作"]);
    seed_row_with(&conn, "bbbbb", 300, &["工作"]);
    seed_row_with(&conn, "c", 200, &["生活"]);

    let stats = repo.get_all_with_stats().unwrap();

    let work = stats_of(&stats, "工作");
    assert_eq!(work.count, 2, "工作 应有 2 条");
    assert_eq!(work.last_used_at, 300, "工作 的最近使用应是最大的那个时间戳");
    assert_eq!(work.total_bytes, 8, "工作 的正文长度之和应为 3 + 5");

    let life = stats_of(&stats, "生活");
    assert_eq!(life.count, 1);
    assert_eq!(life.last_used_at, 200);
    assert_eq!(life.total_bytes, 1);
}

/// 只有颜色、还没有任何条目的标签也必须出现——否则它在界面上会凭空消失，
/// 用户就再也删不掉它了。
#[test]
fn stats_include_saved_tags_that_have_no_entries_yet() {
    let conn = Arc::new(Mutex::new(setup_test_db()));
    let repo = SqliteTagRepository::new(conn.clone());
    conn.lock()
        .unwrap()
        .execute("INSERT INTO saved_tags (name, color) VALUES ('空标签', '#fff')", [])
        .unwrap();
    seed_row_with(&conn, "x", 100, &["有内容"]);

    let stats = repo.get_all_with_stats().unwrap();

    let empty = stats_of(&stats, "空标签");
    assert_eq!(empty.count, 0);
    assert_eq!(empty.last_used_at, 0, "没有条目时最近使用为 0");
    assert_eq!(empty.total_bytes, 0);
    assert!(stats.iter().any(|s| s.name == "有内容"));
}

/// 没有任何标签时返回空列表，而不是报错或造出假标签。
#[test]
fn stats_are_empty_on_a_fresh_database() {
    let conn = Arc::new(Mutex::new(setup_test_db()));
    let repo = SqliteTagRepository::new(conn.clone());
    assert!(repo.get_all_with_stats().unwrap().is_empty());
}

/// 一个条目挂多个标签时，每个标签都要算到它——不能因为 join 而漏掉。
#[test]
fn stats_count_an_entry_for_every_tag_it_carries() {
    let conn = Arc::new(Mutex::new(setup_test_db()));
    let repo = SqliteTagRepository::new(conn.clone());
    seed_row_with(&conn, "shared", 500, &["甲", "乙", "丙"]);

    let stats = repo.get_all_with_stats().unwrap();

    for name in ["甲", "乙", "丙"] {
        let s = stats_of(&stats, name);
        assert_eq!(s.count, 1, "{name} 应各计 1 条");
        assert_eq!(s.last_used_at, 500);
        assert_eq!(s.total_bytes, 6);
    }
    assert_eq!(stats.len(), 3, "不应产生重复标签");
}

// ------------------------------------------------------------------
// R3: deleting a tag group must unlink entries, never delete them
// ------------------------------------------------------------------

/// Seed one row plus its `entry_tags` links with raw SQL.
///
/// Deliberately bypasses `repo.save`: the save path runs image handling and
/// dedup, neither of which is under test here, and a raw insert keeps these
/// tests independent of it.
fn seed_row(
    conn_arc: &Arc<Mutex<Connection>>,
    content: &str,
    content_type: &str,
    tags: &[&str],
) -> i64 {
    let conn = conn_arc.lock().unwrap();
    let tags_json = serde_json::to_string(&tags).unwrap();
    conn.execute(
        "INSERT INTO clipboard_history
            (content_type, content, html_content, source_app, timestamp, preview,
             is_pinned, content_hash, tags, use_count, is_external, pinned_order)
         VALUES (?1, ?2, NULL, 'TestApp', 1, ?2, 0, 0, ?3, 0, 0, 0)",
        rusqlite::params![content_type, content, tags_json],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    for tag in tags {
        conn.execute(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
            rusqlite::params![id, tag],
        )
        .unwrap();
    }
    id
}

/// `SELECT COUNT(*)` over a query that takes no parameters.
fn count_all(conn_arc: &Arc<Mutex<Connection>>, sql: &str) -> i64 {
    let conn = conn_arc.lock().unwrap();
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// One text column of one row, selected by id.
fn scalar_text(conn_arc: &Arc<Mutex<Connection>>, sql: &str, id: i64) -> String {
    let conn = conn_arc.lock().unwrap();
    conn.query_row(sql, rusqlite::params![id], |row| row.get(0)).unwrap()
}

/// One integer column of one row, selected by id.
fn scalar_int(conn_arc: &Arc<Mutex<Connection>>, sql: &str, id: i64) -> i64 {
    let conn = conn_arc.lock().unwrap();
    conn.query_row(sql, rusqlite::params![id], |row| row.get(0)).unwrap()
}

#[test]
fn r3_delete_tag_group_unlinks_entries_without_deleting_them() {
    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let tag_repo =
        crate::infrastructure::repository::tag_repo::SqliteTagRepository::new(conn_arc.clone());

    let victim = seed_row(&conn_arc, "secret note", "text", &["密码"]);
    let survivor = seed_row(&conn_arc, "plain note", "text", &["work"]);
    tag_repo.create("密码").unwrap();

    crate::infrastructure::repository::tag_repo::TagRepository::delete_globally(
        &tag_repo, "密码", None,
    )
    .expect("delete_globally failed");

    // The entries themselves survive — this is the whole point of the change.
    assert_eq!(
        count_all(&conn_arc, "SELECT COUNT(*) FROM clipboard_history"),
        2,
        "both entries must survive the group delete"
    );

    // The join links are gone, and so is the group row.
    assert_eq!(
        count_all(&conn_arc, "SELECT COUNT(*) FROM entry_tags WHERE tag = '密码'"),
        0,
        "entry_tags links for the deleted group must be gone"
    );
    assert_eq!(
        count_all(&conn_arc, "SELECT COUNT(*) FROM saved_tags WHERE name = '密码'"),
        0,
        "saved_tags row for the deleted group must be gone"
    );

    // The other group is untouched.
    assert_eq!(
        count_all(&conn_arc, "SELECT COUNT(*) FROM entry_tags WHERE tag = 'work'"),
        1,
        "unrelated group must be untouched"
    );

    // The denormalized JSON cache is refreshed, so no ghost tag remains visible.
    assert_eq!(
        scalar_text(&conn_arc, "SELECT tags FROM clipboard_history WHERE id = ?1", victim),
        "[]",
        "JSON tag cache must drop the deleted group"
    );
    assert_eq!(
        scalar_text(&conn_arc, "SELECT tags FROM clipboard_history WHERE id = ?1", survivor),
        "[\"work\"]",
        "unrelated entry's cache must be intact"
    );
}

#[test]
fn r3_delete_tag_group_is_case_insensitive() {
    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let tag_repo =
        crate::infrastructure::repository::tag_repo::SqliteTagRepository::new(conn_arc.clone());

    let id = seed_row(&conn_arc, "case test", "text", &["Sensitive"]);

    // Lower-case target against a mixed-case stored tag: a user-facing delete must
    // not leave the group behind because of capitalisation.
    crate::infrastructure::repository::tag_repo::TagRepository::delete_globally(
        &tag_repo,
        "sensitive",
        None,
    )
    .expect("delete_globally failed");

    assert_eq!(
        scalar_text(&conn_arc, "SELECT tags FROM clipboard_history WHERE id = ?1", id),
        "[]",
        "case-insensitive delete must clear the link and the cache"
    );
    assert_eq!(
        count_all(&conn_arc, "SELECT COUNT(*) FROM clipboard_history"),
        1,
        "the entry must survive"
    );
}

// ------------------------------------------------------------------
// R4/R6: note storage and the binary body guard
// ------------------------------------------------------------------

#[test]
fn r6_update_entry_note_round_trips_and_clears() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "note target", "text", &[]);

    repo.update_entry_note(id, "  这是备注  ").expect("note write failed");
    assert_eq!(
        scalar_text(&conn_arc, "SELECT note FROM clipboard_history WHERE id = ?1", id),
        "这是备注",
        "note must be trimmed on write"
    );

    // Clearing is an empty write, not a delete of the row.
    repo.update_entry_note(id, "   ").expect("note clear failed");
    assert_eq!(
        scalar_text(&conn_arc, "SELECT note FROM clipboard_history WHERE id = ?1", id),
        "",
        "whitespace-only note must clear it"
    );
    assert_eq!(
        count_all(&conn_arc, "SELECT COUNT(*) FROM clipboard_history"),
        1,
        "clearing a note must not touch the entry"
    );
}

#[test]
fn r6_update_entry_note_clamps_to_char_budget() {
    use crate::infrastructure::repository::clipboard_repo::{
        ClipboardRepository, MAX_ENTRY_NOTE_CHARS,
    };

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "long note", "text", &[]);

    // Multi-byte characters: the clamp must count chars, not bytes, or CJK notes
    // would be cut at roughly a third of the advertised budget.
    let long_note = "备".repeat(MAX_ENTRY_NOTE_CHARS + 50);
    repo.update_entry_note(id, &long_note).expect("note write failed");

    let stored = scalar_text(&conn_arc, "SELECT note FROM clipboard_history WHERE id = ?1", id);
    assert_eq!(stored.chars().count(), MAX_ENTRY_NOTE_CHARS);
}

#[test]
fn r6_update_entry_note_on_missing_row_is_not_an_error() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc);

    // An entry deleted in another window must not surface as a hard failure.
    repo.update_entry_note(4242, "orphan").expect("missing row must be a no-op");
}

#[test]
fn r4_body_edit_is_refused_for_binary_content_types() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());

    let cases = [
        ("image", "/data/attachments/a.png"),
        ("file", "/data/attachments/doc.pdf"),
        ("video", "/data/attachments/clip.mp4"),
    ];
    for (content_type, original) in cases {
        let id = seed_row(&conn_arc, original, content_type, &[]);

        let err = repo
            .update_entry_content(id, "replacement text", "replacement", None)
            .expect_err("binary body edit must be refused");
        assert!(
            err.contains(content_type),
            "error should name the offending type, got: {}",
            err
        );

        // The payload is untouched, so the row cannot become a hash/content mismatch.
        assert_eq!(
            scalar_text(&conn_arc, "SELECT content FROM clipboard_history WHERE id = ?1", id),
            original,
            "{} body must be unchanged",
            content_type
        );

        // The note path stays available for the same entry.
        repo.update_entry_note(id, "annotated").expect("note must still work");
    }
}

#[test]
fn r4_body_edit_still_works_for_text_types() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());

    for content_type in ["text", "code", "url"] {
        let id = seed_row(&conn_arc, "before", content_type, &[]);
        repo.update_entry_content(id, "after", "after", None)
            .expect("text body edit must succeed");

        assert_eq!(
            scalar_text(&conn_arc, "SELECT content FROM clipboard_history WHERE id = ?1", id),
            "after"
        );
        assert_eq!(
            scalar_text(
                &conn_arc,
                "SELECT content_type FROM clipboard_history WHERE id = ?1",
                id
            ),
            content_type,
            "content_type must be preserved"
        );
    }
}

#[test]
fn r13_rich_text_edit_keeps_type_and_html() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "rich body", "rich_text", &[]);
    {
        let conn = conn_arc.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_history SET html_content = '<b>rich body</b>' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    // 只改正文、不带 HTML：这是"用户在编辑器里改了文字但没动格式"的场景。
    repo.update_entry_content(id, "plain now", "plain now", None)
        .expect("rich_text edit must succeed");

    // R13：**不再降级**。曾几何时这里断言 `content_type == "text"` 且 HTML 被清空
    // （函数名 `r4_rich_text_edit_downgrades_to_text_and_clears_html`），
    // 那正是用户报的"编辑富文本会坍缩成纯文本"。现在反向断言：
    // 类型保持 `rich_text`，且原有 HTML 原样留着。
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT content_type FROM clipboard_history WHERE id = ?1",
            id
        ),
        "rich_text",
        "editing a rich-text body must NOT downgrade the content type"
    );
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT COALESCE(html_content, '<null>') FROM clipboard_history WHERE id = ?1",
            id
        ),
        "<b>rich body</b>",
        "editing a rich-text body must NOT clear html_content"
    );
    // 正文本身照旧被写入
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT content FROM clipboard_history WHERE id = ?1",
            id
        ),
        "plain now"
    );
}

#[test]
fn r13_rich_text_edit_recomputes_content_hash() {
    use crate::database::calc_text_hash;
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "before", "rich_text", &[]);

    repo.update_entry_content(id, "after", "after", Some("<p>after</p>"))
        .expect("rich_text edit must succeed");

    // 内容哈希必须跟着正文重算：否则去重与云同步 sync_key 会拿一个已不描述本行的
    // 哈希去比对，导致该合并的条目并存、该同步的改动被判为相同。
    assert_eq!(
        scalar_int(
            &conn_arc,
            "SELECT content_hash FROM clipboard_history WHERE id = ?1",
            id
        ),
        calc_text_hash("after") as i64,
        "content_hash must be recomputed from the new content"
    );
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT COALESCE(html_content, '<null>') FROM clipboard_history WHERE id = ?1",
            id
        ),
        "<p>after</p>",
        "the supplied html must be persisted"
    );
}

#[test]
fn r13_format_only_edit_is_not_treated_as_a_no_op() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "same words", "rich_text", &[]);
    {
        let conn = conn_arc.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_history SET html_content = '<p>same words</p>' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    // 正文一字未改，只把"同一段文字"加粗。
    repo.update_entry_content(id, "same words", "same words", Some("<p><b>same words</b></p>"))
        .expect("format-only edit must succeed");

    // 旧短路条件（`old_content == content && content_type != "rich_text" && !has_html`）
    // 会把"只改格式"当成无变化而丢弃；新增的 html 比较让它落盘。
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT COALESCE(html_content, '<null>') FROM clipboard_history WHERE id = ?1",
            id
        ),
        "<p><b>same words</b></p>",
        "a format-only change must be persisted, not dropped as a no-op"
    );
}

#[test]
fn r13_unchanged_edit_is_a_true_no_op() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "同文", "rich_text", &[]);
    {
        let conn = conn_arc.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_history SET html_content = '<p>同文</p>' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    // 正文与 HTML 都没变 -> 必须真的短路（否则每次保存都会多一次无意义的写与刷新）。
    repo.update_entry_content(id, "同文", "同文", Some("<p>同文</p>"))
        .expect("no-op edit must succeed");
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT COALESCE(html_content, '<null>') FROM clipboard_history WHERE id = ?1",
            id
        ),
        "<p>同文</p>"
    );
}

#[test]
fn r13_sensitive_rich_text_encrypts_html_together_with_content() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "秘密正文", "rich_text", &["sensitive"]);

    repo.update_entry_content(id, "新的秘密", "新的秘密", Some("<p>新的秘密</p>"))
        .expect("sensitive rich_text edit must succeed");

    // 敏感条目的 HTML 必须与正文一起加密：只加密正文而 HTML 明文落库，等于
    // 用户以为打了敏感标签，内容却仍是明文。
    let html = scalar_text(
        &conn_arc,
        "SELECT COALESCE(html_content, '<null>') FROM clipboard_history WHERE id = ?1",
        id,
    );
    assert!(
        !html.contains("新的秘密"),
        "html_content must not be stored in plaintext for a sensitive entry, got: {}",
        html
    );
}

#[test]
fn r13_rich_edit_derives_plain_content_from_html() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "旧的正文", "rich_text", &[]);

    // 界面为了不丢格式，送的是编辑器里的 `innerHTML`。正文列必须是**派生的纯文本**，
    // 否则粘贴与列表预览会拿到 HTML 源码，与界面显示的正文不符。
    repo.update_entry_content(
        id,
        "<p>新的<b>正文</b></p>",
        "<p>新的<b>正文</b></p>",
        Some("<p>新的<b>正文</b></p>"),
    )
    .expect("rich edit must succeed");

    let content = scalar_text(
        &conn_arc,
        "SELECT content FROM clipboard_history WHERE id = ?1",
        id,
    );
    assert!(
        !content.contains('<'),
        "content 必须是派生的纯文本，不能是 HTML 源码，got: {}",
        content
    );
    assert!(content.contains("新的"), "got: {}", content);
    assert!(content.contains("正文"), "got: {}", content);

    // 预览同样从派生正文重算，否则列表里显示的还是旧文字。
    let preview = scalar_text(
        &conn_arc,
        "SELECT preview FROM clipboard_history WHERE id = ?1",
        id,
    );
    assert!(!preview.contains("旧的"), "preview 未重算：{}", preview);
}

#[test]
fn r13_rich_write_is_idempotent_for_repeated_saves() {
    use crate::database::calc_text_hash;
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "同文", "rich_text", &[]);

    let html = "<p>同文</p>";
    repo.update_entry_content(id, html, html, Some(html)).unwrap();
    let h1 = scalar_int(
        &conn_arc,
        "SELECT content_hash FROM clipboard_history WHERE id = ?1",
        id,
    );
    let c1 = scalar_text(
        &conn_arc,
        "SELECT content FROM clipboard_history WHERE id = ?1",
        id,
    );

    // 再存一次完全相同的 HTML：派生结果必须稳定，哈希不变。
    repo.update_entry_content(id, html, html, Some(html)).unwrap();
    let h2 = scalar_int(
        &conn_arc,
        "SELECT content_hash FROM clipboard_history WHERE id = ?1",
        id,
    );
    let c2 = scalar_text(
        &conn_arc,
        "SELECT content FROM clipboard_history WHERE id = ?1",
        id,
    );

    assert_eq!(h1, h2, "派生必须是确定性的，否则去重与云同步会误判");
    assert_eq!(c1, c2);
    assert_eq!(h1, calc_text_hash(&c1) as i64, "哈希必须描述派生后的正文");
}

#[test]
fn r13_text_entry_edit_never_gains_html() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "plain", "text", &[]);

    // 对一个纯文本条目附带 HTML：不应造出"类型是 text、却带着 HTML"的错位行。
    repo.update_entry_content(id, "changed", "changed", Some("<p>changed</p>"))
        .expect("text edit must succeed");

    assert_eq!(
        scalar_int(
            &conn_arc,
            "SELECT COUNT(*) FROM clipboard_history WHERE id = ?1 AND html_content IS NULL",
            id
        ),
        1,
        "a text row must not gain html_content"
    );
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT content_type FROM clipboard_history WHERE id = ?1",
            id
        ),
        "text"
    );
}

#[test]
fn r4_binary_content_type_helper_matches_the_schema() {
    use crate::infrastructure::repository::clipboard_repo::is_binary_content_type;
    for t in ["image", "file", "video"] {
        assert!(is_binary_content_type(t), "{} must be treated as binary", t);
    }
    for t in ["text", "code", "url", "rich_text", ""] {
        assert!(!is_binary_content_type(t), "{} must be editable as text", t);
    }
}

#[test]
fn r6_session_note_survives_first_save() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
    use crate::domain::models::ClipboardEntry;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());

    // A session row (id 0) carrying a remark, as produced by tagging a
    // not-yet-persisted entry: `update_entry_note` writes the note in memory and the
    // subsequent `save` is what actually persists the row. If `save`'s INSERT omits
    // the note column the remark is silently dropped here — which is exactly what
    // used to happen.
    let entry = ClipboardEntry {
        id: 0,
        content_type: "text".to_string(),
        content: "session row".to_string(),
        html_content: None,
        source_app: "Test".to_string(),
        source_app_path: None,
        timestamp: 1,
        preview: "session row".to_string(),
        is_pinned: false,
        tags: vec!["work".to_string()],
        use_count: 0,
        is_external: false,
        pinned_order: 0,
        note: "SESSION NOTE".to_string(),
        file_preview_exists: true,
    };

    let new_id = repo.save(&entry, None).expect("save failed");

    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT note FROM clipboard_history WHERE id = ?1",
            new_id
        ),
        "SESSION NOTE",
        "a note written on a session row must survive its first save"
    );
}

#[test]
fn r6_repeat_save_does_not_clobber_an_existing_note() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
    use crate::domain::models::ClipboardEntry;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "repeat target", "text", &[]);

    // The user writes a remark...
    repo.update_entry_note(id, "IMPORTANT NOTE").expect("note write failed");

    // ...and the same content is copied again. The update branch must not touch the
    // note column: callers pass an entry captured earlier, so writing it through
    // would let a stale copy wipe the remark.
    let mut entry = repo.get_entry_by_id(id).expect("read failed").expect("row missing");
    entry.note = String::new();
    repo.save(&entry, None).expect("save failed");

    assert_eq!(
        scalar_text(&conn_arc, "SELECT note FROM clipboard_history WHERE id = ?1", id),
        "IMPORTANT NOTE",
        "re-saving an entry must not clear a note written in the meantime"
    );
}

#[test]
fn r6_normalize_note_is_total() {
    use crate::infrastructure::repository::clipboard_repo::{normalize_note, MAX_ENTRY_NOTE_CHARS};
    assert_eq!(normalize_note(""), "");
    assert_eq!(normalize_note("  padded  "), "padded");
    assert_eq!(normalize_note("\n\t mixed \r\n"), "mixed");
    // No panic on boundary lengths, and no over-long result.
    let exact = "x".repeat(MAX_ENTRY_NOTE_CHARS);
    assert_eq!(normalize_note(&exact), exact);
    let one_over = "x".repeat(MAX_ENTRY_NOTE_CHARS + 1);
    assert_eq!(normalize_note(&one_over).chars().count(), MAX_ENTRY_NOTE_CHARS);
}

    /// R3 acceptance: after a group is deleted, every read path the UI uses for that
    /// group must return empty rather than error, and no ghost tag may survive in the
    /// denormalized cache the list views read.
    #[test]
    fn r3_deleted_group_leaves_no_ghost_and_reads_cleanly() {
        let conn = setup_test_db();
        let conn_arc = Arc::new(Mutex::new(conn));
        let repo = SqliteClipboardRepository::new(conn_arc.clone());
        let tag_repo = SqliteTagRepository::new(conn_arc.clone());

        seed_row(&conn_arc, "ghost candidate", "text", &["临时分组"]);
        seed_row(&conn_arc, "unrelated", "text", &["keep"]);
        tag_repo.create("临时分组").unwrap();

        TagRepository::delete_globally(&tag_repo, "临时分组", None).expect("delete failed");

        // 1. The group is gone from the tag list the manager renders.
        let counts = tag_repo.get_all_with_counts().unwrap();
        assert!(
            !counts.contains_key("临时分组"),
            "deleted group must not be offered by get_all_with_counts"
        );
        assert_eq!(counts.get("keep").copied(), Some(1));

        // 2. Loading a deleted group's items returns an empty list, not an error.
        let items = tag_repo.get_entries_by_tag("临时分组").expect("read must not error");
        assert!(items.is_empty(), "a deleted group must have no items");

        // 3. Tag-filtered search for the deleted name returns nothing.
        let found = repo.search("临时分组", 10, false).expect("search must not error");
        assert!(
            found.is_empty(),
            "searching a deleted group must not resurrect entries through stale links"
        );

        // 4. The surviving group is still fully readable.
        let kept = tag_repo.get_entries_by_tag("keep").expect("read must not error");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].tags, vec!["keep".to_string()]);

        // 5. The entry that lost its only tag keeps an empty tag list, not a stale one.
        let history = repo.get_history(10, 0, None).unwrap();
        let orphan = history
            .iter()
            .find(|e| e.content == "ghost candidate")
            .expect("the untagged entry must still be listed");
        assert!(orphan.tags.is_empty(), "no ghost tag may remain on the entry");
    }

}
