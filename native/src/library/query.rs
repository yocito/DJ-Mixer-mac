//! ライブラリの検索・一覧（djmix/library/query.py）。
use super::roots::where_sql;
use crate::db;
use crate::planner::genres::group_of;
use crate::prefs::{rating_expr, LAST, PLAYS, RATING_IT};
use anyhow::Result;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub fn track_cols() -> String {
    let r = rating_expr();
    format!(
        "t.id, t.title, t.artist, t.album, t.genre, t.year, t.duration, t.folder, t.analysis_status, \
         t.bpm, t.camelot, t.lufs, t.beat_class, {r} AS rating, COALESCE(p.rating, 0) AS rating_app, \
         {} AS rating_it, \
         CASE WHEN COALESCE(p.rating, 0) > 0 THEN 'app' WHEN ({r}) > 0 THEN 'itunes' ELSE NULL END AS rating_src, \
         {PLAYS} AS plays, {LAST} AS last_played, \
         CASE WHEN COALESCE(p.itunes_last_played, 0) > COALESCE(p.last_played, 0) THEN 1 ELSE 0 END AS last_itunes",
        if r == RATING_IT { "COALESCE(p.itunes_rating, 0)" } else { "0" }
    )
}

pub const FROM: &str = "tracks t LEFT JOIN track_prefs p ON p.track_id = t.id";

fn sort_expr(sort: &str) -> String {
    match sort {
        "title" => "t.title COLLATE NOCASE".into(),
        "artist" => "t.artist COLLATE NOCASE".into(),
        "album" => "t.album COLLATE NOCASE".into(),
        "duration" => "t.duration".into(),
        "bpm" => "t.bpm".into(),
        "rating" => format!("NULLIF({}, 0)", rating_expr()),
        "last_played" => LAST.into(),
        "plays" => format!("NULLIF({PLAYS}, 0)"),
        "camelot" => "CAST(substr(t.camelot, 1, length(t.camelot) - 1) AS INTEGER) * 2 + (substr(t.camelot, -1) = 'B')".into(),
        _ => "t.path".into(),
    }
}

pub fn order_by(sort: &str, desc: bool, playlist: bool) -> String {
    let dir = if desc { "DESC" } else { "ASC" };
    if playlist && (sort == "path" || sort == "pos") {
        return format!("pl.pos {dir}");
    }
    let e = sort_expr(sort);
    let tie = if sort == "artist" { ", t.album COLLATE NOCASE, t.path" } else { ", t.path" };
    format!("({e}) IS NULL, {e} {dir}{tie}")
}

#[derive(Default, Debug, Clone)]
pub struct SearchOpts {
    pub q: String,
    pub folder: Option<String>,
    pub analyzed: Option<bool>,
    pub sort: String,
    pub offset: i64,
    pub limit: i64,
    pub desc: bool,
    pub root: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub playlist: Option<i64>,
    pub artist_q: String,
    pub album_q: String,
    pub bpm_min: Option<f64>,
    pub bpm_max: Option<f64>,
    pub keys: Vec<String>,
    pub groups: Vec<String>,
    pub rating_min: Option<i64>,
}

pub fn search(o: &SearchOpts) -> Result<Value> {
    let (rw, rargs) = where_sql("t");
    let mut wh: Vec<String> = vec![rw];
    let mut args: Vec<SqlValue> = rargs;
    for word in o.q.split_whitespace() {
        wh.push("(t.title LIKE ? OR t.artist LIKE ? OR t.album LIKE ? OR t.folder LIKE ?)".into());
        for _ in 0..4 {
            args.push(SqlValue::Text(format!("%{word}%")));
        }
    }
    if let Some(r) = &o.root {
        wh.push("t.root = ?".into());
        args.push(SqlValue::Text(r.clone()));
    }
    if let Some(f) = &o.folder {
        wh.push("t.folder = ?".into());
        args.push(SqlValue::Text(f.clone()));
    }
    for (col, v) in [("artist", &o.artist), ("album", &o.album)] {
        if let Some(v) = v {
            wh.push(format!("COALESCE(t.{col}, '') = ?"));
            args.push(SqlValue::Text(v.clone()));
        }
    }
    for (col, v) in [("artist", &o.artist_q), ("album", &o.album_q)] {
        for word in v.split_whitespace() {
            wh.push(format!("t.{col} LIKE ?"));
            args.push(SqlValue::Text(format!("%{word}%")));
        }
    }
    if let Some(b) = o.bpm_min {
        wh.push("t.bpm >= ?".into());
        args.push(SqlValue::Real(b));
    }
    if let Some(b) = o.bpm_max {
        wh.push("t.bpm <= ?".into());
        args.push(SqlValue::Real(b));
    }
    if !o.keys.is_empty() {
        wh.push(format!("t.camelot IN ({})", vec!["?"; o.keys.len()].join(",")));
        args.extend(o.keys.iter().map(|k| SqlValue::Text(k.clone())));
    }
    if !o.groups.is_empty() {
        let genres: Vec<String> = db::with(|c| {
            let mut st = c.prepare("SELECT DISTINCT COALESCE(genre, '') FROM tracks")?;
            let v = st.query_map([], |r| r.get::<_, String>(0))?.flatten().collect();
            Ok(v)
        })?;
        let gs: Vec<String> = genres
            .into_iter()
            .filter(|g| {
                let grp = group_of(if g.is_empty() { None } else { Some(g.as_str()) });
                o.groups.iter().any(|x| x == grp)
            })
            .collect();
        if gs.is_empty() {
            wh.push("0".into());
        } else {
            wh.push(format!("COALESCE(t.genre, '') IN ({})", vec!["?"; gs.len()].join(",")));
            args.extend(gs.into_iter().map(SqlValue::Text));
        }
    }
    if let Some(r) = o.rating_min.filter(|r| *r != 0) {
        wh.push(format!("({}) >= ?", rating_expr()));
        args.push(SqlValue::Integer(r));
    }
    match o.analyzed {
        Some(true) => wh.push("t.analysis_status = 'done'".into()),
        Some(false) => wh.push("t.analysis_status != 'done'".into()),
        None => {}
    }
    let mut frm = FROM.to_string();
    if let Some(pl) = o.playlist {
        frm += " JOIN itunes_playlist_items pl ON pl.track_id = t.id AND pl.playlist_id = ?";
        args.insert(0, SqlValue::Integer(pl));
    }
    let w = format!("WHERE {}", wh.join(" AND "));
    let sort = if o.sort.is_empty() { "path" } else { o.sort.as_str() };
    db::with(|c| {
        let total: i64 =
            c.query_row(&format!("SELECT COUNT(*) FROM {frm} {w}"), rusqlite::params_from_iter(args.iter()), |r| r.get(0))?;
        let mut a2 = args.clone();
        a2.push(SqlValue::Integer(o.limit));
        a2.push(SqlValue::Integer(o.offset));
        let sql = format!(
            "SELECT {}{} FROM {frm} {w} ORDER BY {} LIMIT ? OFFSET ?",
            track_cols(),
            if o.playlist.is_some() { ", pl.pos AS pl_pos" } else { "" },
            order_by(sort, o.desc, o.playlist.is_some())
        );
        let items = db::query_json(c, &sql, &db::params_of(&a2))?;
        Ok(json!({"total": total, "offset": o.offset, "items": items}))
    })
}

/// 音楽フォルダごとの、直下のフォルダと曲数。
pub fn folders() -> Result<Value> {
    let (rw, rargs) = where_sql("");
    db::with(|c| {
        let rows = db::query_json(
            c,
            &format!("SELECT root, folder, COUNT(*) AS count FROM tracks WHERE {rw} GROUP BY root, folder ORDER BY root, folder COLLATE NOCASE"),
            &db::params_of(&rargs),
        )?;
        Ok(Value::Array(rows.into_iter().map(Value::Object).collect()))
    })
}

/// アーティスト／アルバムの一覧と曲数。アルバムは artist で絞れる。
pub fn facets(kind: &str, artist: Option<&str>) -> Result<Value> {
    let (rw, rargs) = where_sql("");
    db::with(|c| {
        if kind == "artist" {
            let rows = db::query_json(
                c,
                &format!("SELECT COALESCE(artist, '') AS name, COUNT(*) AS count FROM tracks WHERE {rw} GROUP BY name ORDER BY name = '', name COLLATE NOCASE"),
                &db::params_of(&rargs),
            )?;
            return Ok(Value::Array(rows.into_iter().map(Value::Object).collect()));
        }
        let mut w = rw.clone();
        let mut args = rargs.clone();
        if let Some(a) = artist {
            w += " AND COALESCE(artist, '') = ?";
            args.push(SqlValue::Text(a.to_string()));
        }
        let rows = db::query_json(
            c,
            &format!(
                "SELECT COALESCE(album, '') AS name, COUNT(*) AS count, COUNT(DISTINCT COALESCE(artist, '')) AS artists, \
                 MIN(COALESCE(artist, '')) AS artist FROM tracks WHERE {w} GROUP BY name ORDER BY name = '', name COLLATE NOCASE"
            ),
            &db::params_of(&args),
        )?;
        Ok(Value::Array(rows.into_iter().map(Value::Object).collect()))
    })
}

/// おまかせの「アルバムで選ぶ」: アルバム（アルバム名で区別）ごとの曲数・解析済みの曲数・代表のアーティスト。
pub fn album_list(q: &str) -> Result<Value> {
    let (rw, rargs) = where_sql("");
    let mut w = format!("{rw} AND COALESCE(album, '') <> ''");
    let mut args = rargs;
    if !q.is_empty() {
        w += " AND (album LIKE ? OR artist LIKE ?)";
        args.push(SqlValue::Text(format!("%{q}%")));
        args.push(SqlValue::Text(format!("%{q}%")));
    }
    args.push(SqlValue::Integer(2000));
    db::with(|c| {
        let mut st = c.prepare(&format!(
            "SELECT album, COUNT(*), SUM(analysis_status = 'done'), COUNT(DISTINCT COALESCE(artist, '')), MIN(year), \
             (SELECT artist FROM tracks t2 WHERE t2.album = tracks.album GROUP BY artist ORDER BY COUNT(*) DESC LIMIT 1) \
             FROM tracks WHERE {w} GROUP BY album ORDER BY album COLLATE NOCASE LIMIT ?"
        ))?;
        let rows = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(json!({"name": r.get::<_, Option<String>>(0)?, "count": r.get::<_, i64>(1)?,
                          "analyzed": r.get::<_, Option<i64>>(2)?.unwrap_or(0), "artists": r.get::<_, i64>(3)?,
                          "year": r.get::<_, Option<String>>(4)?, "artist": r.get::<_, Option<String>>(5)?.unwrap_or_default()}))
            })?
            .flatten()
            .collect();
        Ok(Value::Array(rows))
    })
}

/// 指定したアルバムの解析済みの曲（起点の曲を選ぶ一覧に使う）。
pub fn album_tracks(albums: &[String]) -> Result<Value> {
    if albums.is_empty() {
        return Ok(json!([]));
    }
    let (rw, rargs) = where_sql("t");
    let mut args: Vec<SqlValue> = albums.iter().map(|a| SqlValue::Text(a.clone())).collect();
    args.extend(rargs);
    db::with(|c| {
        let rows = db::query_json(
            c,
            &format!(
                "SELECT t.id, t.title, t.artist, t.album, t.bpm, t.camelot, t.duration FROM tracks t \
                 WHERE t.album IN ({}) AND t.analysis_status = 'done' AND {rw} ORDER BY t.album COLLATE NOCASE, t.path",
                vec!["?"; albums.len()].join(",")
            ),
            &db::params_of(&args),
        )?;
        Ok(Value::Array(rows.into_iter().map(Value::Object).collect()))
    })
}

pub fn counts() -> Result<Value> {
    let (rw, rargs) = where_sql("");
    db::with(|c| {
        let (all, analyzed): (i64, Option<i64>) = c.query_row(
            &format!("SELECT COUNT(*), SUM(analysis_status='done') FROM tracks WHERE {rw}"),
            rusqlite::params_from_iter(rargs.iter()),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(json!({"all": all, "analyzed": analyzed.unwrap_or(0)}))
    })
}

/// 曲の情報（id → 曲の行）。
pub fn get_tracks(ids: &[i64]) -> Result<HashMap<i64, Map<String, Value>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::new();
    db::with(|c| {
        for chunk in ids.chunks(500) {
            let sql = format!("SELECT {} FROM {FROM} WHERE t.id IN ({})", track_cols(), vec!["?"; chunk.len()].join(","));
            let args: Vec<SqlValue> = chunk.iter().map(|&i| SqlValue::Integer(i)).collect();
            for r in db::query_json(c, &sql, &db::params_of(&args))? {
                if let Some(id) = r.get("id").and_then(|v| v.as_i64()) {
                    out.insert(id, r);
                }
            }
        }
        Ok(())
    })?;
    Ok(out)
}

pub fn track_path(id: i64) -> Option<String> {
    db::with(|c| Ok(c.query_row("SELECT path FROM tracks WHERE id = ?", [id], |r| r.get::<_, String>(0)).ok())).ok().flatten()
}
