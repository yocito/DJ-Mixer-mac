//! iTunes（ミュージック.app）の情報の取り込み（djmix/library/itunes.py）。
//!
//! 「ライブラリ XML」を**読むだけ**で、★・再生回数・最後に再生した日・プレイリストを取り込む。
use crate::db;
use std::collections::HashSet;

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
