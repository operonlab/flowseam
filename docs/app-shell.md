# App shell: languages, menu bar, versions, start at login, staying up

Status: implemented in 0.1.1. The patterns follow Hoard (`lab/hoard`): a source-keyed string
catalog, a "what's new" sheet with a seen-version key, and an update check that never goes online. Its
Swift code does not port; what carries over is these rules.

## Defaults chosen

- **Languages:** 繁體中文 (source), English, 日本語, 한국어.
  - The language follows the system, and Settings › 進階 can override it.
  - It switches at once: egui redraws every frame, so no relaunch is needed, unlike Hoard.
  - Missing text falls back to English, then to the Chinese key.
- **Menu bar (macOS) and tray (Windows):**
  - An icon whose menu shows the sharing state and has: sharing on/off, each peer's link status,
    Settings…, What's New, Check for Updates…, and Quit.
  - Closing the window keeps Flowseam running there, and sharing continues. On macOS the Dock icon is
    hidden while no window is open.
  - Quit is the only way out.
  - The macOS app menu gains About, Check for Updates… and What's New.
- **Version:** one number, the `flowseam` crate version. `make-app.sh` already refuses a mismatch. It is
  shown in the sidebar footer, in About, and in the update sheet.
- **What's new:** the notes are structured data per version, and their text goes through `tr`.
  - When it shows, as in Hoard:
    - after a fresh install: nothing;
    - to an existing user with no record yet: the current version only;
    - otherwise: every version newer than the last one seen.
  - The seen version is stored and never goes down.
  - `CHANGELOG.md` is the long form, linked from the sheet.
- **Check for updates:** no network, like Hoard.
  - It shows the current version and opens `github.com/operonlab/flowseam/releases`.
  - When Homebrew installed Flowseam, it offers `brew upgrade`, run in Terminal on macOS.
- **Start at login:** a toggle in Settings, **off by default**: login items are not changed without the
  owner's say.
  - macOS uses `SMAppService.mainApp`; Windows uses the HKCU `Run` key.
  - A launch at login starts with no window, in the menu bar or tray. Sharing resumes as it was left.
- **Staying up:**
  - **The worker is supervised by the window process.** A worker that exits is restarted with
    backoff: 1 s, 2 s, 5 s, 10 s, then every 30 s. Each restart is logged, and the window shows
    「已自動重新啟動」.
  - **A stalled worker is restarted.** A worker that is alive but has not written `status.toml` for
    10 s is killed and restarted, with the reason logged.
  - **The worker no longer depends on the window being responsive.** It writes its own timestamped
    log file and does not pipe stderr to the window. A window that hangs used to stall the worker on a
    full pipe; on the Windows test machine, on 2026‑11‑05, that left a dozen connections in CLOSE_WAIT.
  - **A window started while a worker runs attaches to it** (the per-user lock and `status.toml`) and
    does not start a second worker.
  - **On Windows the window draws on the low-power GPU.** It uses wgpu with DX12 and
    `PowerPreference::LowPower`, so it never opens a context on a discrete GPU. On the Windows test machine,
    `nvidia-smi` listed flowseam.exe as an RTX 3090 client. That driver logs nvlddmkm errors for days
    at a time, and a hung flowseam.exe there could not be terminated.

## Streams (separate worktrees, in parallel)

| Stream | New files | Touches in gui.rs / main.rs |
|---|---|---|
| i18n | `i18n.rs` catalog, `i18n/*.json`, check test | every existing string; language picker in 進階 |
| menu bar / tray | `menubar.rs` | start-up wiring, close-to-tray, menu events |
| version, what's new, update | `about.rs`, `CHANGELOG.md`, notes data | sidebar footer, sheet hooks |
| start at login | `autostart.rs` | one toggle in 進階, the `--hidden` launch flag |
| staying up | `worker.rs`, worker-side log | worker status line; Windows renderer options |

New text in any stream goes through `i18n::tr` / `trf` from the start, so the i18n stream only
converts what exists today.
