//! 音楽フォルダ（複数。djmix/library/roots.py）。
//!
//! 曲には、どの音楽フォルダの曲かを tracks.root に記録する。設定から外した音楽フォルダの曲は**消さずに隠す**
//! （ライブラリ・おまかせ・まとめて解析の対象から外す）。戻せば ★・履歴・解析結果ごと元どおりに見える。
use crate::{db, settings};
use anyhow::Result;
use rusqlite::types::Value as SqlValue;
use std::path::{Component, Path, PathBuf};

/// os.path.normpath に近い正規化（. と .. を畳み、末尾の / を取る）。
pub fn normpath(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() {
        return String::new();
    }
    let path = Path::new(p);
    let mut out: Vec<Component> = vec![];
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    let pb: PathBuf = out.iter().collect();
    let s = pb.to_string_lossy().into_owned();
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

/// 比べるための形（Mac のファイルシステムは大文字小文字を区別しないことが多いが、パスの記録は区別して扱う）。
pub fn norm(p: &str) -> String {
    normpath(p)
}

/// 設定の音楽フォルダ（music_roots。無ければ music_root 1 つ）。
pub fn configured() -> Vec<String> {
    let s = settings::load();
    let mut roots: Vec<String> = s
        .get("music_roots")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    if roots.is_empty() {
        let mr = settings::get_str(&s, "music_root");
        if !mr.trim().is_empty() {
            roots.push(mr.to_string());
        }
    }
    let mut out: Vec<String> = vec![];
    let mut seen = std::collections::HashSet::new();
    for r in roots {
        let k = norm(&r);
        if !k.is_empty() && seen.insert(k) {
            out.push(normpath(&r));
        }
    }
    out
}

/// いま使う音楽フォルダの曲だけに絞る WHERE の部分と引数。alias は表の別名（"t" など。空なら付けない）。
pub fn where_sql(alias: &str) -> (String, Vec<SqlValue>) {
    let rs = configured();
    let col = if alias.is_empty() { "root".to_string() } else { format!("{alias}.root") };
    // root の無い曲（どの音楽フォルダの曲か分からないもの）は隠さない
    if rs.is_empty() {
        return (format!("{col} IS NULL"), vec![]);
    }
    let marks = vec!["?"; rs.len()].join(",");
    (format!("({col} IS NULL OR {col} IN ({marks}))"), rs.into_iter().map(SqlValue::Text).collect())
}

/// root の入っていない曲（音楽フォルダを複数にする前の曲）に、パスから音楽フォルダを入れる。
pub fn backfill() -> Result<usize> {
    let rs = configured();
    let s = settings::load();
    let mut cands = rs.clone();
    let mr = settings::get_str(&s, "music_root").to_string();
    if !mr.is_empty() && !rs.contains(&mr) {
        cands.push(mr);
    }
    db::tx(|c| {
        let mut n = 0;
        for r in &cands {
            let base = normpath(r);
            let prefix = format!("{}/", base.trim_end_matches(['/', '\\']));
            let like = prefix.replace('!', "!!").replace('%', "!%").replace('_', "!_") + "%";
            n += c.execute(
                "UPDATE tracks SET root = ? WHERE root IS NULL AND (path LIKE ? ESCAPE '!' OR path = ?)",
                rusqlite::params![base, like, r],
            )?;
        }
        Ok(n)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn normpath() {
        assert_eq!(super::normpath("/a/b/../c/"), "/a/c");
        assert_eq!(super::normpath(" /Music/./x "), "/Music/x");
    }
}
