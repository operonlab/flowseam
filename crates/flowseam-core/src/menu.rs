//! The window's stdin lines to a worker, one grammar for every platform. Pure: the worker decides what each does.

/// A line from the window: `switch <uuid>[,<uuid>…] <peer>` moves those monitors of the peer and then the
/// input, `input <peer>` moves only the keyboard and mouse, `return <peer>` brings every monitor configured
/// for the peer home (one may have changed input by itself). From the settings: `inspect <uuid>` reads a
/// monitor's inputs, and `test <token> <uuid> <input> <home> <peer>` shows a candidate input for a while and
/// comes back; its result carries `token`, so the window asks only about the test it started. The peer
/// comes last because a name may hold spaces.
#[derive(Debug, PartialEq, Eq)]
pub enum MenuCmd<'a> {
    Switch { uuids: Vec<&'a str>, peer: &'a str },
    Input(&'a str),
    Return(&'a str),
    Inspect(&'a str),
    /// The tray's "back": hand control back to whoever drives this Mac and ask its peers to bring their monitors home.
    Home,
    /// Monitors as (uuid, candidate input, this Mac's input).
    Test { token: u64, monitors: Vec<(&'a str, u16, u16)>, peer: &'a str },
}

pub fn menu_command(line: &str) -> Option<MenuCmd<'_>> {
    fn rest(s: &str) -> Option<&str> {
        Some(s.trim()).filter(|p| !p.is_empty())
    }
    let line = line.trim();
    if line == "home" {
        return Some(MenuCmd::Home);
    }
    let (verb, args) = line.split_once(' ')?;
    match verb {
        "switch" => {
            let (uuids, peer) = args.trim().split_once(' ')?;
            let uuids: Vec<&str> = uuids.split(',').filter(|u| !u.is_empty()).collect();
            if uuids.is_empty() {
                return None;
            }
            Some(MenuCmd::Switch { uuids, peer: rest(peer)? })
        }
        "input" => rest(args).map(MenuCmd::Input),
        "return" => rest(args).map(MenuCmd::Return),
        "inspect" => rest(args).map(MenuCmd::Inspect),
        // `test <token> <uuid>:<input>:<home>[,…] <peer>`: the peer last, since a name may hold spaces.
        "test" => {
            let mut it = args.trim().splitn(3, ' ');
            let token = it.next()?.parse().ok()?;
            let monitors = it
                .next()?
                .split(',')
                .map(|m| {
                    let mut f = m.split(':');
                    let monitor = (f.next().filter(|u| !u.is_empty())?, f.next()?.parse().ok()?, f.next()?.parse().ok()?);
                    f.next().is_none().then_some(monitor)
                })
                .collect::<Option<Vec<_>>>()?;
            let peer = rest(it.next()?)?;
            Some(MenuCmd::Test { token, monitors, peer })
        }
        _ => None,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_lines_name_the_peer_and_what_to_move() {
        assert_eq!(menu_command("switch U,V my pc\n"), Some(MenuCmd::Switch { uuids: vec!["U", "V"], peer: "my pc" }));
        assert_eq!(menu_command("switch U pc\n"), Some(MenuCmd::Switch { uuids: vec!["U"], peer: "pc" }));
        assert_eq!(menu_command("switch pc\n"), None, "no monitor named: the window sends `input` for that");
        assert_eq!(menu_command("switch , pc\n"), None);
        assert_eq!(menu_command("switch \n"), None);
        assert_eq!(menu_command("  input  my pc \r\n"), Some(MenuCmd::Input("my pc")));
        assert_eq!(menu_command("return my pc\n"), Some(MenuCmd::Return("my pc")));
        assert_eq!(menu_command("return \n"), None);
        assert_eq!(menu_command("home\n"), Some(MenuCmd::Home), "the tray's back button");
        assert_eq!(menu_command(" home \r\n"), Some(MenuCmd::Home));
        assert_eq!(menu_command("inspect U-1\n"), Some(MenuCmd::Inspect("U-1")));
        assert_eq!(menu_command("test 77 U-1:15:18 my pc\n"), Some(MenuCmd::Test { token: 77, monitors: vec![("U-1", 15, 18)], peer: "my pc" }));
        assert_eq!(menu_command("test 77 U-1:15:18,V-2:17:19 pc\n"), Some(MenuCmd::Test { token: 77, monitors: vec![("U-1", 15, 18), ("V-2", 17, 19)], peer: "pc" }));
        for bad in ["test 77 U-1:15:x pc", "test 77 U-1:15:18", "test 77 U-1:15 pc", "test 77 U-1:15:18:9 pc", "test 77 U-1:15:18, pc", "test 77 :15:18 pc", "test x U-1:15:18 pc"] {
            assert_eq!(menu_command(&format!("{bad}\n")), None, "{bad}");
        }
    }
}
