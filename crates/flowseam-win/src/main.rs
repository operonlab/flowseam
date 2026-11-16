use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use flowseam_core::proto::{Point, Rect};
use flowseam_win::app;
use flowseam_win::clip_os::ClipThread;
use flowseam_win::inject::PrintInjector;
use flowseam_win::Clipboard;

const USAGE: &str = "usage:
  flowseam-win pair-serve --config P [--peer NAME] [--peer-addr HOST:PORT]
  flowseam-win pair --config P --addr HOST:PORT --code 123456 [--peer NAME]
  flowseam-win run --config P [--stdio] [--max-secs N]

  run           listens on `listen` and dials every paired peer (--listen is accepted and changes nothing)

  --stdio       fake clipboard and input: stdin lines set the clipboard, and what would be
                injected is printed as INJECT lines instead (for tests; moves nothing)

How the controller's modifier keys land here is chosen on the controller, per device.";

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match real_main(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("flowseam-win: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).ok_or(USAGE)?;
    let config = PathBuf::from(opt(args, "--config").ok_or_else(|| format!("--config is required\n{USAGE}"))?);
    match cmd {
        "pair-serve" => app::pair_serve(
            &config,
            &opt(args, "--peer").unwrap_or_else(|| "peer".into()),
            &opt(args, "--peer-addr").unwrap_or_else(|| "0.0.0.0:24800".into()),
        ),
        "pair" => app::pair_dial(
            &config,
            &opt(args, "--peer").unwrap_or_else(|| "peer".into()),
            &opt(args, "--addr").ok_or("--addr is required")?,
            &opt(args, "--code").ok_or("--code is required")?,
        ),
        "run" => {
            let max = match opt(args, "--max-secs") {
                Some(n) => Some(std::time::Duration::from_secs(n.parse().map_err(|_| "--max-secs must be a number")?)),
                None => None,
            };
            app::run_until(&config, machine(flag(args, "--stdio"))?, max)
        }
        _ => Err(USAGE.into()),
    }
}

/// This PC's own input, clipboard and adapters; under `--stdio` (and on other hosts) the stand-ins: stdin
/// lines set the clipboard and what would be injected is printed, on this PC's displays or a fixed 1920×1080
/// screen elsewhere.
fn machine(stdio: bool) -> Result<app::Machine, String> {
    #[cfg(windows)]
    if !stdio {
        return app::this_pc();
    }
    let _ = stdio;
    let shared = flowseam_win::stdio::StdioClipboard::spawn_stdin_reader();
    let clipboard = ClipThread::start(move || Ok(Box::new(shared) as Box<dyn Clipboard>))?;
    #[cfg(windows)]
    let displays: Arc<dyn Fn() -> Result<Vec<Rect>, String> + Send + Sync> = Arc::new(flowseam_win::win_input::displays);
    #[cfg(not(windows))]
    let displays: Arc<dyn Fn() -> Result<Vec<Rect>, String> + Send + Sync> = Arc::new(|| Ok(vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }]));
    let start = displays()?.first().map_or(Point { x: 0, y: 0 }, |r| Point { x: r.x + r.w / 2, y: r.y + r.h / 2 });
    Ok(app::Machine {
        injector: Box::new(PrintInjector::new(start)),
        displays,
        clipboard: Some(Arc::new(clipboard)),
        link: app::link_host(),
        drags: app::drag_dirs(),
        capturing: false,
    })
}
