<!-- doctype: narrative -->
# Changelog

All notable changes to Flowseam are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-11-06

### Upgrading

- **Upgrade every machine together.** 0.1.1 does not connect to 0.1.0. Pairings carry over, so nothing needs to
  be paired again.
- On Windows, build the new version from source and replace the old `flowseam.exe`.

### Added

- A file dragged onto a Mac stays a drag there: release it over a folder, the desktop or an app, instead of it
  always landing in Downloads. A file dragged onto a Windows PC still goes to Downloads.
- Volume and media keys reach the computer you are controlling.
- A menu bar icon on the Mac and a tray icon on Windows show whether sharing is on and which computers are
  connected. Closing the window keeps Flowseam running; Quit is in the menu.
- A settings window on Windows, with a button that shows the pairing code.
- A Mac can show a pairing code too, so two Macs can be paired from the app.
- The app speaks English, 繁體中文, 日本語 and 한국어. It follows the system language; Settings › Advanced ›
  Language overrides it.
- Open at Login, in Settings › Advanced. It is off until you turn it on.
- The version is shown in the window, What's New lists the changes after an update, and Check for Updates
  opens the Releases page (Flowseam itself does not go online), or updates with Homebrew when Homebrew
  installed the copy you are running.
- Settings shows whether each paired computer is connected.
- A new app icon.

### Changed

- Between two Macs, Flowseam uses the fastest link they share, with Thunderbolt and Ethernet ahead of Wi‑Fi.
- Saving settings applies them while sharing, without stopping it.
- After a restart or an update, sharing comes back the way it was left.
- If Flowseam's background process exits or stops responding, it is restarted, and the window says so.
- Only one copy of Flowseam shares input per user account.

### Fixed

- The pointer on the controlled computer no longer loses steps when the mouse moves fast, on a Mac or a
  Windows PC, so it keeps the speed you move at.
- A file dragged onto a Mac no longer restarts Flowseam when the drop takes longer than 3 seconds.
- A file dragged out of a background Finder window no longer stays stuck mid-drag on that Mac.
- Releasing the mouse right after a file drag crosses the edge still drops the file.
- A drag that crosses to another computer and comes back no longer leaves the mouse button held.
- If the background process hangs, or macOS takes a permission away while sharing, the keyboard and mouse are
  released instead of staying locked.
- The controlling Mac no longer stays locked when the other computer never takes control.
- A slow disk no longer freezes the mouse during a file drag.
- A computer showing a pairing code keeps waiting when an already paired computer that is sharing connects
  to it. In 0.1.0 the Windows pairing code stopped with an error as soon as that happened.
- In Overview, a computer placed above or below this one stays inside the picture instead of covering the card
  above it.
- Quitting Flowseam from a script or a tool that quits apps now quits it on a Mac. The request could reach
  Flowseam's background process instead, which the window then restarted.
- Homebrew no longer prints deprecation warnings about the Flowseam cask.

## [0.1.0] - 2026-11-05

### Added

- First release: one keyboard and mouse across Macs and a Windows PC, with a shared text clipboard, files
  dragged across the edge into Downloads, and pairing with a 6-digit code over pinned TLS.
