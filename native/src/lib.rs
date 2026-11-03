//! DJ Mixer（Mac 版）。Windows 版（Python / FastAPI）の djmix パッケージを Rust に移植したもの。
//!
//! 画面（frontend/。HTML と Web Audio の再生エンジン）はそのまま使い、このクレートが同じ API を 127.0.0.1 で提供する。
pub mod config;
pub mod db;
pub mod edits;
pub mod logging;
pub mod midi;
pub mod mixes;
pub mod prefs;
pub mod queues;
pub mod server;
pub mod server_ext;
pub mod settings;
pub mod stubs;
pub mod util;

pub mod audio {
    pub mod decode;
    pub mod pcm;
}

pub mod analysis {
    pub mod bars;
    pub mod beats;
    pub mod bulk;
    pub mod cache;
    pub mod cutpoints;
    pub mod pipeline;
    pub mod sections;
    pub mod stems;
    pub mod types;
    pub mod worker;
}

pub mod library {
    pub mod itunes;
    pub mod query;
    pub mod roots;
    pub mod scanner;
    pub mod textfix;
}

pub mod planner {
    pub mod genres;
}
