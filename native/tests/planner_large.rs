//! おまかせ（planner）を Windows 版（Python）と照合する（大きめ: 1000 曲。候補を 600 曲に絞る所・時間も見る）。
#[path = "fixtures/planner/synth.rs"]
mod synth;

use djmix::planner::{desc, engine};

#[test]
fn large_library_matches_python_and_is_fast() {
    synth::init_data_dir();
    let exp = synth::load_expected("expected_large.json.gz");
    synth::build(exp["n"].as_i64().unwrap(), exp["lib"].as_i64().unwrap());
    let t0 = std::time::Instant::now();
    desc::backfill().unwrap();
    eprintln!("backfill: {:.3}s", t0.elapsed().as_secs_f64());
    let t0 = std::time::Instant::now();
    let pool = engine::get_pool();
    eprintln!("pool ({} 曲): {:.3}s", pool.len(), t0.elapsed().as_secs_f64());
    let (errs, times) = synth::check_scenarios(&exp);
    for (k, t) in &times {
        eprintln!("{k}: {t:.3}s");
    }
    if !errs.is_empty() {
        for e in errs.iter().take(80) {
            eprintln!("{e}");
        }
        panic!("Python 版との違い {} 件", errs.len());
    }
    // 候補を 600 曲に絞る所を通っていること
    let capped = exp["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["fn"] == "plan_library" && s["result"]["candidates"] == engine::MAX_CANDS);
    assert!(capped, "候補が 600 曲になる場面が無い");
    // 600 曲の候補から 3 案（リリースビルドでは 1 秒よりずっと短い。デバッグビルドでは目安を緩める）
    let limit = if cfg!(debug_assertions) { 30.0 } else { 1.0 };
    for (k, t) in times.iter().filter(|(k, _)| k.contains("plan_library")) {
        assert!(*t < limit, "{k} が遅い: {t:.3}s");
    }
}
