//! 起動（djmix/__main__.py）: サーバーを裏のスレッドで立ち上げ、専用ウィンドウ（WKWebView）で画面を開く。
//!
//!     djmix              # 専用ウィンドウ（通常。アプリの DJ Mixer.app から起動するのと同じ）
//!     djmix --browser    # 既定のブラウザで開く（開発用）
//!     djmix --server     # サーバーだけ起動する（テスト用）
//!     --port N（0 なら空きポート）、--debug（Web インスペクタを有効にする）
use djmix::config::{self, APP_TITLE, HOST};
use djmix::server::{router, AppState};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

struct Args {
    port: u16,
    browser: bool,
    server: bool,
    debug: bool,
}

fn parse_args() -> Args {
    let mut a = Args { port: 0, browser: false, server: false, debug: false };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--port" => a.port = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--browser" => a.browser = true,
            "--server" => a.server = true,
            "--debug" => a.debug = true,
            "-h" | "--help" => {
                println!("djmix [--browser | --server] [--port N] [--debug]");
                std::process::exit(0);
            }
            // Finder から起動したときに付くことがある引数（-psn_…）は無視する
            _ => {}
        }
    }
    a
}

fn wait_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if let Ok(mut s) = TcpStream::connect_timeout(&format!("{HOST}:{port}").parse().unwrap(), Duration::from_secs(1)) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = write!(s, "GET /api/health HTTP/1.0\r\nHost: {HOST}:{port}\r\n\r\n");
            let mut buf = [0u8; 64];
            if let Ok(n) = s.read(&mut buf) {
                if String::from_utf8_lossy(&buf[..n]).contains(" 200 ") {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("サーバーが {} 秒以内に起動しませんでした", timeout.as_secs()))
}

/// もう起動しているか（データのフォルダのロックで見る。二重に起動すると解析や DB がぶつかる）
fn single_instance() -> Option<std::fs::File> {
    let p = config::paths().data_dir.join(".lock");
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p).ok()?;
    use std::os::unix::io::AsRawFd;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (r == 0).then_some(f)
}

#[allow(dead_code)]
fn splash() -> String {
    use base64::Engine;
    let icon = config::paths().resources_dir.join("assets").join("icons").join("a").join("96.png");
    let img = std::fs::read(icon)
        .map(|b| {
            format!(
                r#"<img src="data:image/png;base64,{}" width="96" height="96">"#,
                base64::engine::general_purpose::STANDARD.encode(b)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><style>
html,body{{margin:0;height:100%;background:#0b0c0e;color:#e8e8e8;font-family:"Hiragino Sans","Hiragino Kaku Gothic ProN",-apple-system,sans-serif}}
.c{{height:100%;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:18px}}
.t{{font-size:20px;letter-spacing:.06em}}.s{{color:#8a9099;font-size:13px}}
.bar{{width:260px;height:4px;border-radius:2px;background:#22262c;overflow:hidden}}
.bar i{{display:block;width:40%;height:100%;background:linear-gradient(90deg,#22d3ee,#fb923c);animation:m 1.2s ease-in-out infinite alternate}}
@keyframes m{{from{{margin-left:0}}to{{margin-left:60%}}}}
</style></head><body><div class="c">{img}<div class="t">DJ Mixer を起動しています…</div>
<div class="bar"><i></i></div><div class="s" id="s">0 秒</div></div>
<script>const t0=Date.now();setInterval(()=>{{document.getElementById('s').textContent=Math.floor((Date.now()-t0)/1000)+' 秒'}},500);</script>
</body></html>"#
    )
}

fn start_server(state: Arc<AppState>, listener: std::net::TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio");
            rt.block_on(async move {
                listener.set_nonblocking(true).ok();
                let l = tokio::net::TcpListener::from_std(listener).expect("listener");
                state.startup();
                if let Err(e) = axum::serve(l, router(state.clone())).await {
                    log::error!("server stopped: {e}");
                }
            });
        })
        .expect("server thread")
}

fn main() {
    let args = parse_args();
    let p = config::paths();
    let _ = std::fs::create_dir_all(&p.data_dir);
    // ターミナルから起動したときだけ標準エラーにも書く
    djmix::logging::init(unsafe { libc::isatty(2) } == 1 || args.server || args.browser);
    config::seed_mappings();
    log::info!("DJ Mixer {} data={} resources={}", config::VERSION, p.data_dir.display(), p.resources_dir.display());

    let _lock = match single_instance() {
        Some(l) => l,
        None => {
            log::warn!("already running");
            eprintln!("DJ Mixer はもう起動しています（{}）", p.data_dir.display());
            std::process::exit(0);
        }
    };
    let listener = match std::net::TcpListener::bind((HOST, args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ポート {} を開けません: {e}", args.port);
            std::process::exit(1);
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(args.port);
    let url = format!("http://{HOST}:{port}");
    let state = AppState::new();

    if args.server {
        log::info!("server only: {url}");
        start_server(state, listener).join().ok();
        return;
    }
    let th = start_server(state.clone(), listener);
    if args.browser || !cfg!(feature = "window") {
        if let Err(e) = wait_ready(port, STARTUP_TIMEOUT) {
            log::error!("startup failed: {e}");
            std::process::exit(1);
        }
        log::info!("server ready: {url}");
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
        th.join().ok();
        return;
    }
    #[cfg(feature = "window")]
    {
        let st = state.clone();
        let st_exit = state.clone();
        djmix_window::run(
            djmix_window::Options {
                title: APP_TITLE.into(),
                splash_html: splash(),
                width: 1600.0,
                height: 1000.0,
                min_width: 1280.0,
                min_height: 800.0,
                debug: args.debug,
            },
            move |h| {
                let hd = h.clone();
                *st.dialog.lock() = Some(Arc::new(move || hd.pick_folder().ok()));
                match wait_ready(port, STARTUP_TIMEOUT) {
                    Ok(()) => {
                        log::info!("server ready: {url}");
                        h.load_url(&url);
                    }
                    Err(e) => {
                        log::error!("startup failed: {e}");
                        h.fail(&format!(
                            "DJ Mixer を起動できませんでした。\n\n{e}\n\n詳しくは {} を見てください。",
                            config::paths().data_dir.join("djmix.log").display()
                        ));
                    }
                }
            },
            move || st_exit.shutdown(),
        );
    }
    #[allow(unreachable_code)]
    {
        let _ = APP_TITLE;
    }
}
