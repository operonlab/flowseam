<!-- doctype: contract -->
# Flowseam

**English** · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

One mouse and keyboard for your Macs and PCs. Push the pointer off the edge of one screen and it carries on to
the next computer; what you type follows it, and so do the clipboard and the files you drag.

https://github.com/user-attachments/assets/be5499eb-c39a-4722-b87a-0203b3a30c64

<sub>Narration and music in the demo video were generated with ElevenLabs (elevenlabs.io).</sub>

> **Status: early (0.1).** Used every day on two Macs and a Windows PC. Expect rough edges, and keep every
> machine on the same version.

## What it does

| | Mac ↔ Mac | Mac → Windows | Windows → Mac |
|---|---|---|---|
| Mouse and keyboard across a screen edge | yes | yes | no: a Windows PC is controlled, it does not control |
| Clipboard text | yes | yes | yes |
| Drag files across the edge | yes, dropped wherever you release them: a folder, the desktop or an app | yes, saved to Downloads | not yet |
| Volume and media keys | yes | yes | no |

- **Switching.** Rest the pointer at a shared edge for a moment (200 ms by default) or press a shortcut
  (default ⌃⌥⌘S). While a mouse button is held the pointer stays on its screen, so dragging a window or
  selecting text never slips onto the next computer; dragging files is the one exception.
- **Taking control back.** ⌃⌥⌘Esc on the Mac always returns the mouse and keyboard to it.
- **Mac shortcuts on Windows.** Driving a Windows PC from a Mac can translate shortcuts: ⌘C copies, ⌘Tab
  switches apps, ⌥ alone opens Start (Devices & Edges › Key Mapping).
- **The fastest link between Macs.** When two Macs are joined by more than one network, Flowseam uses the
  fastest: Thunderbolt or Ethernet ahead of Wi‑Fi.
- **Private by design.** Machines pair once with a 6‑digit code. Every connection after that is mutual TLS,
  pinned to the certificate of the machine you paired with. Nothing goes through a server: Flowseam talks
  over your local network, or over an SSH tunnel you set up. It has no telemetry, and Check for Updates only
  opens the Releases page.
- **Stays out of the way.** A menu bar icon (Mac) or tray icon (Windows) shows whether sharing is on and
  which machines are connected; closing the window keeps Flowseam running. If the background process dies
  or stalls, it is restarted. Open at Login is a setting, off by default.
- **Four languages.** English, 繁體中文, 日本語 and 한국어. The app follows the system language; Settings ›
  Advanced › Language overrides it.

## Screenshots

<p>
  <img src="docs/screenshots/en/mac-overview.png" width="49%" alt="Overview on a Mac: this Mac in the middle, a MacBook Pro on its left and a Windows PC on its right">
  <img src="docs/screenshots/en/win-overview.png" width="49%" alt="Overview on a Windows PC: a Mac mini on its left and a MacBook Pro above it">
</p>

## Install

### macOS 13 or later (Apple silicon and Intel)

```sh
brew install --cask operonlab/tap/flowseam
```

Or download `Flowseam-<version>-macos.zip` from [Releases](https://github.com/operonlab/flowseam/releases).

Open Flowseam once. macOS asks for **Accessibility** and **Input Monitoring** (System Settings › Privacy &
Security); allow both. Every release is signed with the same certificate, so updates keep the permissions.

Flowseam is signed with its own certificate, not notarized by Apple. The Homebrew cask clears the download
quarantine for you. If you install from the zip, macOS refuses the first launch: open System Settings ›
Privacy & Security and choose **Open Anyway**.

Update with `brew upgrade --cask flowseam`.

### Windows 10 or 11

There is no installer yet; build it with Rust (stable, MSVC toolchain):

```powershell
git clone https://github.com/operonlab/flowseam
cd flowseam
cargo build --release -p flowseam
.\target\release\flowseam.exe
```

Flowseam opens its settings window and puts an icon in the tray. Let it through Windows Firewall when asked:
the Mac connects to it on TCP port 24800.

## Set up

1. **Add the other computer** under **Devices & Edges** on each machine: a name, its address (host name or IP)
   and the screen edge it sits on.

   <img src="docs/screenshots/en/mac-edges.png" width="600" alt="Devices & Edges: the address, the screen edge and the key mapping of each computer">
2. **Pair.** One computer shows a code and the other types it in. On the one that shows it (the Windows PC, or either Mac when you pair two Macs), turn sharing off, since pairing uses the same port; open **Pairing**, pick the other computer and choose **Show Pairing Code**. On the other Mac open **Pairing**, pick the first computer, type the code, confirm, then choose **Save**. Pairing happens once; after that the machines trust each other automatically.

   <img src="docs/screenshots/en/mac-pairing.png" width="600" alt="Pairing: pick a computer, then type its code or show this computer's code">
3. **Turn on sharing** from the window or the menu bar icon, and push the pointer across the edge.

## Known limitations

- A Windows PC can be controlled but cannot control another computer, and files cannot be dragged out of
  Windows yet.
- Files dragged onto a Windows PC are saved to Downloads rather than where you release them.
- The clipboard carries text only: no images, rich text or files.
- No Linux support.
- Windows has no installer or signed build; build it from source.
- Machines are found by the address you enter; there is no automatic discovery on the local network yet.

## Troubleshooting

- **Logs.** macOS: `~/Library/Logs/Flowseam/worker.log`. Windows: `%LOCALAPPDATA%\Flowseam\worker.log`.
  The previous run is kept as `worker.log.1`.
- **Settings and pairings** live in `~/.config/flowseam` (on Windows, `%USERPROFILE%\.config\flowseam`).
- **The pointer will not cross.** Check that sharing is on, that both machines show each other as connected,
  and that the edge you push against is the one set under Devices & Edges.
- **"Missing permission" on a Mac.** Turn Flowseam on under System Settings › Privacy & Security ›
  Accessibility and Input Monitoring, then quit and reopen it.

## Build from source

Rust stable. `cargo test --workspace` runs on macOS and Windows. `scripts/make-app.sh <version>` builds and
signs the macOS app for both Apple silicon and Intel; it needs a code-signing identity and `cargo-about`
(`cargo install cargo-about --locked --features cli`).

Design notes are in [docs/](docs/). Changes are listed in [CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE). The licenses of the open-source libraries Flowseam is built from ship with the app, in
`Flowseam.app/Contents/Resources/THIRD-PARTY-NOTICES.html`.
