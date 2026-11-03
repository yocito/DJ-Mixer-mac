//! SQLite（djmix/db.py と同じ表）。解析結果（自動）とユーザー修正（user_edits）は別の表に分けて保存する。
//!
//! 接続はスレッドごとに持つ（サーバーの処理は複数のスレッドで動くため）。WAL なので読み書きが重なっても待たずに済む。
use crate::config::paths;
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tracks(
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,
  folder TEXT NOT NULL,
  size INTEGER, mtime REAL,
  title TEXT, artist TEXT, album TEXT, genre TEXT, year TEXT,
  duration REAL,
  analysis_status TEXT NOT NULL DEFAULT 'none',
  analysis_version INTEGER,
  analysis_error TEXT,
  bpm REAL, camelot TEXT, lufs REAL, beat_class TEXT
);
CREATE INDEX IF NOT EXISTS ix_tracks_folder ON tracks(folder);
CREATE INDEX IF NOT EXISTS ix_tracks_status ON tracks(analysis_status);
CREATE TABLE IF NOT EXISTS user_edits(
  track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
  meter INTEGER,
  bpm_scale REAL NOT NULL DEFAULT 1,
  downbeat_anchors TEXT,
  grid_offset_ms REAL NOT NULL DEFAULT 0,
  hotcues TEXT, cue_point REAL, memo TEXT
);
CREATE TABLE IF NOT EXISTS queues(
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  updated REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS queue_items(
  queue_id INTEGER NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
  pos INTEGER NOT NULL,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  use_start REAL, use_end REAL, use_label TEXT,
  PRIMARY KEY(queue_id, pos)
);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS track_prefs(
  track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
  rating INTEGER NOT NULL DEFAULT 0,
  play_count INTEGER NOT NULL DEFAULT 0,
  last_played REAL
);
CREATE TABLE IF NOT EXISTS itunes_playlists(
  id INTEGER PRIMARY KEY,
  pid TEXT,
  name TEXT NOT NULL,
  parent_pid TEXT,
  is_folder INTEGER NOT NULL DEFAULT 0,
  is_smart INTEGER NOT NULL DEFAULT 0,
  sort INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS itunes_playlist_items(
  playlist_id INTEGER NOT NULL REFERENCES itunes_playlists(id) ON DELETE CASCADE,
  pos INTEGER NOT NULL,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS ix_itpl_items ON itunes_playlist_items(playlist_id, pos);
CREATE TABLE IF NOT EXISTS plays(
  id INTEGER PRIMARY KEY,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  at REAL NOT NULL,
  source TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_plays_at ON plays(at);
CREATE TABLE IF NOT EXISTS track_desc(
  track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  data TEXT NOT NULL
);
"#;

/// 後から追加した列（古い DB にも起動時に追加する）
const ADDED_COLUMNS: &[(&str, &[(&str, &str)])] = &[
    ("tracks", &[("root", "TEXT")]),
    ("track_prefs", &[("itunes_rating", "INTEGER"), ("itunes_plays", "INTEGER"), ("itunes_last_played", "REAL")]),
    (
        "queues",
        &[
            ("kind", "TEXT"),
            ("created", "REAL"),
            ("plays", "INTEGER NOT NULL DEFAULT 0"),
            ("last_played", "REAL"),
            ("exports", "TEXT"),
        ],
    ),
    (
        "queue_items",
        &[("join_next", "INTEGER"), ("join_bars", "INTEGER"), ("join_tpl", "TEXT"), ("join_params", "TEXT")],
    ),
];

fn migrate(c: &Connection) -> Result<()> {
    for (table, cols) in ADDED_COLUMNS {
        let mut st = c.prepare(&format!("PRAGMA table_info({table})"))?;
        let have: HashSet<String> = st.query_map([], |r| r.get::<_, String>(1))?.flatten().collect();
        for (name, typ) in cols.iter() {
            if !have.contains(*name) {
                c.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {typ}"), [])?;
            }
        }
    }
    // タグの文字化けを一度だけ直す（ファイルのタグは書き換えない）
    let done: Option<String> = c
        .query_row("SELECT value FROM settings WHERE key = 'tags_fixed_v1'", [], |r| r.get(0))
        .ok();
    if done.is_none() {
        use crate::library::textfix::fix_mojibake;
        let rows: Vec<(i64, [Option<String>; 4])> = {
            let mut st = c.prepare("SELECT id, title, artist, album, genre FROM tracks")?;
            let it = st.query_map([], |r| Ok((r.get(0)?, [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])))?;
            it.flatten().collect()
        };
        for (id, cols) in rows {
            let fixed: Vec<Option<String>> =
                cols.iter().map(|v| v.as_ref().map(|s| fix_mojibake(s).unwrap_or_else(|| s.clone()))).collect();
            if fixed.iter().zip(cols.iter()).any(|(a, b)| a != b) {
                c.execute(
                    "UPDATE tracks SET title = ?, artist = ?, album = ?, genre = ? WHERE id = ?",
                    rusqlite::params![fixed[0], fixed[1], fixed[2], fixed[3], id],
                )?;
            }
        }
        c.execute("INSERT INTO settings(key, value) VALUES('tags_fixed_v1', 'true')", [])?;
    }
    Ok(())
}

static INITIALIZED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

thread_local! {
    static CONNS: RefCell<HashMap<PathBuf, std::rc::Rc<Connection>>> = RefCell::new(HashMap::new());
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(30))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    let mut init = INITIALIZED.lock();
    let set = init.get_or_insert_with(HashSet::new);
    if !set.contains(path) {
        c.execute_batch(SCHEMA)?;
        migrate(&c)?;
        set.insert(path.to_path_buf());
    }
    Ok(c)
}

/// このスレッドの接続（無ければ開く）。`f` の中だけで使う。
pub fn with<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    with_path(&paths().db_path, f)
}

pub fn with_path<T>(path: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let c = CONNS.with(|m| -> Result<std::rc::Rc<Connection>> {
        let mut m = m.borrow_mut();
        if let Some(c) = m.get(path) {
            return Ok(c.clone());
        }
        let c = std::rc::Rc::new(open(path)?);
        m.insert(path.to_path_buf(), c.clone());
        Ok(c)
    })?;
    f(&c)
}

thread_local! {
    static TX_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// トランザクション（成功したら commit、エラーなら rollback）。入れ子にしたときは外側のトランザクションに含める。
pub fn tx<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    with(|c| {
        let depth = TX_DEPTH.with(|d| d.get());
        if depth > 0 {
            return f(c);
        }
        c.execute_batch("BEGIN IMMEDIATE")?;
        TX_DEPTH.with(|d| d.set(1));
        let r = f(c);
        TX_DEPTH.with(|d| d.set(0));
        match r {
            Ok(v) => {
                c.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = c.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })
}

/// 行を JSON のオブジェクトに（列名 → 値）。Python の dict(row) と同じ。
pub fn row_to_json(r: &rusqlite::Row, names: &[String]) -> rusqlite::Result<serde_json::Map<String, serde_json::Value>> {
    use rusqlite::types::ValueRef;
    let mut m = serde_json::Map::new();
    for (i, n) in names.iter().enumerate() {
        let v = match r.get_ref(i)? {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(x) => serde_json::Value::from(x),
            ValueRef::Real(x) => serde_json::Number::from_f64(x).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
            ValueRef::Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
        };
        m.insert(n.clone(), v);
    }
    Ok(m)
}

/// SELECT の結果を JSON のオブジェクトの列に。
pub fn query_json(
    c: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let mut st = c.prepare(sql)?;
    let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let rows = st.query_map(params, |r| row_to_json(r, &names))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 動的な引数の列を `&dyn ToSql` の列に
pub fn params_of(v: &[rusqlite::types::Value]) -> Vec<&dyn rusqlite::ToSql> {
    v.iter().map(|x| x as &dyn rusqlite::ToSql).collect()
}

/// 解析状態を書き換える（worker・bulk 共通）。cols は追加で書く列。
pub fn set_status(track_id: i64, status: &str, cols: &[(&str, rusqlite::types::Value)]) -> Result<()> {
    let mut sets = vec!["analysis_status = ?".to_string()];
    let mut vals: Vec<rusqlite::types::Value> = vec![status.to_string().into()];
    for (k, v) in cols {
        sets.push(format!("{k} = ?"));
        vals.push(v.clone());
    }
    vals.push(track_id.into());
    tx(|c| {
        c.execute(&format!("UPDATE tracks SET {} WHERE id = ?", sets.join(", ")), rusqlite::params_from_iter(vals.iter()))?;
        Ok(())
    })
}
