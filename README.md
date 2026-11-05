# Flowseam

Move your mouse, keyboard, clipboard and files across the seam between your computers.

> **Status: early (0.x).** Used daily between two Macs and a Windows PC. Expect rough edges, and expect
> both ends to need the same version: machines on different protocol versions refuse to connect.

## What works today

| | Mac ↔ Mac | Mac → Windows | Windows → Mac |
|---|---|---|---|
| Mouse and keyboard across a screen edge | yes | yes | — (Windows is driven, not driving) |
| Shared clipboard (text) | yes | yes | yes |
| Drag a file over the edge, drop it on the other screen | yes | yes | not yet |

- Dropped files land in the receiving machine's **Downloads** folder.
- While a mouse button is held, the cursor only crosses the edge when you are dragging files; a window or
  a text selection stays on its screen until you let go.
- Driving Windows from a Mac can translate Mac shortcuts: ⌘C copies, ⌘Tab switches apps, ⌥ alone opens
  Start, and so on (Devices › Key mapping).
- Machines pair once with a 6-digit code; every connection after that is TLS pinned to the paired machine.
  Connect on the local network, or through an SSH tunnel.

## Install (macOS)

```sh
brew install --cask operonlab/tap/flowseam
```

Open Flowseam once. macOS asks for **Accessibility** and **Input Monitoring** (System Settings › Privacy
& Security); grant both. Every release is signed with the same certificate, so updates keep the grants.

To start it at login, add Flowseam under System Settings › General › Login Items.

Update with `brew upgrade --cask flowseam`. Run the same version on every machine.

## Windows

There is no installer or window yet: `flowseam-win` is a console program, built from source
(`cargo build --release -p flowseam-win`) and usually started by a scheduled task.

```text
flowseam-win pair-serve --config C:\path\config.toml     # shows the code to type on the Mac
flowseam-win run --config C:\path\config.toml --listen   # wait for the Mac
```

## Pairing

On the controlling Mac, open Flowseam › Pairing, pick the device and type the code the other machine
shows. Showing a code on a Mac is not in the app yet.

## Build from source

Rust stable. `cargo test --workspace` runs everywhere; `scripts/make-app.sh <version>` builds and signs the
macOS app (both Apple silicon and Intel) and needs a signing identity.

## License

MIT
