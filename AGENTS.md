# AGENTS.md — guide for AI coding agents (and humans)

This repo is **NightCoop Helper**: a single-binary Rust GUI (egui/eframe) that installs, updates
and troubleshoots the [NightCoop](https://nightcoop.net/) co-op mod for Cyberpunk 2077.
**Primary target: Steam Deck (SteamOS, game under Proton). Secondary: Windows.**
The end user is non-technical and plays on a Steam Deck; every change should keep the
one-button flow working without a terminal.

## Ground rules

- **Never break the one-click path**: Home → "Install everything" must keep working on a fresh Deck.
- **Portable**: no installers, no runtime dependencies. Data lives in `NightCoopHelper-data/` next to the exe
  (`config::data_dir()`), falling back to the user config dir only if that folder isn't writable.
- **Don't touch the user's saves except through backup/restore.** Back up before anything risky.
- **Steam config edits require Steam to be closed** (Steam rewrites `localconfig.vdf` / `shortcuts.vdf` on exit).
  Always back up the file first (see `steam::set_launch_options`). Never restart Steam from Gaming Mode
  (`steam::in_gaming_mode()`): that kills the Deck's session.
- Keep the Linux binary free of new link-time system libraries (check with the ELF snippet below).
  Prefer pure-Rust crates; anything needing GTK/OpenSSL at link time is out.
- Match the existing style: small modules, `anyhow` errors with user-readable context, comments only where the *why* isn't obvious.
- The UI copy is plain English for a gamer, not a developer. Say what to do, not what failed internally.

## Layout

| File | Responsibility |
|---|---|
| `src/main.rs` | Window setup (maximized on Deck), module list. |
| `src/app.rs` | All GUI pages, the job runner (one background thread at a time), confirm modals, styling. |
| `src/ops.rs` | Long-running operations: install/uninstall RED4ext & NightCoop, rollback, enable/disable, save backup/restore, launch options, bug report. Has the integration test. |
| `src/game.rs` | Finding the game (Steam / GOG / Epic / manual) and `Status::scan` (what's installed, versions, saves, Proton, logs). |
| `src/steam.rs` | Steam roots & libraries, `appmanifest`, launch-option merge/strip, process detection, Steam shutdown/start, Deck detection. |
| `src/remote.rs` | Release feeds: GitHub releases API (RED4ext, helper) and `https://nightcoop.net/releases.json`. |
| `src/vdf.rs` | Text KeyValues parser/writer that preserves raw escaping (round-trips Steam files). |
| `src/shortcuts.rs` | Binary `shortcuts.vdf` reader/writer for "Add to Steam". |
| `src/selfupdate.rs` | Helper self-update from this repo's GitHub releases. `HOME_REPO` lives here. |
| `src/config.rs` | `Settings` (user prefs) and `InstallState` (what the helper installed), both JSON in the data dir. |
| `src/util.rs` | HTTP (ureq + rustls), download with progress + SHA-256, zip/7z extraction, copy-merge, PE version reading, version compare. |
| `scripts/package.py` | Copies builds into `dist/` with stable names, makes the Deck tarball (keeps +x), writes `SHA256SUMS.txt`. |
| `build.ps1` | Tests → Windows release build → Linux cross-build (`cargo zigbuild`, glibc 2.28) → package. |

## Facts the code relies on

- **NightCoop** (from nightcoop.net docs): native RED4ext plugin, no other dependencies (no CET).
  Install = copy the build's `red4ext` folder into the game folder. Check file:
  `red4ext/plugins/NightCoop/NightCoop.dll`. Update = replace the `NightCoop` folder as a whole, but keep
  `servers.txt` (and legacy `hub.txt`). Logs: `red4ext/logs/` and `red4ext/plugins/NightCoop/logs/`.
  Players join with an invite code (`CODE@host#fingerprint`), relay hub on UDP 47800.
- **Release feed** `https://nightcoop.net/releases.json` (written by the site's `server/releases.py`):
  `{schema, updated, latest, releases:[{version, channel:"stable"|"beta", yanked:false|{reason}, date,
  title, notes, game:[...], protocol, files:[{kind:"mod"|"hub-src", url, size, sha256, mirrors:[]}]}]}`.
  Text fields may be a string or `{"en","ru"}`. URLs may be relative to the manifest. Parsing is lenient on purpose.
- **RED4ext** zip layout: `bin/x64/winmm.dll` + `red4ext/RED4ext.dll` (+ licenses). GitHub assets carry a
  `digest: "sha256:…"` which we verify. Symbols zip is skipped.
- **Proton** needs `WINEDLLOVERRIDES="winmm,version=n,b" %command%` (`version` is harmless; it helps CET users).
- **Deck save path**: `<library>/steamapps/compatdata/1091500/pfx/drive_c/users/steamuser/Saved Games/CD Projekt Red/Cyberpunk 2077`.
- **Launch options** live in `<steam>/userdata/<id>/config/localconfig.vdf` →
  `UserLocalConfigStore/Software/Valve/Steam/apps/1091500/LaunchOptions` (key case varies: `apps`/`Apps`).

## Build, test, run

```powershell
cargo test                      # unit + fake-game install cycle (no network)
cargo test -- --ignored         # also downloads real RED4ext into a temp folder
cargo run                       # debug GUI
$env:NCH_PAGE="versions"; cargo run   # open a page directly: home, versions, saves, servers, troubleshoot, guide, settings
.\build.ps1                     # full release build into dist\
```

Linux cross-compiling needs `rustup target add x86_64-unknown-linux-gnu`, `cargo install cargo-zigbuild`,
`pip install ziglang` (`build.ps1` finds zig via Python; or set `CARGO_ZIGBUILD_ZIG_PATH`).
There is no way to run the Linux GUI on the Windows dev box — **Deck behaviour must be verified by the user on hardware.**
Ask them for the diagnostics text (Troubleshoot → Copy diagnostics) when something fails.

Check the Linux binary's link deps after adding crates:

```python
from elftools.elf.elffile import ELFFile          # pip install pyelftools
f = ELFFile(open("target/x86_64-unknown-linux-gnu/release/nightcoop-helper", "rb"))
print([t.needed for s in f.iter_sections() if s.name == ".dynamic" for t in s.iter_tags() if t.entry.d_tag == "DT_NEEDED"])
# expected: libm.so.6, libc.so.6, libpthread.so.0, libdl.so.2 — nothing else
```

## Releasing

1. Bump `version` in `Cargo.toml` (self-update compares this against the release tag).
2. `.\build.ps1`
3. Commit, tag `vX.Y.Z`, push.
4. Create a GitHub release for the tag and upload **all files from `dist/`** with their exact names:
   `NightCoopHelper-windows-x64.exe`, `NightCoopHelper-linux-x86_64`, `NightCoopHelper-SteamDeck.tar.gz`, `SHA256SUMS.txt`.
   The self-updater picks the asset containing `windows` + `.exe`, or `linux` without an archive extension,
   and the README links use `/releases/latest/download/<name>` — **don't rename them**.

## Open items / next big update (after the NightCoop open beta goes live)

The helper was written on release day, before any NightCoop build existed (`releases.json` had `releases: []`).
When the first build is published, verify and adjust:

- [ ] Real `releases.json` entry parses (`remote::NcManifest`), `latest_for()` picks the right build on both channels.
- [ ] Real archive layout installs cleanly (`ops::install_nightcoop_archive` finds `red4ext/…/NightCoop.dll`;
      check whether the build ships extra root folders like `archive/` or `r6/`).
- [ ] Version shown for an installed build: we trust our record when the DLL hash matches, else PE version — confirm the DLL carries a sensible version resource.
- [ ] `game: [...]` format vs. our game-version display (`util::pe_version` on `Cyberpunk2077.exe` gives e.g. `2.5`) — consider warning on a mismatch.
- [ ] On the Deck: full "Install everything" from Desktop Mode, Steam restart, launch option written, game shows the Co-op menu.
- [ ] "Add to Steam" shortcut appears in Gaming Mode and launches.
- [ ] File picker (xdg-desktop-portal) works in Desktop Mode.
- [ ] Remove the "open beta 23 October" hint on Home once builds exist (it only shows while the list is empty).
- Ideas: compare versions with a partner via invite string, Windows exe icon, Proton version recommendation,
  detect Heroic/Lutris installs on Linux.
