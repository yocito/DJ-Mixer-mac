//! iTunes（Mac ではミュージック.app）の情報の取り込み（djmix/library/itunes.py）。
//!
//! 「ライブラリ XML」を**読むだけ**で、★・再生回数・最後に再生した日・プレイリストを取り込む。
//! ミュージック.app 側の情報も、音楽ファイルのタグも変えない。曲はファイルのパスで対応づける。
//! 前提: ミュージック.app の「設定」→「ファイル」（古い版は「詳細」）→「ライブラリ XML をほかのアプリケーションと共有」。
use crate::{db, settings, util};
use anyhow::Result;
use parking_lot::Mutex;
use plist::Value as P;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

static LOCK: Mutex<()> = Mutex::new(());

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// 既定の場所（ミュージック.app は ~/Music/Music、古い iTunes は ~/Music/iTunes）
pub fn default_dir() -> PathBuf {
    let m = home().join("Music").join("Music");
    if m.exists() {
        m
    } else {
        home().join("Music").join("iTunes")
    }
}

fn candidates() -> Vec<PathBuf> {
    let h = home().join("Music");
    vec![
        h.join("Music").join("Library.xml"),
        h.join("Music").join("Music Library.xml"),
        h.join("iTunes").join("iTunes Music Library.xml"),
        h.join("iTunes").join("iTunes Library.xml"),
    ]
}

/// 設定の XML、無ければ既定の場所で見つかったもの。
pub fn xml_path() -> Option<PathBuf> {
    let s = settings::load();
    let p = settings::get_str(&s, "itunes_xml").trim().to_string();
    if !p.is_empty() {
        return Some(PathBuf::from(p));
    }
    candidates().into_iter().find(|c| c.exists())
}

/// file:///Users/me/Music/a%20b.m4a → /Users/me/Music/a b.m4a（file://localhost/C:/… の Windows 形式も読める）
pub fn location_to_path(loc: &str) -> Option<String> {
    let rest = loc.strip_prefix("file://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => return None,
    };
    let mut p = percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned();
    if !host.is_empty() && host != "localhost" {
        p = format!("//{host}{p}");
    } else if p.len() >= 3 && p.as_bytes()[0] == b'/' && p.as_bytes()[2] == b':' {
        p = p[1..].to_string();
    }
    Some(super::roots::normpath(&p))
}

/// 対応づけに使う形（Unicode の正規化 NFC・大文字小文字を区別しない。Mac のファイル名は NFD で返ることがある）
pub fn key(p: &str) -> String {
    super::roots::normpath(p).nfc().collect::<String>().to_lowercase()
}

struct ItTrack {
    path: String,
    rating: i64,
    plays: i64,
    last: Option<f64>,
}

struct ItPlaylist {
    pid: String,
    name: String,
    parent: Option<String>,
    folder: bool,
    smart: bool,
    sort: i64,
    items: Vec<i64>,
}

fn int(v: Option<&P>) -> i64 {
    match v {
        Some(P::Integer(i)) => i.as_signed().unwrap_or(0),
        Some(P::Real(r)) => *r as i64,
        Some(P::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn boolean(v: Option<&P>) -> bool {
    matches!(v, Some(P::Boolean(true)))
}

fn date(v: Option<&P>) -> Option<f64> {
    match v {
        Some(P::Date(d)) => {
            let st: std::time::SystemTime = (*d).into();
            st.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
        }
        _ => None,
    }
}

fn parse(path: &Path) -> Result<(HashMap<i64, ItTrack>, Vec<ItPlaylist>)> {
    let pl = P::from_file(path)?;
    let d = pl.as_dictionary().ok_or_else(|| anyhow::anyhow!("not a dictionary"))?;
    let mut tracks = HashMap::new();
    if let Some(ts) = d.get("Tracks").and_then(|v| v.as_dictionary()) {
        for (tid, t) in ts {
            let Some(t) = t.as_dictionary() else { continue };
            let Some(p) = t.get("Location").and_then(|v| v.as_string()).and_then(location_to_path) else { continue };
            let rating = if boolean(t.get("Rating Computed")) { 0 } else { int(t.get("Rating")) / 20 };
            let id = t.get("Track ID").map(|v| int(Some(v))).unwrap_or_else(|| tid.parse().unwrap_or(0));
            tracks.insert(
                id,
                ItTrack { path: p, rating: rating.clamp(0, 5), plays: int(t.get("Play Count")), last: date(t.get("Play Date UTC")) },
            );
        }
    }
    let mut playlists = vec![];
    if let Some(ps) = d.get("Playlists").and_then(|v| v.as_array()) {
        for (i, p) in ps.iter().enumerate() {
            let Some(p) = p.as_dictionary() else { continue };
            if boolean(p.get("Master")) || p.get("Distinguished Kind").is_some() || matches!(p.get("Visible"), Some(P::Boolean(false))) {
                continue;
            }
            let folder = boolean(p.get("Folder"));
            playlists.push(ItPlaylist {
                pid: p.get("Playlist Persistent ID").and_then(|v| v.as_string()).map(String::from).unwrap_or_else(|| format!("#{i}")),
                name: p.get("Name").and_then(|v| v.as_string()).filter(|s| !s.is_empty()).unwrap_or("（名前なし）").to_string(),
                parent: p.get("Parent Persistent ID").and_then(|v| v.as_string()).map(String::from),
                folder,
                smart: p.contains_key("Smart Info") && !folder,
                sort: i as i64,
                items: p
                    .get("Playlist Items")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_dictionary().and_then(|x| x.get("Track ID")).map(|v| int(Some(v))))
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    Ok((tracks, playlists))
}

fn mtime(p: &Path) -> Option<f64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

/// 読み込んで DB に入れる。戻り値（と設定 itunes_status）: 件数・時刻・エラー。
pub fn import_xml(path: Option<PathBuf>) -> Result<Value> {
    let _g = LOCK.lock();
    let path = path.or_else(xml_path);
    let mut st = Map::new();
    st.insert("path".into(), json!(path.as_ref().map(|p| p.to_string_lossy().into_owned())));
    st.insert("at".into(), json!(util::now()));
    st.insert("error".into(), Value::Null);
    let Some(path) = path.filter(|p| p.exists()) else {
        st.insert("error".into(), json!("not_found"));
        settings::save_one("itunes_status", Value::Object(st.clone()))?;
        return Ok(Value::Object(st));
    };
    let t0 = util::now();
    let (tracks, playlists) = match parse(&path) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("itunes xml read failed: {e}");
            st.insert("error".into(), json!(util::truncate_chars(&format!("XML を読めませんでした: {e}"), 300)));
            settings::save_one("itunes_status", Value::Object(st.clone()))?;
            return Ok(Value::Object(st));
        }
    };
    let by_path: HashMap<String, i64> = db::with(|c| {
        let mut s = c.prepare("SELECT id, path FROM tracks")?;
        let m = s.query_map([], |r| Ok((key(&r.get::<_, String>(1)?), r.get::<_, i64>(0)?)))?.flatten().collect();
        Ok(m)
    })?;
    let idmap: HashMap<i64, i64> =
        tracks.iter().filter_map(|(itid, t)| by_path.get(&key(&t.path)).map(|aid| (*itid, *aid))).collect();
    let rows: Vec<(i64, Option<i64>, Option<i64>, Option<f64>)> = idmap
        .iter()
        .map(|(itid, aid)| {
            let t = &tracks[itid];
            (*aid, Some(t.rating).filter(|x| *x > 0), Some(t.plays).filter(|x| *x > 0), t.last)
        })
        .filter(|r| r.1.is_some() || r.2.is_some() || r.3.is_some())
        .collect();
    db::tx(|c| {
        c.execute("UPDATE track_prefs SET itunes_rating = NULL, itunes_plays = NULL, itunes_last_played = NULL", [])?;
        for r in &rows {
            c.execute(
                "INSERT INTO track_prefs(track_id, itunes_rating, itunes_plays, itunes_last_played) VALUES(?, ?, ?, ?) \
                 ON CONFLICT(track_id) DO UPDATE SET itunes_rating = excluded.itunes_rating, \
                 itunes_plays = excluded.itunes_plays, itunes_last_played = excluded.itunes_last_played",
                rusqlite::params![r.0, r.1, r.2, r.3],
            )?;
        }
        c.execute("DELETE FROM itunes_playlist_items", [])?;
        c.execute("DELETE FROM itunes_playlists", [])?;
        for p in &playlists {
            c.execute(
                "INSERT INTO itunes_playlists(pid, name, parent_pid, is_folder, is_smart, sort) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![p.pid, p.name, p.parent, p.folder as i64, p.smart as i64, p.sort],
            )?;
            let id = c.last_insert_rowid();
            if !p.folder {
                for (pos, x) in p.items.iter().filter_map(|x| idmap.get(x)).enumerate() {
                    c.execute(
                        "INSERT INTO itunes_playlist_items(playlist_id, pos, track_id) VALUES(?, ?, ?)",
                        rusqlite::params![id, pos as i64, x],
                    )?;
                }
            }
        }
        Ok(())
    })?;
    st.insert("mtime".into(), json!(mtime(&path)));
    st.insert("tracks".into(), json!(tracks.len()));
    st.insert("matched".into(), json!(idmap.len()));
    st.insert("rated".into(), json!(rows.iter().filter(|r| r.1.is_some()).count()));
    st.insert("played".into(), json!(rows.iter().filter(|r| r.2.is_some()).count()));
    st.insert("playlists".into(), json!(playlists.iter().filter(|p| !p.folder).count()));
    st.insert("sec".into(), json!(util::round(util::now() - t0, 2)));
    settings::save_one("itunes_status", Value::Object(st.clone()))?;
    log::info!("itunes imported: {} tracks, {} matched", tracks.len(), idmap.len());
    Ok(Value::Object(st))
}

/// 連携をやめる: iTunes から取り込んだ値とプレイリストを消す（アプリの ★・履歴はそのまま）。
pub fn unlink() -> Result<()> {
    let _g = LOCK.lock();
    db::tx(|c| {
        c.execute("UPDATE track_prefs SET itunes_rating = NULL, itunes_plays = NULL, itunes_last_played = NULL", [])?;
        c.execute("DELETE FROM itunes_playlist_items", [])?;
        c.execute("DELETE FROM itunes_playlists", [])?;
        Ok(())
    })?;
    let mut m = Map::new();
    m.insert("itunes_enabled".into(), json!(false));
    m.insert("itunes_status".into(), json!({}));
    settings::save(&m)?;
    Ok(())
}

pub fn status() -> Value {
    let s = settings::load();
    let p = xml_path();
    json!({"enabled": settings::get_bool(&s, "itunes_enabled", true), "use_rating": settings::get_bool(&s, "itunes_use_rating", true),
           "xml": p.as_ref().map(|p| p.to_string_lossy().into_owned()), "exists": p.as_ref().map(|p| p.exists()).unwrap_or(false),
           "default_dir": default_dir().to_string_lossy(), "last": s.get("itunes_status").cloned().unwrap_or(json!({}))})
}

/// XML が前回の読み込みから変わっていれば読み直す（連携が有効なときだけ）。
pub fn maybe_reimport() -> Result<bool> {
    let s = settings::load();
    if !settings::get_bool(&s, "itunes_enabled", true) {
        return Ok(false);
    }
    let last = s.get("itunes_status").cloned().unwrap_or(json!({}));
    let Some(p) = xml_path().filter(|p| p.exists()) else {
        if last.get("error").and_then(|v| v.as_str()) != Some("not_found") {
            import_xml(None)?; // 「見つからない」を記録しておく（画面の案内に使う）
        }
        return Ok(false);
    };
    let same = last.get("mtime").and_then(|v| v.as_f64()) == mtime(&p)
        && last.get("path").and_then(|v| v.as_str()) == Some(p.to_string_lossy().as_ref())
        && last.get("error").map(|v| v.is_null()).unwrap_or(true);
    if same {
        return Ok(false);
    }
    import_xml(Some(p))?;
    Ok(true)
}

/// 起動時と、interval 秒ごとに XML の更新を見る（裏のスレッド）。
pub fn start_watch(interval_sec: u64) {
    std::thread::Builder::new()
        .name("itunes-watch".into())
        .spawn(move || loop {
            if let Err(e) = maybe_reimport() {
                log::error!("itunes watch: {e:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(interval_sec));
        })
        .ok();
}

/// プレイリストの一覧（フォルダの形。曲数と解析済みの数は設定の音楽フォルダの曲だけ数える）。
pub fn playlists() -> Result<Value> {
    let (rw, rargs) = super::roots::where_sql("t");
    db::with(|c| {
        let mut cnt: HashMap<i64, (i64, i64)> = HashMap::new();
        {
            let mut st = c.prepare(&format!(
                "SELECT i.playlist_id, COUNT(*), SUM(t.analysis_status = 'done') FROM itunes_playlist_items i JOIN tracks t ON t.id = i.track_id \
                 WHERE {rw} GROUP BY i.playlist_id"
            ))?;
            for r in st
                .query_map(rusqlite::params_from_iter(rargs.iter()), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0)))
                })?
                .flatten()
            {
                cnt.insert(r.0, (r.1, r.2));
            }
        }
        let mut st = c.prepare("SELECT id, pid, name, parent_pid, is_folder, is_smart FROM itunes_playlists ORDER BY sort")?;
        let rows = st
            .query_map([], |r| {
                let id: i64 = r.get(0)?;
                let (n, a) = cnt.get(&id).copied().unwrap_or((0, 0));
                Ok(json!({"id": id, "pid": r.get::<_, Option<String>>(1)?, "name": r.get::<_, String>(2)?,
                          "parent": r.get::<_, Option<String>>(3)?, "folder": r.get::<_, i64>(4)? != 0,
                          "smart": r.get::<_, i64>(5)? != 0, "count": n, "analyzed": a}))
            })?
            .flatten()
            .collect();
        Ok(Value::Array(rows))
    })
}

/// 指定したプレイリストに入っている曲。
pub fn playlist_tracks(ids: &[i64]) -> HashSet<i64> {
    if ids.is_empty() {
        return HashSet::new();
    }
    let marks = vec!["?"; ids.len()].join(",");
    db::with(|c| {
        let mut st = c.prepare(&format!("SELECT DISTINCT track_id FROM itunes_playlist_items WHERE playlist_id IN ({marks})"))?;
        let s = st.query_map(rusqlite::params_from_iter(ids.iter()), |r| r.get::<_, i64>(0))?.flatten().collect();
        Ok(s)
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn locations() {
        assert_eq!(super::location_to_path("file:///Users/me/Music/a%20b.m4a").as_deref(), Some("/Users/me/Music/a b.m4a"));
        assert_eq!(super::location_to_path("file://localhost/C:/Music/x.mp3").as_deref(), Some("C:/Music/x.mp3"));
        assert_eq!(super::location_to_path("http://x"), None);
    }
}
