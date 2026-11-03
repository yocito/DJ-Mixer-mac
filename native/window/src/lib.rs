//! DJ Mixer の専用ウィンドウ（Windows 版の pywebview の代わり）。
//!
//! - tao のウィンドウに wry の WebView（Mac は WKWebView）を載せて、ローカルのサーバーの画面を開く
//! - 起動中は「起動しています…」の画面をすぐに出し、サーバーが立ち上がったらアプリの画面に切り替える
//! - Mac のメニュー（アプリ・編集・表示・ウィンドウ）。編集メニューが無いと入力欄で ⌘C / ⌘V が効かない
//! - フォルダ選択（NSOpenPanel）はメインスレッドで開く必要があるので、イベントループへ頼んで結果を返す
use std::sync::mpsc;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

pub struct Options {
    pub title: String,
    pub splash_html: String,
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
    pub debug: bool,
}

enum Cmd {
    Load(String),
    PickFolder(mpsc::Sender<Option<String>>),
    Error(String),
    Quit,
}

/// 別のスレッドからウィンドウに頼む窓口
#[derive(Clone)]
pub struct Handle {
    proxy: EventLoopProxy<Cmd>,
}

// EventLoopProxy は Send だが Sync でないので、使う側で Mutex に入れなくて済むよう送るたびに複製する
unsafe impl Sync for Handle {}

impl Handle {
    /// 画面を開き直す（サーバーが立ち上がったとき）
    pub fn load_url(&self, url: &str) {
        let _ = self.proxy.send_event(Cmd::Load(url.to_string()));
    }

    /// フォルダを選んでもらう。None = キャンセル。ウィンドウが無くなっていたら Err。
    pub fn pick_folder(&self) -> Result<Option<String>, ()> {
        let (tx, rx) = mpsc::channel();
        self.proxy.send_event(Cmd::PickFolder(tx)).map_err(|_| ())?;
        rx.recv().map_err(|_| ())
    }

    /// 起動の失敗を知らせて閉じる
    pub fn fail(&self, msg: &str) {
        let _ = self.proxy.send_event(Cmd::Error(msg.to_string()));
    }

    pub fn quit(&self) {
        let _ = self.proxy.send_event(Cmd::Quit);
    }
}

fn build_menu(title: &str) -> muda::Menu {
    use muda::{AboutMetadata, Menu, PredefinedMenuItem, Submenu};
    let menu = Menu::new();
    let app = Submenu::new(title, true);
    let _ = app.append_items(&[
        &PredefinedMenuItem::about(None, Some(AboutMetadata { name: Some(title.to_string()), ..Default::default() })),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::services(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::quit(None),
    ]);
    let edit = Submenu::new("編集", true);
    let _ = edit.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ]);
    let view = Submenu::new("表示", true);
    let _ = view.append_items(&[&PredefinedMenuItem::fullscreen(None)]);
    let window = Submenu::new("ウィンドウ", true);
    let _ = window.append_items(&[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(None)]);
    let _ = menu.append_items(&[&app, &edit, &view, &window]);
    menu
}

/// ウィンドウを開いてイベントループを回す（メインスレッドから呼ぶ。閉じるまで戻らない）。
/// on_ready には別のスレッドから使える窓口が渡る（サーバーの起動を待って load_url する）。
pub fn run(opts: Options, on_ready: impl FnOnce(Handle) + Send + 'static, on_exit: impl FnOnce() + 'static) -> ! {
    let event_loop = EventLoopBuilder::<Cmd>::with_user_event().build();
    let handle = Handle { proxy: event_loop.create_proxy() };
    let menu = build_menu(&opts.title);
    #[cfg(target_os = "macos")]
    menu.init_for_nsapp();
    let window = WindowBuilder::new()
        .with_title(&opts.title)
        .with_inner_size(LogicalSize::new(opts.width, opts.height))
        .with_min_inner_size(LogicalSize::new(opts.min_width, opts.min_height))
        .build(&event_loop)
        .expect("ウィンドウを作れませんでした");
    let webview = WebViewBuilder::new()
        .with_html(opts.splash_html.clone())
        // 自分のアプリの画面なので、クリック前でも音声を開始できるよう自動再生の制限を外す
        .with_autoplay(true)
        .with_devtools(opts.debug)
        .with_background_color((11, 12, 14, 255))
        // 外のサイトへのリンクは既定のブラウザで開く（アプリの中では開かない）
        .with_navigation_handler(|url| {
            let local = url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost") || url.starts_with("about:")
                || url.starts_with("data:");
            if !local && (url.starts_with("http://") || url.starts_with("https://")) {
                let _ = std::process::Command::new("open").arg(&url).spawn();
                return false;
            }
            true
        })
        .build(&window)
        .expect("WebView を作れませんでした");
    std::thread::spawn(move || on_ready(handle));
    let mut on_exit = Some(on_exit);
    let _menu = menu;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Cmd::Load(url)) => {
                if let Err(e) = webview.load_url(&url) {
                    log::error!("load_url failed: {e}");
                }
            }
            Event::UserEvent(Cmd::PickFolder(tx)) => {
                let r = rfd::FileDialog::new()
                    .set_title("音楽フォルダを選ぶ")
                    .set_parent(&window)
                    .pick_folder()
                    .map(|p| p.to_string_lossy().into_owned());
                let _ = tx.send(r);
            }
            Event::UserEvent(Cmd::Error(msg)) => {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title(&opts.title)
                    .set_description(&msg)
                    .set_buttons(rfd::MessageButtons::Ok)
                    .show();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(Cmd::Quit) => *control_flow = ControlFlow::Exit,
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *control_flow = ControlFlow::Exit,
            Event::LoopDestroyed => {
                if let Some(f) = on_exit.take() {
                    f();
                }
            }
            _ => {}
        }
    })
}
