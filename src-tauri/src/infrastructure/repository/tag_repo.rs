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
    fn create(&self, name: &str) -> Result<(), String>;
    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), String>;
    fn delete_globally(&self, name: &str, data_dir: Option<&std::path::Path>)
        -> Result<(), String>;
    fn get_entries_by_tag(&self, tag: &str) -> Result<Vec<ClipboardEntry>, String>;
    fn update_entry_tags(&self, id: i64, tags: Vec<String>) -> Result<(), String>;
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
            .update_entry_content(id, "replacement text", "replacement")
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
        repo.update_entry_content(id, "after", "after")
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
fn r4_rich_text_edit_downgrades_to_text_and_clears_html() {
    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;

    let conn = setup_test_db();
    let conn_arc = Arc::new(Mutex::new(conn));
    let repo = SqliteClipboardRepository::new(conn_arc.clone());
    let id = seed_row(&conn_arc, "rich body", "rich_text", &[]);
    {
        // Give the row HTML so the downgrade path has something to clear.
        let conn = conn_arc.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_history SET html_content = '<b>rich body</b>' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    repo.update_entry_content(id, "plain now", "plain now")
        .expect("rich_text edit must succeed");

    // This is the documented, pre-existing backend behaviour the UI warns about:
    // editing a rich-text body turns it into plain text and drops the HTML.
    assert_eq!(
        scalar_text(
            &conn_arc,
            "SELECT content_type FROM clipboard_history WHERE id = ?1",
            id
        ),
        "text"
    );
    assert_eq!(
        scalar_int(
            &conn_arc,
            "SELECT COUNT(*) FROM clipboard_history WHERE id = ?1 AND html_content IS NULL",
            id
        ),
        1,
        "html_content must be cleared"
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
