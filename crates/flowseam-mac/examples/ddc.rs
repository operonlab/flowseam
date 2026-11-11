//! Hardware probe for `flowseam_mac::ddc`.
//!
//!   cargo run -p flowseam-mac --example ddc -- list
//!   cargo run -p flowseam-mac --example ddc -- raw  <uuid> <vcp>
//!   cargo run -p flowseam-mac --example ddc -- flip <uuid> <to> <home> <hold-secs>
//!
//! `flip` takes the display away for `hold-secs`. Run it under an external watchdog that restores
//! the input: the revert here is what is being tested, so it cannot be its own safety net.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ddc: macOS only");
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use flowseam_mac::ddc::{self, mac, Readback};
    use std::process::ExitCode;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |s: &str| -> u16 {
        s.strip_prefix("0x").map_or_else(|| s.parse(), |h| u16::from_str_radix(h, 16)).unwrap_or_else(|_| panic!("not a number: {s}"))
    };
    let open = |uuid: &str| mac::open(uuid).unwrap_or_else(|e| panic!("open {uuid}: {e:?}"));

    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["list"] => {
            for m in mac::online().expect("online displays") {
                let ddc = match mac::open(&m.uuid) {
                    Ok(d) => format!("input={:?}", d.get(ddc::VCP_INPUT)),
                    Err(e) => format!("{e:?}"),
                };
                println!("{} {} {:?} {:?} {ddc}", m.id, m.uuid, m.name, m.edid);
            }
            ExitCode::SUCCESS
        }
        ["caps", uuid] => {
            let caps = open(uuid).capabilities();
            println!("caps={caps:?}");
            println!("inputs={:?}", caps.as_deref().ok().and_then(ddc::input_sources));
            ExitCode::SUCCESS
        }
        ["probe", uuid, input, home] => {
            use flowseam_mac::kvm::{mac::run_now, Cmd, Monitor};
            let monitor = Monitor { uuid: uuid.to_string(), input: num(input), home: num(home) };
            println!("{:?}", run_now(Cmd::Probe { peer: "probe".into(), monitor }));
            ExitCode::SUCCESS
        }
        ["capsraw", uuid, offset] => {
            println!("raw={:02X?}", open(uuid).caps_raw(num(offset)));
            ExitCode::SUCCESS
        }
        ["raw", uuid, vcp] => {
            let d = open(uuid);
            let vcp = num(vcp) as u8;
            let raw = d.get_raw(vcp);
            println!("raw={raw:02X?}");
            println!("parsed={:?}", raw.and_then(|b| ddc::parse_reply(vcp, &b)));
            ExitCode::SUCCESS
        }
        ["flip", uuid, to, home, hold] => {
            let (d, to, home) = (open(uuid), num(to), num(home));
            let hold = Duration::from_secs(num(hold).into());
            let t0 = Instant::now();
            let ms = || t0.elapsed().as_millis();
            let read = || d.get(ddc::VCP_INPUT).map(|r| r.current);
            let pause = || sleep(ddc::CONFIRM_INTERVAL);

            println!("{:>6}ms set {to}: {:?}", ms(), d.set(ddc::VCP_INPUT, to));
            let away = ddc::confirm(read, to, ddc::CONFIRM_ATTEMPTS, pause);
            println!("{:>6}ms away: {away:?}", ms());
            sleep(hold.saturating_sub(t0.elapsed()));
            // Always sent, whatever happened above.
            println!("{:>6}ms set {home}: {:?}", ms(), d.set(ddc::VCP_INPUT, home));
            let back = ddc::confirm(read, home, ddc::CONFIRM_ATTEMPTS, pause);
            println!("{:>6}ms home: {back:?}", ms());

            match (away, back) {
                (Readback::Confirmed(_), Readback::Confirmed(_)) => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        _ => {
            eprintln!("usage: ddc list | raw <uuid> <vcp> | flip <uuid> <to> <home> <hold-secs>");
            ExitCode::from(2)
        }
    }
}
