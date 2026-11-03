//! おまかせ（planner）の単体テスト: Windows 版の tests/test_planner.py・test_segments.py・test_cutin.py のうち、
//! 要約（desc）・使う範囲の候補（segments）・相性と探索（engine）に関するものを移したもの。合成の要約・解析キャッシュで試す。
#[path = "fixtures/planner/synth.rs"]
mod synth;

use djmix::analysis::cutpoints;
use djmix::analysis::types::{Analysis, BarFeats, Extra, Section};
use djmix::analysis::{cache, stems};
use djmix::db;
use djmix::planner::desc::{self, Desc, PyNum, Region, Use};
use djmix::planner::engine::{self, Join, JoinParams, Matrix, Pool, TrackRow};
use djmix::planner::segments::{self, Seg};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use synth::Rng;

/// DB（★・再生履歴）を読むテストは 1 つずつ（★ を書き込むテストがあるため）
static LOCK: Lazy<Mutex<()>> = Lazy::new(|| {
    synth::init_data_dir();
    Mutex::new(())
});

fn lock() -> parking_lot::MutexGuard<'static, ()> {
    LOCK.lock()
}

// ---------------------------------------------------------------- 要約（test_planner.py の synth）
/// 一定テンポの曲の解析結果と小節ごとの特徴量。途中（n/2〜）だけ盛り上がる。
fn synth_track(n: usize, bpm: f64, no_beat: &[(f64, f64)], seed: u64) -> (Analysis, BarFeats) {
    let mut r = Rng::new(seed);
    let bar = 4.0 * 60.0 / bpm;
    let mut feats: BarFeats = vec![[0f32; 15]; n];
    for (i, row) in feats.iter_mut().enumerate() {
        let level = if i >= n / 2 { 0.9 } else { 0.3 };
        row[0] = (level + (r.u() - 0.5) * 0.04) as f32;
        row[1] = (level * 0.8) as f32;
        for c in 0..12 {
            row[2 + c] = (r.u() * 0.1 + 0.5) as f32;
        }
        row[14] = 0.3;
    }
    let a = json!({
        "bpm": bpm, "meter": 4, "duration": n as f64 * bar, "beats": [],
        "bars": (0..n).map(|i| json!({"beat": i * 4, "time": desc::np::round(i as f64 * bar, 3), "conf": 1.0})).collect::<Vec<_>>(),
        "no_beat": no_beat.iter().map(|(s, e)| json!([s, e])).collect::<Vec<_>>(), "beat_class": "A",
        "key": {"camelot": "8B", "corr": 0.5}, "key_head30": {"camelot": "8B", "corr": 0.5}, "key_tail30": {"camelot": "8B", "corr": 0.5},
        "extra": {"centroid": 1500.0, "low_ratio": 0.2, "onset_rate": 3.0},
    });
    (serde_json::from_value(a).unwrap(), feats)
}

#[test]
fn use_range_is_cut_for_long_track_and_fadeout() {
    let _g = lock();
    // 200 小節 × 2 秒 = 400 秒。最後の 6 秒は拍が取れない（フェードアウト）
    let (a, feats) = synth_track(200, 120.0, &[(394.0, 400.0)], 1);
    let d = desc::build(&a, &feats, None).unwrap();
    let u = &d.use_;
    assert!(u.t1 - u.t0 <= desc::MAX_USE_SEC + 1.0 && u.t1 - u.t0 >= desc::MIN_USE_SEC, "{u:?}");
    assert!(u.t1 <= 394.0 && u.bar1 > u.bar0);
    assert!((u.t0 / 2.0 - (u.t0 / 2.0).round()).abs() < 1e-6); // 端は小節頭
    assert!(d.out.nobeat.v == 0.0 && d.in_.chroma.len() == 12);
}

#[test]
fn short_track_keeps_whole_and_bad_track_is_skipped() {
    let _g = lock();
    let (a, feats) = synth_track(100, 120.0, &[], 1); // 200 秒
    let u = desc::build(&a, &feats, None).unwrap().use_;
    assert!(u.t0 == 0.0 && u.t1 == 200.0);
    let (a2, f2) = synth_track(10, 120.0, &[], 1);
    assert!(desc::build(&a2, &f2, None).is_none()); // 小節が少なすぎる
}

#[test]
fn store_and_load() {
    let _g = lock();
    db::with(|c| {
        c.execute(
            "INSERT INTO tracks(id, path, folder, title, genre, analysis_status) VALUES(501, 'a.mp3', 'x', 'A', 'Jazz', 'done')",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let (a, feats) = synth_track(200, 120.0, &[], 1);
    desc::update(501, &a, &feats, None).unwrap();
    let got = desc::load_all();
    let d = got.iter().find(|d| d.id == 501).unwrap();
    assert!(d.title.as_deref() == Some("A") && d.bpm == 120.0 && d.use_.bar1 > 0);
    // 保存した JSON のキーは Python 版と同じ
    let data: String = db::with(|c| Ok(c.query_row("SELECT data FROM track_desc WHERE track_id = 501", [], |r| r.get(0))?)).unwrap();
    let v: Value = serde_json::from_str(&data).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        ["bpm", "meter", "duration", "beat_class", "key", "key_head", "key_tail", "extra", "use", "in", "out", "vocals"]
    );
    assert_eq!(v["in"]["nobeat"], json!(0)); // 拍の取れない区間が無い曲は整数の 0（Python の sum([])）
}

#[test]
fn stable_bounds_moves_join_to_steady_beats() {
    let _g = lock();
    // Phase 5 F: 繋ぐ部分（終わり・始まりの 16 小節）に拍の取れない所があれば、拍の取れる所までずらす。
    let bt: Vec<f64> = (0..201).map(|i| i as f64 * 2.0).collect(); // 2 秒の小節 × 200（400 秒）
    let nb = [(360.0, 400.0), (0.0, 20.0)]; // 最後の 40 秒・最初の 20 秒は拍なし
    let mut notes = vec![];
    let (b0, b1) = desc::stable_bounds(&nb, &bt, 0, 200, Some(&mut notes));
    assert!(b1 == 180 && b0 == 10); // 360 秒で終える・20 秒から始める
    assert_eq!(notes, ["拍の取れる所で終える", "拍の取れる所から始める"]);
    // ずらせる上限（max(90 秒, 40%)）を超えるなら、そのまま
    assert_eq!(desc::stable_bounds(&[(100.0, 400.0)], &bt, 0, 200, None), (0, 200));
    // 拍が取れていればそのまま
    assert_eq!(desc::stable_bounds(&[], &bt, 0, 200, None), (0, 200));
}

// ---------------------------------------------------------------- 相性と探索（合成の要約で試す）
struct Fd {
    bpm: f64,
    key: &'static str,
    genre: &'static str,
    artist: String,
    nobeat: f64,
    cen: f64,
    ons: f64,
    use_len: f64,
    seed: u64,
}

impl Default for Fd {
    fn default() -> Self {
        Fd { bpm: 120.0, key: "8B", genre: "Jazz", artist: "A".into(), nobeat: 0.0, cen: 1500.0, ons: 3.0, use_len: 240.0, seed: 0 }
    }
}

fn fake_desc(id: i64, f: Fd) -> Desc {
    let mut r = Rng::new(f.seed + 1000);
    let ch: Vec<f64> = (0..12).map(|_| r.u() * 0.1 + 0.5).collect();
    let reg = Region { chroma: ch, harm: 0.4, low: 0.5, nobeat: PyNum::float(f.nobeat) };
    Desc {
        id,
        bpm: f.bpm,
        meter: 4,
        duration: f.use_len + 20.0,
        beat_class: Some("A".into()),
        key: f.key.into(),
        key_head: f.key.into(),
        key_tail: f.key.into(),
        extra: Extra { centroid: f.cen, low_ratio: 0.2, onset_rate: f.ons },
        use_: Use { t0: 0.0, t1: f.use_len, bar0: 0, bar1: (f.use_len / (240.0 / f.bpm)) as i64, notes: vec![] },
        in_: reg.clone(),
        out: reg,
        title: Some(format!("t{}", f.seed)),
        artist: Some(f.artist),
        genre: Some(f.genre.into()),
        ..Default::default()
    }
}

fn many(n: i64) -> Vec<Desc> {
    let mut r = Rng::new(5);
    let keys = ["8B", "9B", "7B", "8A", "3A"];
    let mut uni = |lo: f64, hi: f64| lo + (hi - lo) * r.u();
    (1..=n)
        .map(|i| {
            let bpm = uni(105.0, 130.0);
            let cen = uni(800.0, 3000.0);
            let ons = uni(1.0, 6.0);
            let use_len = uni(200.0, 300.0);
            fake_desc(
                i,
                Fd {
                    bpm,
                    key: keys[(i % 5) as usize],
                    genre: if i % 4 != 0 { "Jazz" } else { "Rock" },
                    artist: format!("a{}", i % 13),
                    cen,
                    ons,
                    use_len,
                    seed: i as u64,
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn ids_of(plan: &Value) -> Vec<i64> {
    plan["tracks"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
}

fn matrix(pool: &Pool, ids: &[i64]) -> Matrix {
    Matrix::new(&pool.view(ids), ids, "auto")
}

#[test]
fn pair_scores() {
    let _g = lock();
    let pool = Pool::new(vec![
        fake_desc(1, Fd { bpm: 120.0, ..Default::default() }),
        fake_desc(2, Fd { bpm: 121.0, seed: 2, ..Default::default() }),
        fake_desc(3, Fd { bpm: 60.5, seed: 3, ..Default::default() }),
        fake_desc(4, Fd { bpm: 120.0, key: "3A", seed: 4, ..Default::default() }),
        fake_desc(5, Fd { bpm: 121.0, nobeat: 4.0, seed: 5, ..Default::default() }),
        fake_desc(6, Fd { bpm: 90.0, seed: 6, ..Default::default() }),
    ]);
    let m = matrix(&pool, &[1, 2, 3, 4, 5, 6]);
    let s = |a: i64, b: i64| m.s_at(m.pos[&a], m.pos[&b]);
    assert!(s(1, 2) > s(1, 4)); // キーが離れていると下がる
    let p = m.pair(m.pos[&1], m.pos[&3]);
    assert!(p.k == 0.5 && p.ts == 1.0); // 半分のテンポは ×½ で合わせる
    let j = m.join(1, 5);
    assert!(j.tpl == "echo" && j.why.join(" ").contains("エコーアウト")); // 拍が取れない
    let j = m.join(1, 2);
    assert!(j.tpl == "long" && j.bars == 16);
    assert!(s(1, 6) < s(1, 2)); // 90→120 は離れている
}

#[test]
fn plan_library_three_distinct_plans() {
    let _g = lock();
    let pool = Pool::new(many(40));
    let r = pool.plan_library(1, &["ジャズ・フュージョン".into()], 20.0, "up", &[], None, None, None, None, "auto", "auto").unwrap();
    let plans = r["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 3);
    for p in plans {
        let t = p["tracks"].as_array().unwrap();
        assert!(t[0]["id"] == 1 && p["minutes"].as_f64().unwrap() >= 20.0);
        assert!(t[1..].iter().all(|x| x["genre"] == "Jazz")); // 範囲の曲だけ
        assert_eq!(ids_of(p).into_iter().collect::<HashSet<_>>().len(), t.len()); // 同じ曲は 2 回使わない
        assert!(t[..t.len() - 1].iter().all(|x| x.get("join").is_some()) && t[t.len() - 1].get("join").is_none());
    }
    let a: HashSet<i64> = ids_of(&plans[0])[1..].iter().copied().collect();
    let b: HashSet<i64> = ids_of(&plans[1])[1..].iter().copied().collect();
    assert!(a.intersection(&b).count() as f64 <= 0.5 * a.len().max(b.len()) as f64); // 互いに半分以上違う
}

#[test]
fn plan_queue_keeps_tracks_and_improves() {
    let _g = lock();
    let pool = Pool::new(many(40));
    let ids = [3, 17, 8, 22, 11, 30, 5];
    let r = pool.plan_queue(&ids, "auto").unwrap();
    let (best, cur) = (&r["plans"][0], &r["plans"][1]);
    let mut b = ids_of(best);
    b.sort();
    let mut want = ids.to_vec();
    want.sort();
    assert_eq!(b, want);
    assert_eq!(ids_of(cur), ids);
    assert!(best["score"].as_f64() >= cur["score"].as_f64());
}

#[test]
fn next_candidates() {
    let _g = lock();
    let pool = Pool::new(many(40));
    let c = pool.next_candidates(Some(1), None, &[], &[2, 3], 5, None, &[]);
    let c = c.as_array().unwrap();
    assert!(c.len() == 5 && c.iter().all(|x| ![1, 2, 3].contains(&x["id"].as_i64().unwrap())));
    let sc: Vec<f64> = c.iter().map(|x| x["score"].as_f64().unwrap()).collect();
    let mut sorted = sc.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(sc, sorted);
    let r = pool.next_candidates(Some(1), Some(9), &[], &[], 5, None, &[]); // 差し替え: 前後の相性の平均
    assert!(r.as_array().unwrap().iter().all(|x| x.get("join").is_some() && x.get("join_next").is_some()));
}

#[test]
fn plan_follows_drawn_curve() {
    let _g = lock();
    let pool = Pool::new(many(60));
    let up = &pool.plan_library(1, &[], 25.0, "custom", &[], Some(&[0.1, 0.9]), None, None, None, "auto", "auto").unwrap()["plans"][0];
    let down = &pool.plan_library(1, &[], 25.0, "custom", &[], Some(&[0.9, 0.1]), None, None, None, "auto", "auto").unwrap()["plans"][0];
    let half_mean = |p: &Value| {
        let e: Vec<f64> = p["tracks"].as_array().unwrap()[1..].iter().map(|t| t["energy"].as_f64().unwrap()).collect();
        let h = &e[e.len() / 2..];
        h.iter().sum::<f64>() / h.len() as f64
    };
    // 描いた線の向きに、後半の曲のエネルギーが寄る
    assert!(half_mean(up) > half_mean(down));
}

fn set_prefs(ratings: &[(i64, i64)], recent: &[i64]) {
    let now = djmix::util::now();
    db::with(|c| {
        for t in 1..=60 {
            c.execute("INSERT OR IGNORE INTO tracks(id, path, folder, analysis_status) VALUES(?, ?, 'x', 'none')", rusqlite::params![t, format!("p{t}.mp3")])?;
        }
        c.execute("DELETE FROM track_prefs", [])?;
        c.execute("DELETE FROM plays", [])?;
        for (t, r) in ratings {
            c.execute("INSERT INTO track_prefs(track_id, rating) VALUES(?, ?)", rusqlite::params![t, r])?;
        }
        for t in recent {
            c.execute("INSERT INTO plays(track_id, at, source) VALUES(?, ?, 'deck')", rusqlite::params![t, now - 3600.0])?;
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn prefs_skip_and_prefer() {
    let _g = lock();
    let pool = Pool::new(many(60));
    let plan = |p: &Pool, o: Option<&Map<String, Value>>| p.plan_library(1, &[], 25.0, "keep", &[], None, o, None, None, "auto", "auto").unwrap();
    let first = plan(&pool, None);
    let used: Vec<i64> = ids_of(&first["plans"][0])[1..].to_vec();
    // ★1 と最近かけた曲は使わない（起点の曲は除く）
    set_prefs(&[(used[0], 1)], &[used[1], 1]);
    let o = json!({"skip_rating1": true, "avoid_recent_days": 14});
    let r = plan(&pool, o.as_object());
    let ids: HashSet<i64> = r["plans"].as_array().unwrap().iter().flat_map(ids_of).collect();
    assert!(!ids.contains(&used[0]) && !ids.contains(&used[1]) && r["plans"][0]["tracks"][0]["id"] == 1);
    // ★5 を付けた曲は選ばれやすくなる
    let seen: HashSet<i64> = plan(&pool, None)["plans"].as_array().unwrap().iter().flat_map(ids_of).collect();
    let unused: Vec<i64> = (2..61).filter(|t| !seen.contains(t)).take(15).collect();
    set_prefs(&unused.iter().map(|&t| (t, 5)).collect::<Vec<_>>(), &[]);
    let o = json!({"prefer_rating": true});
    let r = plan(&pool, o.as_object());
    assert!(ids_of(&r["plans"][0]).iter().any(|t| unused.contains(t)));
    // ★ は結果の曲にも入る
    assert!(r["plans"][0]["tracks"].as_array().unwrap().iter().any(|t| t["rating"] == 5));
    set_prefs(&[], &[]);
}

#[test]
fn auto_ramp_shift_loop() {
    let _g = lock();
    // 1→2: テンポ差 10%（ランプ）、キー 8B→3B は合わないが 3B を −1 半音で 8B（同じ）、2 は使う範囲が短い（ループ）
    let pool = Pool::new(vec![
        fake_desc(1, Fd { bpm: 120.0, key: "8B", seed: 1, ..Default::default() }),
        fake_desc(2, Fd { bpm: 109.0, key: "3B", seed: 2, use_len: 40.0, ..Default::default() }),
        fake_desc(3, Fd { bpm: 110.0, key: "9B", seed: 3, ..Default::default() }),
    ]);
    let m = matrix(&pool, &[1, 2, 3]);
    let j = m.join(1, 2);
    assert!(m.pair(0, 1).ramp && j.tpl == "long" && j.params.tempo == "ramp");
    let p = engine::pack(&m, &[0, 1, 2], false, &HashMap::new());
    let (j1, j2) = (p.tracks[0].join.as_ref().unwrap(), p.tracks[1].join.as_ref().unwrap());
    assert!(j1.params.shift == -1 && j1.why.iter().any(|w| w.contains("-1 半音")));
    assert!(engine::shift_camelot("3B", -1) == "8B" && engine::shift_camelot("3B", 1) == "10B");
    // 2 は −1 半音ずれたまま（末尾 8B）→ 3 は 9B（隣）なので、ずらさずに合う
    assert_eq!(j2.params.shift, 0);
    assert_eq!(j2.params.loop_, 4); // 2 の使う範囲が短い
}

#[test]
fn vocal_fit_and_clash() {
    let _g = lock();
    // モックアップの例と同じ形: 前の曲は最後の 6 小節以外ほぼ歌、次の曲は最初の 8 小節が歌なし
    let vo: Vec<f64> = [vec![0.8; 60], vec![0.0; 6]].concat();
    let vi: Vec<f64> = [vec![0.0; 8], vec![0.8; 40]].concat();
    let fit = engine::best_vocal_fit(&vo, &vi, 16, 16, 8);
    assert!(fit.clash0 > 0 && fit.clash == 0 && fit.bars == 12 && fit.trim == 0); // 12 小節でも重ならない
    assert_eq!(engine::best_vocal_fit(&[0.0; 40], &vi, 16, 16, 8).clash0, 0); // インストなら何もしない
    let mut ds = vec![
        fake_desc(1, Fd { seed: 1, ..Default::default() }),
        fake_desc(2, Fd { seed: 2, ..Default::default() }),
        fake_desc(3, Fd { seed: 3, ..Default::default() }),
    ];
    ds[0].vocals = Some(vec![0.9; 40]);
    ds[1].vocals = Some(vec![0.9; 40]);
    ds[2].vocals = Some(vec![0.0; 40]);
    let pool = Pool::new(ds);
    let m = matrix(&pool, &[1, 2, 3]);
    assert!(m.pair(0, 1).vclash == 1.0 && m.pair(0, 2).vclash == 0.0);
    assert!(m.s_at(0, 1) < m.s_at(0, 2) && m.join(1, 2).why.iter().any(|w| w.contains("ボーカル")));
}

#[test]
fn choose_parts() {
    let _g = lock();
    // Phase 5 B2: ロングは入れ替え、歌が重なるショートは歌を先に抜く。設定で切れる。
    let none = Desc::default();
    let voc = Desc { vocals: Some(vec![0.9; 40]), ..Default::default() };
    let jn = |tpl: &str, bars: i64| Join { tpl: tpl.into(), bars, ..Default::default() };
    let mut j = jn("long", 16);
    engine::choose_parts(true, &none, &none, &mut j);
    assert_eq!(j.params.parts.as_deref(), Some("swap"));
    let mut j = jn("short", 8);
    engine::choose_parts(true, &voc, &voc, &mut j);
    assert_eq!(j.params.parts.as_deref(), Some("vocal"));
    let mut j = jn("echo", 2);
    engine::choose_parts(true, &voc, &voc, &mut j);
    assert_eq!(j.params.parts.as_deref(), Some("none"));
    let mut j = jn("long", 16);
    engine::choose_parts(false, &none, &none, &mut j);
    assert_eq!(j.params.parts.as_deref(), Some("none"));
}

#[test]
fn next_candidates_filter_artist_and_genre() {
    // 次の曲の候補をアーティスト・ジャンルのまとまりで絞る（大文字小文字・空白は区別しない）。
    let _g = lock();
    let pool = Pool::new(many(40));
    let c = pool.next_candidates(Some(1), None, &[], &[], 20, None, &["A3 ".into(), "a5".into()]);
    let c = c.as_array().unwrap();
    assert!(!c.is_empty() && c.iter().all(|x| x["artist"] == "a3" || x["artist"] == "a5"));
    let g = pool.next_candidates(Some(1), None, &["ロック".into()], &[], 20, None, &[]);
    let g = g.as_array().unwrap();
    assert!(!g.is_empty() && g.iter().all(|x| x["genre"] == "Rock"));
    let both = pool.next_candidates(Some(1), None, &["ロック".into()], &[], 20, None, &["a4".into(), "a8".into()]);
    assert!(both.as_array().unwrap().iter().all(|x| x["genre"] == "Rock" && (x["artist"] == "a4" || x["artist"] == "a8")));
    let f = pool.candidate_facets();
    let names: HashSet<String> = f["artists"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, (0..13).map(|i| format!("a{i}")).collect::<HashSet<_>>());
    assert_eq!(f["artists"].as_array().unwrap().iter().map(|a| a["count"].as_i64().unwrap()).sum::<i64>(), 40);
    let groups: HashSet<&str> = f["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
    assert_eq!(groups, HashSet::from(["ジャズ・フュージョン", "ロック"]));
}

#[test]
fn first_track_starts_from_top() {
    // ミックスの最初の曲は、短い前奏（30 秒以下）を飛ばさず 0:00 から。2 曲目以降は使う範囲のまま（小節頭から）。
    let _g = lock();
    let mut ds = many(40);
    for d in ds.iter_mut() {
        d.use_.t0 = 1.42;
    }
    let pool = Pool::new(ds);
    let r = pool.plan_queue(&[3, 17, 8], "auto").unwrap();
    let t = r["plans"][0]["tracks"].as_array().unwrap();
    assert!(t[0]["use"]["t0"] == 0.0 && t[0]["use"]["notes"].as_array().unwrap().contains(&json!("最初の曲なので冒頭から")));
    assert!(t[1..].iter().all(|x| x["use"]["t0"] == 1.42));
}

#[test]
fn short_tracks_get_short_overlaps() {
    let _g = lock();
    // 短くたくさん繋ぐ（2026-10-25）: 1 曲 90 秒以下を選んだ曲は、重なりを短い方の範囲の 1/3 まで（4 小節単位、最低 4）。ループで延ばさない。
    let ds: Vec<Desc> = (1..=3)
        .map(|k| {
            let mut d = fake_desc(k, Fd { seed: k as u64, use_len: 40.0, ..Default::default() }); // 40 秒 = 20 小節
            d.seg = Some(Seg { len: Some(45.0), pos: "auto".into(), t0: None, t1: None });
            d
        })
        .collect();
    let pool = Pool::new(ds);
    let m = matrix(&pool, &[1, 2, 3]);
    let j = m.join(1, 2);
    assert!(j.tpl == "short" && j.bars == 4 && j.why.iter().any(|w| w.contains("曲が短い")));
    assert!((m.overlap_sec(0, 1) - 4.0 * 2.0).abs() < 1e-9);
    let p = engine::pack(&m, &[0, 1, 2], false, &HashMap::new());
    assert!(p.tracks[..2].iter().all(|t| t.join.as_ref().unwrap().params.loop_ == 0));
    // 長さを選んでいない曲は今までどおり（標準の重なり）
    let pool2 = Pool::new((1..=2).map(|k| fake_desc(k, Fd { seed: k as u64, use_len: 40.0, ..Default::default() })).collect());
    let m2 = matrix(&pool2, &[1, 2]);
    assert!([8, 16].contains(&m2.join(1, 2).bars));
}

/// アルバムで選ぶ（2026-10-25）の試験用: アルバム A〜D、テンポもばらばら。
fn album_pool() -> Pool {
    let mut ds = vec![];
    let mut k = 1i64;
    for (a, (n, bpm)) in [(6, 118.0), (6, 122.0), (6, 90.0), (2, 140.0)].into_iter().enumerate() {
        for _ in 0..n {
            let mut d =
                fake_desc(k, Fd { bpm: bpm + (k % 3) as f64, seed: k as u64, artist: format!("art{a}"), use_len: 200.0, ..Default::default() });
            d.album = Some(["A", "B", "C", "D"][a].into());
            ds.push(d);
            k += 1;
        }
    }
    Pool::new(ds)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn plan_albums_covers_every_album_and_ignores_tempo() {
    let _g = lock();
    let pool = album_pool();
    let r = pool.plan_albums(&s(&["A", "B", "C", "D"]), 30.0, "up", None, false, &[], None, None, None, "auto", "auto").unwrap();
    let plans = r["plans"].as_array().unwrap();
    assert!(!plans.is_empty() && r["albums"] == 4 && r["missing_albums"].as_array().unwrap().is_empty());
    for p in plans {
        let alb: HashSet<String> = ids_of(p).iter().map(|t| pool.d[t].album.clone().unwrap()).collect();
        assert_eq!(alb, s(&["A", "B", "C", "D"]).into_iter().collect::<HashSet<_>>()); // どのアルバムからも入る
        assert!(p["albums"].as_object().unwrap().values().all(|v| v.as_i64().unwrap() >= 1));
    }
    let firsts: HashSet<i64> = plans.iter().map(|p| ids_of(p)[0]).collect();
    assert!(firsts.len() >= 2); // 起点の曲は自動で、案ごとに違う
}

#[test]
fn plan_albums_seed_use_all_and_errors() {
    let _g = lock();
    let pool = album_pool();
    let run = |albums: &[&str], minutes: f64, flow: &str, seed: Option<i64>, use_all: bool| {
        pool.plan_albums(&s(albums), minutes, flow, seed, use_all, &[], None, None, None, "auto", "auto")
    };
    let r = run(&["A", "B", "C"], 30.0, "keep", Some(7), false).unwrap();
    assert!(r["plans"].as_array().unwrap().iter().all(|p| ids_of(p)[0] == 7));
    let r = run(&["A", "B", "C", "D"], 5.0, "up", None, true).unwrap();
    let plans = r["plans"].as_array().unwrap();
    assert!(plans.iter().all(|p| ids_of(p).len() == 20) && r["max_minutes"].as_f64().unwrap() > 30.0);
    assert_eq!(plans.iter().map(ids_of).collect::<HashSet<_>>().len(), plans.len());
    let r = run(&["A", "B", "C"], 600.0, "up", None, false).unwrap(); // 曲が足りない → そこまでの並び＋使える長さの目安
    assert!(!r["plans"].as_array().unwrap().is_empty() && r["max_minutes"].as_f64().unwrap() < 600.0);
    assert_eq!(run(&["A", "B"], 30.0, "up", None, false).unwrap_err().0, "アルバムを 3 枚以上選んでください");
    assert_eq!(
        run(&["A", "B", "Z"], 30.0, "up", None, false).unwrap_err().0,
        "解析済みの曲があるアルバムが 2 枚しかありません（3 枚以上必要。解析を進めてください）"
    );
}

#[test]
fn errors_match_python() {
    let _g = lock();
    let pool = Pool::new(many(10));
    let e = pool.plan_library(999, &[], 20.0, "up", &[], None, None, None, None, "auto", "auto").unwrap_err();
    assert_eq!(e.0, "起点の曲がまだ解析されていません");
    let e = pool.plan_library(1, &["クラシック".into()], 20.0, "up", &[], None, None, None, None, "auto", "auto").unwrap_err();
    assert_eq!(e.0, "候補の曲が少なすぎます（範囲を広げるか、ライブラリの解析を進めてください）");
    let e = pool.plan_library(1, &[], 20.0, "sideways", &[], None, None, None, None, "auto", "auto").unwrap_err();
    assert_eq!(e.0, "'sideways'"); // Python では KeyError（サーバーは 400 で str(e) を返す）
    assert_eq!(pool.plan_queue(&[1, 999], "auto").unwrap_err().0, "解析済みの曲が 2 曲以上必要です");
    assert_eq!(pool.rejoin(&[], None, "auto").unwrap_err().0, "解析済みの曲がありません");
    assert_eq!(pool.next_candidates(Some(999), None, &[], &[], 5, None, &[]), json!([]));
    assert_eq!(pool.segment_choices(999, None), json!([]));
}

// ---------------------------------------------------------------- 使う範囲の候補（test_segments.py）
fn write_track(tid: i64, times: &[f64], duration: f64, feats: &BarFeats) {
    let a: Analysis = serde_json::from_value(json!({
        "bars": times.iter().enumerate().map(|(i, t)| json!({"beat": 4 * i, "time": t, "conf": 1.0})).collect::<Vec<_>>(),
        "duration": duration, "no_beat": [], "meter": 4, "bpm": 120.0, "beats": [],
    }))
    .unwrap();
    cache::write(tid, &a, &[0u8; 30], feats, &[0f32; 10]).unwrap();
    segments::forget(tid);
}

fn uniform_feats(n: usize, seed: u64) -> BarFeats {
    let mut r = Rng::new(seed);
    (0..n)
        .map(|_| {
            let mut row = [0f32; 15];
            for v in row.iter_mut() {
                *v = (0.2 + 0.1 * r.u()) as f32;
            }
            row
        })
        .collect()
}

/// 2 秒の小節 × 200 = 400 秒。200〜280 秒が一番大きい（盛り上がり）
static TRACK: Lazy<Desc> = Lazy::new(|| {
    Lazy::force(&LOCK);
    let mut feats = uniform_feats(200, 1);
    for row in feats[100..140].iter_mut() {
        row[0] = 0.9;
    }
    let times: Vec<f64> = (0..200).map(|i| 2.0 * i as f64).collect();
    write_track(1007, &times, 400.0, &feats);
    Desc { use_: Use { t0: 0.0, t1: 330.0, bar0: 0, bar1: 165, notes: vec![] }, ..Default::default() }
});

/// 最初の小節頭が 1.5 秒（CONTROL のように、冒頭に小節の前の音がある曲）。
static LATE_START: Lazy<Desc> = Lazy::new(|| {
    Lazy::force(&LOCK);
    let times: Vec<f64> = (0..199).map(|i| 1.5 + 2.0 * i as f64).collect();
    write_track(1008, &times, 400.0, &uniform_feats(199, 2));
    Desc { use_: Use { t0: 1.5, t1: 331.5, bar0: 0, bar1: 165, notes: vec![] }, ..Default::default() }
});

fn var(tid: i64, base: &Desc, len: Option<f64>, pos: &str) -> Option<segments::Variant> {
    segments::variant(tid, base, len, Some(pos), None, None)
}

#[test]
fn segment_variants() {
    let _g = lock();
    let t = &*TRACK;
    let head = var(1007, t, Some(180.0), "head").unwrap().use_;
    assert!(head.t0 == 0.0 && 150.0 <= head.t1 && head.t1 <= 210.0);
    let tail = var(1007, t, Some(180.0), "tail").unwrap().use_;
    assert!(tail.t1 == 400.0 && 150.0 <= tail.t1 - tail.t0 && tail.t1 - tail.t0 <= 210.0);
    let peak = var(1007, t, Some(120.0), "peak").unwrap().use_;
    assert!(peak.t0 <= 220.0 && peak.t1 >= 260.0, "{peak:?}"); // 盛り上がり（200〜280 秒）を含む
    let full = var(1007, t, None, "full").unwrap().use_;
    assert_eq!((full.t0, full.t1), (0.0, 400.0));
    let auto = var(1007, t, Some(240.0), "auto").unwrap().use_;
    assert!(auto.t0 == 0.0 && 200.0 <= auto.t1 && auto.t1 <= 280.0);
    assert!(var(1007, t, None, "auto").is_none()); // 指定なし → 要約の範囲のまま
    let v = var(1007, t, Some(180.0), "head").unwrap();
    assert_eq!(v.seg, Seg { len: Some(180.0), pos: "head".into(), t0: None, t1: None });
    assert!(v.in_.chroma.len() == 12 && v.out.chroma.len() == 12);
}

#[test]
fn segment_choices() {
    let _g = lock();
    let c = segments::choices(1007, &TRACK, Some(180.0));
    assert_eq!(c[0].pos, "auto");
    let pos: HashSet<&str> = c.iter().map(|x| x.pos.as_str()).collect();
    assert!(["peak", "tail", "full"].iter().all(|p| pos.contains(p))); // 頭から はおまかせと同じ範囲なのでまとめられる
    let ranges: HashSet<(i64, i64)> = c.iter().map(|x| (x.use_.bar0, x.use_.bar1)).collect();
    assert_eq!(ranges.len(), c.len()); // 同じ範囲はまとめる
}

#[test]
fn segment_intro_full_custom_start_at_zero() {
    let _g = lock();
    let t = &*LATE_START;
    let intro = var(1008, t, Some(180.0), "intro").unwrap().use_;
    assert!(intro.t0 == 0.0 && 150.0 <= intro.t1 && intro.t1 <= 210.0); // 冒頭（0:00）から
    let full = var(1008, t, None, "full").unwrap().use_;
    assert_eq!((full.t0, full.t1), (0.0, 400.0)); // 曲全体は 0:00〜最後
    let cu = segments::variant(1008, t, None, Some("custom"), Some(0.0), Some(120.4)).unwrap();
    assert!(cu.use_.t0 == 0.0 && (cu.use_.t1 - 119.5).abs() < 0.01 && cu.seg.pos == "custom"); // 一番近い小節頭
    let cu2 = segments::variant(1008, t, None, Some("custom"), Some(60.2), Some(400.0)).unwrap().use_;
    assert!((cu2.t0 - 59.5).abs() < 0.01 && cu2.t1 == 400.0); // 小節にそろえる・最後はそのまま
    assert!(segments::variant(1008, t, None, Some("custom"), Some(50.0), Some(60.0)).is_none()); // 短すぎる区間は使わない
    let labels: Vec<String> = segments::choices(1008, t, Some(180.0)).into_iter().map(|c| c.label).collect();
    assert!(labels.contains(&"冒頭（0:00）から".to_string()) && labels.contains(&"曲全体（0:00〜最後）".to_string()));
}

#[test]
fn segment_short_lengths_use_the_peak() {
    let _g = lock();
    // 短くたくさん繋ぐ（2026-10-25）: 30〜90 秒は「おまかせ」でも盛り上がりの所を使い、長さに近い範囲になる。
    let t = &*TRACK;
    for l in [30.0, 45.0, 60.0, 90.0] {
        let u = var(1007, t, Some(l), "auto").unwrap().use_;
        assert!(0.6 * l <= u.t1 - u.t0 && u.t1 - u.t0 <= 1.4 * l, "{l} {u:?}");
        assert!(196.0 <= u.t0 && u.t1 <= 284.0, "{l} {u:?}"); // 盛り上がり（200〜280 秒）の中
        assert!(u.notes[0].starts_with("盛り上がり中心に約") && u.notes[0].contains('秒'), "{l} {u:?}");
    }
    assert_eq!(var(1007, t, Some(30.0), "head").unwrap().use_.t0, 0.0);
}

// ---------------------------------------------------------------- カットイン（test_cutin.py）
fn cp_analysis(n: usize, bl: f64) -> (Analysis, BarFeats, Vec<Section>, stems::StemLevels) {
    let a: Analysis = serde_json::from_value(json!({
        "bars": (0..n).map(|i| json!({"beat": 4 * i, "time": i as f64 * bl, "conf": 1.0})).collect::<Vec<_>>(),
        "duration": n as f64 * bl, "no_beat": [], "meter": 4, "bpm": 240.0 / bl, "beats": [],
    }))
    .unwrap();
    let mut feats: BarFeats = vec![[0f32; 15]; n];
    for (i, row) in feats.iter_mut().enumerate() {
        row[0] = if (16..24).contains(&i) { 0.2 } else { 0.05 }; // 16〜23 小節が盛り上がり（サビ）
    }
    let sec = |b: usize| Section { bar0: b, bar1: b, t0: 0.0, t1: 0.0, bars: 0, tags: vec![] };
    let segs = vec![sec(0), sec(16), sec(24), sec(40)];
    let mut voc = vec![[0f32; 4]; n];
    for row in voc[17..23].iter_mut() {
        row[0] = 0.8; // サビの中だけ歌う
    }
    (a, feats, segs, voc)
}

fn row(id: i64, u: Use, pos: &str) -> TrackRow {
    TrackRow { id, use_: u, seg: Seg { pos: pos.into(), ..Default::default() }, ..Default::default() }
}

#[test]
fn planner_make_cut_snaps_to_clean_points() {
    let _g = lock();
    let (a, feats, segs, voc) = cp_analysis(48, 3.0);
    let cp = cutpoints::compute(&a, &feats, &segs, Some(&voc));
    let mut ta = row(1, Use { t0: 0.0, t1: 78.0, bar0: 0, bar1: 26, notes: vec![] }, "auto");
    let mut tb = row(2, Use { t0: 42.0, t1: 135.0, bar0: 14, bar1: 45, notes: vec![] }, "auto");
    let mut j = Join {
        tpl: "echo".into(),
        bars: 2,
        params: JoinParams { tempo: "sync".into(), loop_: 0, shift: 1, ..Default::default() },
        why: vec!["拍を合わせられないのでエコーアウト".into()],
        ..Default::default()
    };
    assert!(engine::make_cut_with(&mut ta, &mut tb, &mut j, false, Some(&cp), Some(&cp)));
    assert!(j.tpl == "cut" && j.bars == 0 && j.mode.as_deref() == Some("cut") && j.params.shift == 0);
    assert!(ta.use_.bar1 == 24 && ta.use_.t1 == 72.0); // サビの切れ目で切る
    assert_eq!(tb.use_.bar0, 16); // サビ頭から入る
    assert!(!j.why.iter().any(|w| w.starts_with("拍を合わせられない")));
    // 自分で選んだ区間は動かさない
    let mut ta2 = row(1, Use { t0: 0.0, t1: 78.0, bar0: 0, bar1: 26, notes: vec![] }, "custom");
    let mut tb2 = tb.clone();
    let mut j2 = Join { tpl: "long".into(), bars: 16, ..Default::default() };
    assert!(engine::make_cut_with(&mut ta2, &mut tb2, &mut j2, true, Some(&cp), Some(&cp)));
    assert_eq!(ta2.use_.bar1, 26);
    // 書き出す JSON の params（Python 版と同じキー）
    assert_eq!(
        serde_json::to_value(&j.params).unwrap(),
        json!({"tempo": "sync", "loop": 0, "shift": 0, "parts": "none", "cutTrim": "auto"})
    );
}

#[test]
fn planner_join_modes() {
    let _g = lock();
    // 切り替えポイントが分からないとき、おまかせはカットインにしない（エコーのまま）。指定されたらカットインにする
    let ta = row(1, Use { t0: 0.0, t1: 52.0, bar0: 0, bar1: 26, notes: vec![] }, "");
    let tb = row(2, Use { t0: 0.0, t1: 90.0, bar0: 0, bar1: 45, notes: vec![] }, "");
    let j = Join { tpl: "echo".into(), bars: 2, ..Default::default() };
    assert!(!engine::make_cut_with(&mut ta.clone(), &mut tb.clone(), &mut j.clone(), false, None, None));
    let mut j2 = j.clone();
    assert!(engine::make_cut_with(&mut ta.clone(), &mut tb.clone(), &mut j2, true, None, None) && j2.tpl == "cut");
}
