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
| `src/ops.rs` | Long-running operations: install/uninstall RED4ext & NightCoop, rollback, mods off/on (parking), save backup/restore/mirror, launch options, bug report. Has the integration test. |
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
- **Mods off** moves `bin/x64/winmm.dll` and the whole `red4ext/` folder into `<game>/_NightCoopHelper_mods_off/`
  (same drive, so it's a rename). `Status.mods_off` is "that folder exists"; every install/uninstall calls
  `ensure_mods_on` first, and "Install everything" turns mods back on. Legacy `red4ext/NightCoop.disabled/` (v0.1.0) is still understood.
- **Game updates** are detected by comparing the Steam `buildid` saved at install (`InstallState.game_build_id`) with the current one.
- **Steam Cloud** status comes from `<steam>/userdata/<id>/1091500/remotecache.vdf` (read-only). There is no supported way to
  force a sync; don't try to edit Steam's cloud files.
- **Launch options** live in `<steam>/userdata/<id>/config/localconfig.vdf` →
  `UserLocalConfigStore/Software/Valve/Steam/apps/1091500/LaunchOptions` (key case varies: `apps`/`Apps`).

## Build, test, run

```powershell
cargo test                      # unit + fake-game install cycle (no network)
cargo test -- --include-ignored # also installs real RED4ext and the live NightCoop build into temp folders
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

## What the real NightCoop builds look like (verified on 0.1.0-beta.1)

- Feed entry: `version: "0.1.0-beta.1"`, `channel: "beta"`, `game: ["2.5", "2.31"]`, `protocol: 128`, `build: "a7c537d"`,
  one `files` entry `{kind: "mod", name, url: "dl/<ver>/NightCoop-<ver>.zip" (relative), size, sha256}`, `yanked: null`.
  While only betas exist, the **stable** channel falls back to the site's `latest` (see `NcManifest::latest_for`).
- Zip layout: `bin/x64/winmm.dll`, `red4ext/RED4ext.dll` + licenses, `red4ext/plugins/NightCoop/NightCoop.dll`.
  Nothing outside `bin/` and `red4ext/`, so Mods off covers everything.
- **RED4ext is bundled** (1.30.0, byte-identical to GitHub's). The site says: "RED4ext 1.30 is already in the archive —
  nothing to install on its own." Policy (`InstallState.red4ext_custom`):
  - default **Bundled**: every NightCoop install makes RED4ext match the build's copy (`ops::install_bundled_red4ext`);
    "Install everything" also reinstalls the build (from the checksum-matched cache) if RED4ext drifted.
  - **My own version**: set when the player installs a RED4ext release/file from Mods & Versions; NightCoop installs then
    never touch RED4ext. Switching back to Bundled reinstalls the current build to restore its RED4ext.
  - GitHub RED4ext is only a fallback (no build listed / feed down) and an informational "newer on GitHub" note.
- `NightCoop.dll` has **no version resource**, so the installed version is known only from `InstallState` (trusted while the
  DLL's SHA-256 matches). A build installed outside the helper shows "unknown version" and is offered as an update.
- Versions use semver pre-releases: `0.1.0` > `0.1.0-beta.2` > `0.1.0-beta.1` (`util::cmp_versions`).
- `game` entries are matched against the exe's ProductVersion (`2.5` here) with `util::version_covers`.

`cargo test -- --include-ignored` runs `real_nightcoop_install`, which installs the live latest build into a temp folder.

## Open items

- [ ] On the Deck: full "Install everything" from Desktop Mode, Steam restart, launch option written, game shows the Co-op menu.
- [ ] "Add to Steam" shortcut appears in Gaming Mode and launches.
- [ ] File picker (xdg-desktop-portal) works in Desktop Mode.
- [ ] NightCoop officially supports Windows 10/11 only; collect Deck/Proton reports.
- Ideas: Windows exe/window icon, Proton version recommendation, detect Heroic/Lutris installs on Linux,
  controller-friendly focus navigation for Gaming Mode, identify manually installed builds by hashing the DLL inside cached zips.

## Changelog

- **0.3.1** — "Remove everything" clears the Proton launch option when *any* Steam account on the device has it
  (it used to require every account, so a Deck with two accounts could keep it).
- **0.3.0** — Tuned to the real NightCoop 0.1.0-beta.1: RED4ext bundled with NightCoop is the default (with a
  "My own version" choice that updates never override), one-click setup installs the build directly (GitHub RED4ext only as
  fallback), semver pre-release ordering, protocol number in "Copy my versions"/partner compare, game-version support
  check on Home, Remove everything button, build id shown, "vX available" on Home.
- **0.2.0** — Mods off/on switch (survives restarts), game-update alert, partner version compare, Steam Cloud status,
  extra backup folder + export, safer restore (folder swap; can't prune the backup being restored), download cache pruning,
  no more `panic = "abort"` (a failing job shows an error instead of closing the app), fewer per-frame disk/process scans.
- **0.1.0** — First release.
