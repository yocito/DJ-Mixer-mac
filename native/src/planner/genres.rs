//! ジャンルのまとまり（djmix/planner/genres.py）。タグの表記の揺れ（Jazz / Fusion / Jazz & Fusion / Smooth Jazz …）をまとめる。
use once_cell::sync::Lazy;
use regex::Regex;

const GROUPS: &[(&str, &str)] = &[
    ("ジャズ・フュージョン", r"(?i)jazz|fusion|bossa|swing|bebop"),
    ("R&B・ソウル・ファンク", r"(?i)r&b|rnb|soul|funk"),
    ("J-POP・歌謡", r"(?i)j-?pop|domestic|歌謡|kayo|japanese|邦楽"),
    ("ポップス", r"(?i)pop|adult contemporary|easy listening"),
    ("ロック", r"(?i)rock|metal|punk|alternative|blues"),
    ("ダンス・エレクトロ", r"(?i)dance|house|techno|electro|trance|club"),
    ("サントラ・ゲーム・アニメ", r"(?i)soundtrack|game|anime|ost|サントラ"),
    ("クラシック", r"(?i)classical|orchestra|クラシック"),
];
pub const OTHER: &str = "その他";

static RX: Lazy<Vec<(&'static str, Regex)>> =
    Lazy::new(|| GROUPS.iter().map(|(n, r)| (*n, Regex::new(r).unwrap())).collect());

pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = GROUPS.iter().map(|(n, _)| *n).collect();
    v.push(OTHER);
    v
}

pub fn group_of(genre: Option<&str>) -> &'static str {
    if let Some(g) = genre {
        if !g.is_empty() {
            for (name, rx) in RX.iter() {
                if rx.is_match(g) {
                    return name;
                }
            }
        }
    }
    OTHER
}

#[cfg(test)]
mod tests {
    #[test]
    fn groups() {
        use super::group_of;
        assert_eq!(group_of(Some("Smooth Jazz")), "ジャズ・フュージョン");
        assert_eq!(group_of(Some("J-Pop")), "J-POP・歌謡");
        assert_eq!(group_of(Some("Pop")), "ポップス");
        assert_eq!(group_of(None), "その他");
        assert_eq!(group_of(Some("")), "その他");
    }
}
