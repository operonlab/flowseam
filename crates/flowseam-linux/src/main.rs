use std::path::PathBuf;
use std::process::ExitCode;

use flowseam_linux::app;

const USAGE: &str = "usage:
  flowseam-linux pair-serve --config P [--peer NAME] [--peer-addr HOST:PORT]
  flowseam-linux pair --config P --addr HOST:PORT --code 123456 [--peer NAME]
  flowseam-linux run --config P --stdio [--max-secs N]

  run           listens on `listen` and dials every paired peer
  --stdio       required for now: stdin lines set the clipboard, and what would be injected is printed
                as INJECT lines (this slice has no real input backend; it moves nothing)";

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
            eprintln!("flowseam-linux: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).ok_or(USAGE)?;
    let config = PathBuf::from(opt(args, "--config").ok_or_else(|| format!("--config is required\n{USAGE}"))?);
    match cmd {
        "pair-serve" => app::pair_serve(&config, &opt(args, "--peer").unwrap_or_else(|| "peer".into()), &opt(args, "--peer-addr").unwrap_or_else(|| "0.0.0.0:24800".into())),
        "pair" => app::pair_dial(&config, &opt(args, "--peer").unwrap_or_else(|| "peer".into()), &opt(args, "--addr").ok_or("--addr is required")?, &opt(args, "--code").ok_or("--code is required")?),
        "run" => {
            if !flag(args, "--stdio") {
                return Err(format!("run needs --stdio: there is no real input backend yet\n{USAGE}"));
            }
            if let Some(n) = opt(args, "--max-secs") {
                let n: u64 = n.parse().map_err(|_| "--max-secs must be a number")?;
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(n));
                    app::log("max-secs reached, exiting");
                    std::process::exit(0);
                });
            }
            app::run(&config, app::headless())
        }
        _ => Err(USAGE.into()),
    }
}
