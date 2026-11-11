<!-- doctype: narrative -->
# Changelog

All notable changes to Flowseam are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.4] - 2026-11-11

### Upgrading

- 0.1.4 still connects to 0.1.3, but switching monitors, trackpad gestures, and a switch that puts the cursor in
  the middle of the other screen need 0.1.4 on both computers. Update every machine together.
- On a Mac, `brew upgrade --cask flowseam`. On Windows, quit Flowseam from the tray and replace `flowseam.exe`
  with `Flowseam-0.1.4-windows-x64.exe` from the Releases page.

### Added

- On a Mac with Apple silicon, Flowseam can switch monitors along with the keyboard and mouse, over DDC/CI. In a
  computer's settings, add the monitors that are also cabled to it and pick the input that shows it. A test switches
  a monitor over for a few seconds and then back on its own, and asks whether it showed the other computer; Test
  All does this for every unconfirmed monitor at once and asks about them on one sheet. The answers are saved
  right away, without the other unsaved changes. A monitor nobody tested switches too, marked unconfirmed in the
  menu. The switch button in the menu
  bar and on the Overview page then switches the ticked monitors, one or several, and after them the keyboard and
  mouse. If a monitor fails, every monitor goes
  back. Another button brings the monitors home without moving the keyboard and mouse. An older version that
  rewrites `config.toml` drops the monitors from it; the next start of this one puts them back from the copy
  it keeps in `peer-extras.toml`, along with the gesture mappings.
- On Windows, the button that gives control back to the Mac also asks that Mac to switch its monitors home, even
  when the Mac is not controlling the PC at the time. Both computers need this version; an older Mac is not asked.
- Each release lists the SHA-256 of its files in `SHA256SUMS`, so a downloaded zip or exe can be checked with
  `shasum -a 256 -c SHA256SUMS --ignore-missing` (on Windows, `certutil -hashfile <file> SHA256`).
- `brew uninstall --zap --cask flowseam` also removes Flowseam's settings, logs and caches, and this machine's
  identity and pairings: after that, every computer pairs with it again.
- On a Mac, Advanced can turn off the Dock icon and the menu bar icon. With both off, opening Flowseam again from
  Finder or Spotlight brings the settings window back, and ⌘Q or the Quit button there ends Flowseam.
- An optional update check, off by default: once a day (hourly while checks fail) Flowseam asks GitHub for the
  latest release and only says when a newer one exists, in the menu and in Advanced. A version can be skipped;
  installing stays as before.
- Trackpad gestures from a Mac work on the computer it controls, and no longer also act on the Mac itself.
  Switching spaces, Mission Control, App Exposé, Show Desktop and the app launcher become that computer's own
  shortcut (on a PC: Ctrl+Win+← →, Win+Tab, Win+D, Start). The opposite gesture closes what one opened, as on
  the trackpad. On a PC, swiping down opens Task View, as App Exposé shows the windows, or shows the desktop, as a
  Windows touchpad does, with a swipe up bringing the windows back. Gesture Mapping in a PC's settings picks
  one; with its lock closed it follows Key Mapping, and keys by position take the Windows touchpad's. A PC needs no update for this, and a Mac in between passes gestures on to the PC behind it. Every
  Mac on the way needs this version: an older one gets no gestures at all, so none reach a PC behind it as the
  wrong keys. A gesture made while a modifier key is held does nothing there. Rotating is not
  carried, and a two-finger swipe between pages still arrives as sideways scrolling rather than back or forward.

### Changed

- On a Mac, the background process uses far less CPU while nothing happens: measured on a Mac mini sharing with
  two computers, from about 6.6% to under 1%. It no longer re-reads every network interface twice a second, and
  it runs on a short timer only while the pointer is at an edge or another computer has it.
- While nothing happens, the background process no longer wakes every 50–100 ms just to check whether to stop:
  connections, signal handling and the input tap on a Mac wait until there is something to do.
- With its window closed, the settings process on a Mac no longer copies the system fonts into memory (it read
  PingFang, 78 MB, twice) and draws once a second instead of five times.

### Fixed

- On a Mac controlling another computer, pressing Caps Lock (for example to switch the input source) could
  crash Flowseam's background process, which restarted a second later. Two computers linked to one Mac could also
  crash it now and then, when both read the clipboard at the same moment.
- After a switch from the menu that moved every monitor of the Mac to the other computer, the edge that leads
  back to the Mac no longer ends the switch: it led to a screen nobody could see, and a cursor resting there sent
  the monitors home in the middle of work. Come back with the switch chord on the Mac's keyboard, or with the
  button on the other computer that gives control back. A switch that moved only some monitors keeps the edge.
  This needs this version on both computers. While the monitors are on their way home, the edges and the chord
  wait too: a cursor that went straight back to the other computer used to land there as its monitors left. On
  Windows, the log now records the display layout when a session starts and whenever it changes.
- A switch from the menu bar no longer bounces back after a moment. The other computer's cursor appeared a few
  pixels from the edge that leads back, so the hand's first move, back toward the menu it had just used, ended
  the switch, and the monitors followed. A switch from the menu or the switch chord now puts the cursor in the
  middle of that computer's screen; it needs this version on both computers, and an older one still gets it at
  the edge. While the monitors are switching, the edges and the chord wait: a cursor that wandered to another
  computer meanwhile used to cancel the switch. The log now says why each session ended.
- On a Mac controlled from another computer, shortcuts made of arrows or function keys work: ⌃↑ opens Mission
  Control, ⌃← ⌃→ switch spaces, F11 shows the desktop. These keys arrived without the flags a Mac keyboard sends
  with them, so macOS did not take them as shortcuts, though typing and moving the cursor with them worked.
- On Windows, the settings window shows the Mac it is connected to as connected. It used to show every Mac as not
  connected, even while that Mac was controlling the PC.
- On a Mac, started hidden (`flowseam --hidden`), Flowseam no longer shows its settings window or takes the focus
  from the app you are using. The window used to appear anyway once it had drawn its first frame.
- On a Mac, Open at login starts Flowseam hidden; it used to open the settings window at every login. An earlier
  login item is replaced the next time Flowseam starts, and turning the option off no longer affects a running
  Flowseam.
- Opening Flowseam while it already runs brings up its window instead of starting a second copy with a second
  menu bar icon.

## [0.1.3] - 2026-11-08

### Upgrading

- The relay below runs on the Mac in the middle, so that Mac needs 0.1.3; the computers on either side connect to
  it as before. Keeping every machine on the same version is still the safe choice.
- Windows now has a ready-built `Flowseam-0.1.3-windows-x64.exe` on the Releases page. To update, quit
  Flowseam from the tray and replace the old `flowseam.exe` with it.

### Added

- Through one Mac to the next: while one Mac drives another, pushing the pointer to the edge that leads from that
  Mac to one of its own computers carries on there, for example from a MacBook through a Mac mini to a Windows PC.
  The far computer hands the pointer back to the Mac in the middle as usual. File drags do not take this route yet.
- The computer you touch takes over: while another computer drives a Mac, moving that Mac's own mouse, clicking,
  scrolling or pressing a key on it (modifier keys alone do not count) gives control back to it. If that Mac was
  passing the pointer on to a third computer, it goes on driving that computer itself.
- A ready-built Windows exe on the Releases page. It is not signed, so Windows SmartScreen warns on first launch.

### Fixed

- When another computer drives a Mac with more than one screen, the pointer reaches every screen. It used to stop
  at the edge of the screen it entered.

## [0.1.2] - 2026-11-08

### Upgrading

- Update the Mac and the Windows PC together. The new file drags were tested with 0.1.2 on both.
- On Windows, build the new version from source and replace the old `flowseam.exe`.

### Added

- A file dragged from a Mac onto a Windows PC stays a drag there: release it over a folder, the desktop or an app
  that takes files. It no longer always lands in Downloads; released over nothing that takes files, it still does.
- Files can be dragged out of Windows: while a Mac drives the PC, drag a file in Explorer to the edge that leads
  back to the Mac and release it where you want it there. The file stays where it was on Windows.

### Fixed

- On a Mac with more than one screen, a file dragged back from the computer it was driving lands where it is
  released. When the cursor came home on a screen other than the main one, it fell back to Downloads.

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
