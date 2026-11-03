//! タグの文字化けの修正（djmix/library/textfix.py）。
//!
//! 古い形式のタグ（ID3v1・一部の ID3v2）の Shift-JIS が Latin-1 として読まれると「\x88À\x93¡…」のように化ける。
//! Latin-1 に戻して CP932 として読み直す。正しいアクセント付きの名前（Céline・Björk など）を壊さないよう、
//! (1) U+0080〜U+00FF の文字が 2 つ以上連続している、(2) 読み直した結果にかな・漢字があり、私用領域・制御文字が無い、ときだけ直す。

fn is_high(c: char) -> bool {
    ('\u{80}'..='\u{ff}').contains(&c)
}

fn is_ja(c: char) -> bool {
    ('\u{3040}'..='\u{30ff}').contains(&c) || ('\u{3400}'..='\u{9fff}').contains(&c) || ('\u{ff66}'..='\u{ff9f}').contains(&c)
}

fn is_bad(c: char) -> bool {
    ('\u{e000}'..='\u{f8ff}').contains(&c)
        || ('\u{00}'..='\u{1f}').contains(&c)
        || ('\u{7f}'..='\u{9f}').contains(&c)
        || c == '\u{fffd}'
}

pub fn fix_mojibake(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let pair = chars.windows(2).any(|w| is_high(w[0]) && is_high(w[1]));
    if !pair || chars.iter().any(|&c| is_ja(c)) {
        return None;
    }
    // Latin-1 に戻す（U+0100 以上があれば Latin-1 ではないので直さない）
    let mut bytes = Vec::with_capacity(chars.len());
    for c in &chars {
        let u = *c as u32;
        if u > 0xff {
            return None;
        }
        bytes.push(u as u8);
    }
    let (fixed, _, had_errors) = encoding_rs::SHIFT_JIS.decode(&bytes);
    if had_errors {
        return None;
    }
    if !fixed.chars().any(is_ja) || fixed.chars().any(is_bad) {
        return None;
    }
    Some(fixed.into_owned())
}

/// 直せたら直した文字列、そうでなければそのまま。
pub fn fix(s: Option<String>) -> Option<String> {
    match s {
        Some(v) => Some(fix_mojibake(&v).unwrap_or(v)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sjis_as_latin1(s: &str) -> String {
        let (b, _, _) = encoding_rs::SHIFT_JIS.encode(s);
        b.iter().map(|&x| x as char).collect()
    }

    #[test]
    fn fixes_and_keeps() {
        for ja in ["安藤まさひろ", "運命の夜-piano ver.-", "ウッソ・エヴィン", "トラック 12"] {
            assert_eq!(fix_mojibake(&sjis_as_latin1(ja)).as_deref(), Some(ja));
        }
        for ok in ["Céline Dion", "Björk", "Sigur Rós", "Mötley Crüe", "Déjà Vu", "Prajñà", "Far East Café", "太陽のメロディー", "So What", ""] {
            assert_eq!(fix_mojibake(ok), None, "{ok}");
        }
    }
}
