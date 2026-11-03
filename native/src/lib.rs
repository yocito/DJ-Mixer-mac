//! DJ Mixer（Mac 版）。Windows 版（Python / FastAPI）の djmix パッケージを Rust に移植したもの。
//!
//! 画面（frontend/。HTML と Web Audio の再生エンジン）はそのまま使い、このクレートが同じ API を 127.0.0.1 で提供する。
pub mod config;
pub mod db;
pub mod edits;
pub mod prefs;
pub mod settings;
pub mod util;

pub mod audio {
    pub mod decode;
}

pub mod analysis {
    pub mod bars;
    pub mod beats;
    pub mod cache;
    pub mod cutpoints;
    pub mod sections;
    pub mod stems;
    pub mod types;
}

pub mod library {
    pub mod itunes;
    pub mod roots;
    pub mod textfix;
}

pub mod planner {
    pub mod genres;
}
