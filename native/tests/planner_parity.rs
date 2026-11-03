//! おまかせ（planner）を Windows 版（Python）と照合する: 合成の 64 曲のライブラリ（fixtures/planner/gen.py と同じもの）で、
//! 要約（track_desc）と、plan_library / plan_albums / plan_queue / next_candidates / rejoin / segment_choices / candidate_facets の結果。
#[path = "fixtures/planner/synth.rs"]
mod synth;

use djmix::db;
use djmix::planner::desc;
use serde_json::Value;

#[test]
fn small_library_matches_python() {
    synth::init_data_dir();
    let exp = synth::load_expected("expected_small.json.gz");
    synth::build(exp["n"].as_i64().unwrap(), exp["lib"].as_i64().unwrap());
    desc::backfill().unwrap();
    // 要約（保存する JSON）
    let got: Vec<(i64, String)> = db::with(|c| {
        let mut st = c.prepare("SELECT track_id, data FROM track_desc ORDER BY track_id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
    .unwrap();
    let want = exp["descs"].as_object().unwrap();
    let mut errs = vec![];
    let mut got_ids: Vec<i64> = got.iter().map(|(t, _)| *t).collect();
    let mut want_ids: Vec<i64> = want.keys().map(|k| k.parse().unwrap()).collect();
    got_ids.sort();
    want_ids.sort();
    assert_eq!(got_ids, want_ids, "要約のある曲");
    for (t, data) in &got {
        let g: Value = serde_json::from_str(data).unwrap();
        synth::diff(&format!("desc[{t}]"), &want[&t.to_string()], &g, &mut errs);
    }
    // 探索の結果（gen.py と同じ順に実行する。★ の読み込みなど、前の呼び出しの影響も同じになる）
    let (e2, times) = synth::check_scenarios(&exp);
    errs.extend(e2);
    for (k, t) in &times {
        if *t > 0.05 {
            eprintln!("{k}: {t:.3}s");
        }
    }
    if !errs.is_empty() {
        for e in errs.iter().take(80) {
            eprintln!("{e}");
        }
        panic!("Python 版との違い {} 件", errs.len());
    }
}
