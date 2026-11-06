use std::path::PathBuf;
use std::process::ExitCode;

use flowseam_core::proto::{Point, Rect};
use flowseam_win::app::{self, Host};
use flowseam_win::inject::{Injector, PrintInjector};
use flowseam_win::Clipboard;

const USAGE: &str = "usage:
  flowseam-win pair-serve --config P [--peer NAME] [--peer-addr HOST:PORT]
  flowseam-win pair --config P --addr HOST:PORT --code 123456 [--peer NAME]
  flowseam-win run --config P [--listen] [--stdio] [--max-secs N]

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
            if let Some(n) = opt(args, "--max-secs") {
                let n: u64 = n.parse().map_err(|_| "--max-secs must be a number")?;
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(n));
                    app::log("max-secs reached, exiting");
                    std::process::exit(0);
                });
            }
            let stdio = flag(args, "--stdio");
            #[cfg(windows)]
            let (clipboard, injector, displays) = if stdio {
                let (injector, displays) = input_factory(true)?;
                (clipboard_factory(true), injector, displays)
            } else {
                flowseam_win::real_host_parts()
            };
            #[cfg(not(windows))]
            let (clipboard, injector, displays) = {
                let clipboard = clipboard_factory(stdio);
                let (injector, displays) = input_factory(stdio)?;
                (clipboard, injector, displays)
            };
            let host = Host {
                clipboard: &*clipboard,
                injector: &*injector,
                displays: &*displays,
            };
            if flag(args, "--listen") {
                app::run_serve(&config, &host)
            } else {
                app::run_dial(&config, &host)
            }
        }
        _ => Err(USAGE.into()),
    }
}

type InjectorFactory = Box<dyn Fn() -> Box<dyn Injector> + Sync>;
type DisplayFactory = Box<dyn Fn() -> Result<Vec<Rect>, String> + Sync>;

/// Real SendInput and monitor list on Windows; the printing stand-in (on a fixed 1920×1080 screen when
/// there is no real one to ask) under `--stdio` or on other hosts.
fn input_factory(force_stdio: bool) -> Result<(InjectorFactory, DisplayFactory), String> {
    #[cfg(windows)]
    {
        if !force_stdio {
            let (_, injector, displays) = flowseam_win::real_host_parts();
            return Ok((injector, displays));
        }
        let displays: DisplayFactory = Box::new(flowseam_win::win_input::displays);
        let start = displays()?.first().map_or(Point { x: 0, y: 0 }, |r| Point { x: r.x + r.w / 2, y: r.y + r.h / 2 });
        Ok((Box::new(move || Box::new(PrintInjector::new(start)) as Box<dyn Injector>), displays))
    }
    #[cfg(not(windows))]
    {
        let _ = force_stdio;
        let screen = Rect { x: 0, y: 0, w: 1920, h: 1080 };
        Ok((
            Box::new(move || Box::new(PrintInjector::new(Point { x: 960, y: 540 })) as Box<dyn Injector>),
            Box::new(move || Ok(vec![screen])),
        ))
    }
}

type Factory = Box<dyn Fn() -> Result<Box<dyn Clipboard>, String> + Sync>;

fn clipboard_factory(force_stdio: bool) -> Factory {
    let shared = flowseam_win::stdio::StdioClipboard::spawn_stdin_reader();
    let _ = force_stdio;
    Box::new(move || Ok(Box::new(shared.clone()) as Box<dyn Clipboard>))
}
