//! gen.py の合成ライブラリ（解析キャッシュと DB）を、同じ式・同じ乱数で作る（整数の乱数と四則演算だけなので Python と同じ値になる）。
//! テストから `#[path = "fixtures/planner/synth.rs"] mod synth;` で読み込む。
#![allow(dead_code)]
use djmix::analysis::types::{Analysis, BarFeats};
use djmix::analysis::{cache, stems};
use djmix::db;
use serde_json::{json, Value};

const KEYS_N: [&str; 2] = ["A", "B"];
const GENRES: [Option<&str>; 12] = [
    Some("Jazz"),
    Some("Smooth Jazz"),
    Some("Rock"),
    Some("Pop"),
    Some("Soul"),
    Some("J-Pop"),
    Some("House"),
    None,
    Some("Bossa Nova"),
    Some("Classical"),
    Some("Funk"),
    Some("Soundtrack"),
];
const LEVELS: [f64; 4] = [0.12, 0.3, 0.55, 0.8];

fn key(i: i64) -> String {
    format!("{}{}", i / 2 + 1, KEYS_N[(i % 2) as usize])
}

/// splitmix64（gen.py と同じ）
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn u(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / 9007199254740992.0)
    }
    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
}

pub struct Meta {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<&'static str>,
    pub status: &'static str,
}

/// 曲 k の解析結果・小節ごとの特徴量・パートの量・タグ（gen.py の track と同じ）
pub fn track(k: i64, lib: i64) -> (Analysis, BarFeats, Option<Vec<[f32; 4]>>, Meta) {
    let mut r = Rng::new((k * 7919 + lib * 104729 + 17) as u64);
    let tk = r.int(0, 9);
    let mut bar_ms = r.int(1700, 2400);
    if tk == 0 {
        bar_ms *= 2;
    } else if tk == 1 {
        bar_ms = bar_ms * 2 / 3;
    }
    let bpm = 240000.0 / bar_ms as f64;
    let mut nbars = r.int(60, 180);
    if k % 31 == 0 {
        nbars = 12;
    }
    let off = if r.int(0, 3) == 0 { r.int(500, 25000) } else { r.int(0, 400) };
    let tail = r.int(0, 9000);
    let times: Vec<f64> = (0..nbars).map(|i| (off + i * bar_ms) as f64 / 1000.0).collect();
    let duration = (off + nbars * bar_ms + tail) as f64 / 1000.0;
    let mut nb: Vec<(f64, f64)> = vec![];
    let kind = r.int(0, 6);
    if kind == 1 || kind == 4 {
        let j = r.int(2, 12);
        nb.push((0.0, (off + j * bar_ms - 300) as f64 / 1000.0));
    }
    if kind == 2 || kind == 4 {
        let j = nbars - r.int(2, 14);
        nb.push(((off + j * bar_ms) as f64 / 1000.0, duration));
    }
    if kind == 3 {
        let j = r.int(20, 21.max(nbars - 30));
        let e = j + r.int(2, 10);
        nb.push(((off + j * bar_ms) as f64 / 1000.0, (off + e * bar_ms) as f64 / 1000.0));
    }
    let kk = key(r.int(0, 23));
    let head = if r.int(0, 3) < 2 { kk.clone() } else { key(r.int(0, 23)) };
    let tailk = if r.int(0, 3) < 2 { kk.clone() } else { key(r.int(0, 23)) };
    let centroid = 700.0 + r.int(0, 25000) as f64 / 10.0;
    let low_ratio = r.int(500, 4500) as f64 / 10000.0;
    let onset_rate = r.int(800, 7000) as f64 / 1000.0;
    let seclen = r.int(8, 24);
    let phase = r.int(0, 3);
    let harm_base = r.int(2, 60) as f64 / 100.0;
    let chroma_base: Vec<f64> = (0..12).map(|_| r.int(0, 500) as f64 / 1000.0).collect();
    let mut feats: BarFeats = vec![];
    for i in 0..nbars {
        let lvl = LEVELS[((i / seclen + phase) % 4) as usize];
        let mut row = [0f32; 15];
        row[0] = (lvl + (r.u() - 0.5) * 0.06) as f32;
        row[1] = (lvl * 0.7 + r.u() * 0.1) as f32;
        for c in 0..12 {
            row[2 + c] = (chroma_base[c] + r.u() * 0.15) as f32;
        }
        row[14] = (harm_base + r.u() * 0.1) as f32;
        feats.push(row);
    }
    let mut st: Option<Vec<[f32; 4]>> = None;
    if r.int(0, 2) == 0 {
        let vstyle = r.int(0, 2);
        let mut v = vec![];
        for i in 0..nbars {
            let lvl = LEVELS[((i / seclen + phase) % 4) as usize];
            let sing = match vstyle {
                0 => lvl >= 0.3,
                1 => (i / 8) % 2 == 0,
                _ => r.u() < 0.5,
            };
            let x = if sing { 0.5 + r.u() * 0.45 } else { r.u() * 0.2 };
            let mut row = [x as f32, 0.0, 0.0, 0.0];
            for v in row.iter_mut().skip(1) {
                *v = r.u() as f32;
            }
            v.push(row);
        }
        st = Some(v);
    }
    let mut a = json!({
        "duration": duration, "sample_rate": 44100, "bpm": bpm, "beats": [], "meter": 4,
        "bars": times.iter().enumerate().map(|(i, t)| json!({"beat": 4 * i, "time": t, "conf": 0.9})).collect::<Vec<_>>(),
        "no_beat": nb.iter().map(|(s, e)| json!([s, e])).collect::<Vec<_>>(), "lufs": -14.0,
        "key": {"key": null, "camelot": kk, "corr": 0.5},
        "key_head30": {"key": null, "camelot": head, "corr": 0.5},
        "key_tail30": {"key": null, "camelot": tailk, "corr": 0.5},
        "beat_class": "A",
    });
    if k % 37 != 0 {
        a["extra"] = json!({"centroid": centroid, "low_ratio": low_ratio, "onset_rate": onset_rate});
    }
    let g = r.int(0, GENRES.len() as i64 - 1);
    let mut artist = Some(format!("Artist {}", k % 17));
    if k % 17 == 3 && k % 2 == 0 {
        artist = Some("artist 3 ".into());
    }
    if k % 23 == 0 {
        artist = None;
    }
    let meta = Meta {
        title: format!("Track {k:03}"),
        artist,
        album: if k % 5 != 0 { Some(format!("Album {}", (b'A' + (k % 9) as u8) as char)) } else { None },
        genre: GENRES[g as usize],
        status: if k % 29 == 0 { "none" } else { "done" },
    };
    let a: Analysis = serde_json::from_value(a).expect("analysis");
    (a, feats, st, meta)
}

/// 曲 k の ★・再生回数・iTunes の ★、最近かけたか、プレイリスト 1 に入るか（gen.py の prefs_of と同じ）
pub fn prefs_of(k: i64) -> (i64, i64, i64, bool, bool) {
    let rating = if k % 11 == 0 {
        1
    } else if k % 7 == 0 {
        5
    } else if k % 5 == 0 {
        4
    } else if k % 13 == 0 {
        2
    } else {
        0
    };
    let plays = if k % 3 == 0 { (k * 7) % 23 } else { 0 };
    let it = if k % 19 == 0 { 3 } else { 0 };
    (rating, plays, it, k % 10 == 4, k % 4 == 1)
}

/// 解析キャッシュと DB を作る（config::init の後に呼ぶ）
pub fn build(n: i64, lib: i64) {
    let now = djmix::util::now();
    db::with(|c| {
        c.execute("INSERT INTO itunes_playlists(id, pid, name) VALUES(1, 'P1', 'PL1')", [])?;
        Ok(())
    })
    .unwrap();
    let mut pos = 0;
    for k in 1..=n {
        let (a, feats, st, meta) = track(k, lib);
        db::with(|c| {
            c.execute(
                "INSERT INTO tracks(id, path, folder, title, artist, album, genre, analysis_status) VALUES(?,?,?,?,?,?,?,?)",
                rusqlite::params![k, format!("/music/t{k}.mp3"), "/music", meta.title, meta.artist, meta.album, meta.genre, meta.status],
            )?;
            Ok(())
        })
        .unwrap();
        cache::write(k, &a, &[0u8; 30], &feats, &[0f32; 10]).unwrap();
        if let Some(st) = &st {
            stems::save(k, st).unwrap();
        }
        let (rating, plays, it, recent, inpl) = prefs_of(k);
        db::with(|c| {
            if rating != 0 || plays != 0 || it != 0 {
                c.execute(
                    "INSERT INTO track_prefs(track_id, rating, play_count, itunes_rating) VALUES(?,?,?,?)",
                    rusqlite::params![k, rating, plays, it],
                )?;
            }
            if recent {
                c.execute("INSERT INTO plays(track_id, at, source) VALUES(?,?,?)", rusqlite::params![k, now - 3.0 * 86400.0, "deck"])?;
            }
            if inpl {
                c.execute("INSERT INTO itunes_playlist_items(playlist_id, pos, track_id) VALUES(1,?,?)", rusqlite::params![pos, k])?;
                pos += 1;
            }
            Ok(())
        })
        .unwrap();
    }
}

/// 期待値（gen.py の出力。gzip した JSON）
pub fn load_expected(name: &str) -> Value {
    use std::io::Read;
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/planner").join(name);
    let f = std::fs::File::open(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let mut s = String::new();
    flate2::read::GzDecoder::new(f).read_to_string(&mut s).unwrap();
    serde_json::from_str(&s).unwrap()
}

/// JSON を比べる（数は 1e-6 まで。整数と小数の区別もそろえる）。違いを path: 説明 の形で返す
pub fn diff(path: &str, exp: &Value, got: &Value, out: &mut Vec<String>) {
    match (exp, got) {
        (Value::Number(a), Value::Number(b)) => {
            let ai = a.is_i64() || a.is_u64();
            let bi = b.is_i64() || b.is_u64();
            if ai != bi {
                out.push(format!("{path}: 型が違う（期待 {a}・実際 {b}）"));
            } else if ai {
                if a.as_i64() != b.as_i64() {
                    out.push(format!("{path}: 期待 {a}・実際 {b}"));
                }
            } else {
                let (x, y) = (a.as_f64().unwrap(), b.as_f64().unwrap());
                if (x - y).abs() > 1e-6 {
                    out.push(format!("{path}: 期待 {x}・実際 {y}"));
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{path}: 長さが違う（期待 {}・実際 {}）期待={} 実際={}", a.len(), b.len(), short(exp), short(got)));
                return;
            }
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                diff(&format!("{path}[{i}]"), x, y, out);
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            for (k, x) in a {
                match b.get(k) {
                    Some(y) => diff(&format!("{path}.{k}"), x, y, out),
                    None => out.push(format!("{path}.{k}: 実際に無い")),
                }
            }
            for k in b.keys() {
                if !a.contains_key(k) {
                    out.push(format!("{path}.{k}: 余計にある"));
                }
            }
        }
        _ => {
            if exp != got {
                out.push(format!("{path}: 期待 {}・実際 {}", short(exp), short(got)));
            }
        }
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 300 {
        s.chars().take(300).collect::<String>() + "…"
    } else {
        s
    }
}

/// gen.py の run と同じ呼び方で、場面を 1 つ実行する（結果から sec は除く）。エラーは Python の str(e) と比べる
pub fn run_scenario(fun: &str, a: &Value) -> Result<Value, String> {
    use djmix::planner::engine;
    use djmix::settings;
    let prefs: Option<serde_json::Map<String, Value>> = match a.get("prefs") {
        Some(Value::String(s)) if s == "settings" => Some(settings::load()),
        Some(Value::Object(m)) => Some(m.clone()),
        _ => None,
    };
    let prefs = prefs.as_ref();
    let i64s = |k: &str| -> Vec<i64> {
        a.get(k).and_then(|v| v.as_array()).map(|v| v.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default()
    };
    let strs = |k: &str| -> Vec<String> {
        a.get(k)
            .and_then(|v| v.as_array())
            .map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let f64s = |k: &str| -> Option<Vec<f64>> {
        a.get(k).and_then(|v| v.as_array()).map(|v| v.iter().filter_map(|x| x.as_f64()).collect())
    };
    let opt_f = |k: &str| a.get(k).and_then(|v| v.as_f64());
    let opt_i = |k: &str| a.get(k).and_then(|v| v.as_i64());
    let s_or = |k: &str, d: &'static str| -> String { a.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string() };
    let curve = f64s("curve");
    let playlists = a.get("playlists").and_then(|v| v.as_array()).map(|_| i64s("playlists"));
    let r = match fun {
        "candidate_facets" => Ok(engine::candidate_facets()),
        "plan_library" => engine::plan_library(
            opt_i("seed").unwrap(),
            &strs("groups"),
            opt_f("minutes").unwrap(),
            &s_or("flow", "up"),
            &i64s("avoid"),
            curve.as_deref(),
            prefs,
            playlists.as_deref(),
            opt_f("track_len"),
            &s_or("track_pos", "auto"),
            &s_or("join_mode", "auto"),
        ),
        "plan_albums" => engine::plan_albums(
            &strs("albums"),
            opt_f("minutes").unwrap(),
            &s_or("flow", "up"),
            opt_i("seed"),
            a.get("use_all").and_then(|v| v.as_bool()).unwrap_or(false),
            &i64s("avoid"),
            curve.as_deref(),
            prefs,
            opt_f("track_len"),
            &s_or("track_pos", "auto"),
            &s_or("join_mode", "auto"),
        ),
        "plan_queue" => engine::plan_queue(&i64s("ids"), &s_or("join_mode", "auto")),
        "next_candidates" => Ok(engine::next_candidates(
            opt_i("prev"),
            opt_i("nxt"),
            &strs("groups"),
            &i64s("exclude"),
            opt_i("n").unwrap() as usize,
            prefs,
            &strs("artists"),
        )),
        "rejoin" => {
            let spec: Vec<engine::SegSpec> = serde_json::from_value(a["spec"].clone()).unwrap();
            engine::rejoin(&spec, prefs, &s_or("join_mode", "auto"))
        }
        "segment_choices" => Ok(engine::segment_choices(opt_i("tid").unwrap(), opt_f("length"))),
        _ => panic!("{fun}"),
    };
    r.map(|mut v| {
        if let Some(o) = v.as_object_mut() {
            o.remove("sec");
        }
        v
    })
    .map_err(|e| e.0)
}

/// 期待値の場面をすべて実行して比べる。違いの一覧と、場面ごとの時間（秒）を返す
pub fn check_scenarios(exp: &Value) -> (Vec<String>, Vec<(String, f64)>) {
    let mut errs = vec![];
    let mut times = vec![];
    for (i, sc) in exp["scenarios"].as_array().unwrap().iter().enumerate() {
        let fun = sc["fn"].as_str().unwrap();
        let t0 = std::time::Instant::now();
        let got = run_scenario(fun, &sc["args"]);
        times.push((format!("#{i} {fun}"), t0.elapsed().as_secs_f64()));
        let path = format!("#{i} {fun}({})", sc["args"]);
        match (sc.get("result"), sc.get("error"), got) {
            (Some(r), _, Ok(g)) => diff(&path, r, &g, &mut errs),
            (_, Some(Value::String(e)), Err(g)) => {
                if *e != g {
                    errs.push(format!("{path}: エラーの文言が違う（期待 {e}・実際 {g}）"));
                }
            }
            (r, e, g) => errs.push(format!(
                "{path}: 結果の種類が違う（期待 {:?} / {:?}・実際 {:?}）",
                r.is_some(),
                e,
                g.map(|_| "ok")
            )),
        }
    }
    (errs, times)
}

/// 一時フォルダをデータの置き場所にする（テストのバイナリごとに 1 回）
pub fn init_data_dir() -> std::path::PathBuf {
    let dir = tempfile::tempdir().unwrap().keep();
    djmix::config::init(djmix::config::Paths::new(dir.clone(), djmix::config::default_resources_dir()));
    dir
}
