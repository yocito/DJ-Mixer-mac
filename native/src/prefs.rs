//! 曲の好み（★）と再生の履歴（djmix/prefs.py）。
use crate::{db, settings, util};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const SOURCES: [&str; 3] = ["deck", "mix", "export"];

// ★・再生回数・最後にかけた日は、アプリの値と iTunes から取り込んだ値を合わせる（djmix/library/query.py）
pub const RATING_IT: &str = "CASE WHEN COALESCE(p.rating, 0) > 0 THEN p.rating ELSE COALESCE(p.itunes_rating, 0) END";
pub const RATING_APP: &str = "COALESCE(p.rating, 0)";
pub const PLAYS: &str = "(COALESCE(p.play_count, 0) + COALESCE(p.itunes_plays, 0))";
pub const LAST: &str = "NULLIF(MAX(COALESCE(p.last_played, 0), COALESCE(p.itunes_last_played, 0)), 0)";

/// ★ の式（iTunes の ★ を使う設定なら、アプリの ★ が無い曲は iTunes の ★）
pub fn rating_expr() -> &'static str {
    let s = settings::load();
    if settings::get_bool(&s, "itunes_use_rating", true) && settings::get_bool(&s, "itunes_enabled", true) {
        RATING_IT
    } else {
        RATING_APP
    }
}

pub fn set_rating(track_id: i64, rating: i64) -> Result<i64> {
    let r = rating.clamp(0, 5);
    db::tx(|c| {
        c.execute(
            "INSERT INTO track_prefs(track_id, rating) VALUES(?, ?) ON CONFLICT(track_id) DO UPDATE SET rating = excluded.rating",
            rusqlite::params![track_id, r],
        )?;
        Ok(())
    })?;
    Ok(r)
}

/// 曲を「かけた」と記録する。同じ曲を同じ source で 10 分以内に記録し直さない。
pub fn record_plays(track_ids: &[i64], source: &str, at: Option<f64>) -> Result<usize> {
    let at = at.unwrap_or_else(util::now);
    let mut seen = HashSet::new();
    let ids: Vec<i64> = track_ids.iter().copied().filter(|t| seen.insert(*t)).collect();
    db::tx(|c| {
        let mut n = 0;
        for tid in ids {
            let dup: Option<i64> = c
                .query_row(
                    "SELECT 1 FROM plays WHERE track_id = ? AND source = ? AND at > ?",
                    rusqlite::params![tid, source, at - 600.0],
                    |r| r.get(0),
                )
                .ok();
            if dup.is_some() {
                continue;
            }
            let exists: Option<i64> = c.query_row("SELECT 1 FROM tracks WHERE id = ?", [tid], |r| r.get(0)).ok();
            if exists.is_none() {
                continue;
            }
            c.execute("INSERT INTO plays(track_id, at, source) VALUES(?, ?, ?)", rusqlite::params![tid, at, source])?;
            c.execute(
                "INSERT INTO track_prefs(track_id, play_count, last_played) VALUES(?, 1, ?) ON CONFLICT(track_id) DO UPDATE SET \
                 play_count = play_count + 1, last_played = MAX(COALESCE(last_played, 0), excluded.last_played)",
                rusqlite::params![tid, at],
            )?;
            n += 1;
        }
        Ok(n)
    })
}

/// 曲の ★（アプリの ★ があればそれ、無ければ iTunes の ★）。★ のある曲だけ。
pub fn ratings() -> HashMap<i64, i64> {
    let expr = rating_expr();
    db::with(|c| {
        let mut st = c.prepare(&format!("SELECT p.track_id, {expr} r FROM track_prefs p WHERE r > 0"))?;
        let m = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?.flatten().collect();
        Ok(m)
    })
    .unwrap_or_default()
}

/// 再生回数（アプリ＋iTunes）。
pub fn play_counts() -> HashMap<i64, i64> {
    db::with(|c| {
        let mut st = c.prepare(
            "SELECT track_id, COALESCE(play_count, 0) + COALESCE(itunes_plays, 0) n FROM track_prefs WHERE n > 0",
        )?;
        let m = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?.flatten().collect();
        Ok(m)
    })
    .unwrap_or_default()
}

/// 最近 days 日にかけた曲（アプリの履歴と、iTunes で最後に再生した日）。
pub fn recent(days: f64) -> HashSet<i64> {
    let since = util::now() - days * 86400.0;
    let it = settings::get_bool(&settings::load(), "itunes_enabled", true);
    db::with(|c| {
        let mut out: HashSet<i64> = HashSet::new();
        let mut st = c.prepare("SELECT DISTINCT track_id FROM plays WHERE at >= ?")?;
        out.extend(st.query_map([since], |r| r.get::<_, i64>(0))?.flatten());
        if it {
            let mut st = c.prepare("SELECT track_id FROM track_prefs WHERE itunes_last_played >= ?")?;
            out.extend(st.query_map([since], |r| r.get::<_, i64>(0))?.flatten());
        }
        Ok(out)
    })
    .unwrap_or_default()
}

pub fn history(limit: i64) -> Result<Vec<Value>> {
    db::with(|c| {
        let mut st = c.prepare(
            "SELECT p.track_id, p.at, p.source, t.title, t.artist FROM plays p JOIN tracks t ON t.id = p.track_id \
             ORDER BY p.at DESC LIMIT ?",
        )?;
        let rows = st
            .query_map([limit], |r| {
                Ok(json!({"track_id": r.get::<_, i64>(0)?, "at": r.get::<_, f64>(1)?, "source": r.get::<_, String>(2)?,
                          "title": r.get::<_, Option<String>>(3)?, "artist": r.get::<_, Option<String>>(4)?}))
            })?
            .flatten()
            .collect();
        Ok(rows)
    })
}
