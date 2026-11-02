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
| `src/main.rs` | Window setup (maximized on Deck), module list. `--start-hub` starts the player's hub and exits (autostart). |
| `src/app.rs` | All GUI pages, the job runner (one background thread at a time), confirm modals (`run_confirm`), styling, and the AI agent glue: `ai_tool` maps each tool to the same code the buttons use, `ai_drive` runs approved calls one at a time and returns results. |
| `src/ai.rs` | Optional AI helper over the OpenAI-compatible API (Ollama, LM Studio, llama.cpp, Jan): model list, local server discovery, streaming chat with **tool calls** (`TOOLS`: name, description, params, `Risk::Look/Change/Always`), the knowledge prompt and agent rules. Falls back to plain chat for models without tool support. |
| `src/ops.rs` | Long-running operations: install/uninstall RED4ext & NightCoop, rollback, mods off/on (parking), save backup/restore/mirror, launch options, bug report. Has the integration test. |
| `src/game.rs` | Finding the game (Steam / GOG / Epic / manual) and `Status::scan` (what's installed, versions, saves, Proton, logs). |
| `src/steam.rs` | Steam roots & libraries, `appmanifest`, launch-option merge/strip, process detection, Steam shutdown/start, Deck detection. |
| `src/remote.rs` | Release feeds: GitHub releases API (RED4ext, helper) and `https://nightcoop.net/releases.json`. |
| `src/invite.rs` | Finds NightCoop invites (`CODE@host[:port]#FINGERPRINT` or a bare code) in pasted text; builds the "message for my partner". |
| `src/net.rs` | Connection check: resolves `hub.nightcoop.net`, STUN binding requests to the public servers the mod uses → UDP works? strict NAT? `public_ip()`. |
| `src/upnp.rs` | UPnP IGD client (SSDP + SOAP, no deps): router discovery, external address (CGNAT check), add/remove UDP port mappings. |
| `src/hub.rs` | Self-hosted hub: address parsing (`host#FINGERPRINT`), `hub.txt` "use in my game", SSH launcher for a VPS, and the **native hub**: downloads zig (PyPI wheel, pinned SHA-256) + the official source + ENet, compiles it on the player's PC (Windows or Linux), smoke-tests it, start/stop (detached, log in `logs/hub.log`), fingerprint, key backup, autostart (HKCU Run / XDG autostart), update check against the source's latest commit, Windows Firewall rule (one UAC prompt). |
| `scripts/nightcoop-hub-setup.sh` | One-command hub setup for Ubuntu 24.04+/Debian 13+ (VPS or Linux PC as a systemd service; `--no-service` for a manual run). Builds the official hub source (ENet commit read from the official guide), systemd service, ufw, sysctl, prints `NIGHTCOOP_HUB_ADDRESS=ip#fp`. Served from this repo's `main` branch — keep the path stable. |
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
  title, notes, game:[...], protocol, files:[{kind:"mod"|"hub-src", url, size, sha256, mirrors:[], game?:[...], red4ext?}]}]}`.
  **Since 0.1.2-beta a release may ship one `mod` file per game version** (`files[].game`, plus the bundled `red4ext` version):
  `NcRelease::mod_file_for(exe version)` picks the matching one and returns `None` rather than a wrong file; "Install
  everything" reinstalls when the installed zip's SHA-256 isn't the one for the current game version (the game was updated).
  Text fields may be a string or `{"en","ru"}`. URLs may be relative to the manifest. Parsing is lenient on purpose.
- **RED4ext** zip layout: `bin/x64/winmm.dll` + `red4ext/RED4ext.dll` (+ licenses). GitHub assets carry a
  `digest: "sha256:…"` which we verify. Symbols zip is skipped.
- **Proton** needs `WINEDLLOVERRIDES="winmm,version=n,b" %command%` (`version` is harmless; it helps CET users).
- **Proton also needs Microsoft's VC++ 2015-2022 runtime ≥ 14.40 in the prefix** — `RED4ext.dll` and its `winmm.dll` import
  `msvcp140`/`vcruntime140`/`vcruntime140_1`. With only Wine's stand-ins the loader shows **"RED4ext (error 998) … RED4ext could
  not be loaded"** (ERROR_NOACCESS; first seen on the user's Deck, matches RED4ext issues #76/#115). `ops::install_vc_runtime`
  downloads `https://aka.ms/vs/17/release/vc_redist.x64.exe` (a 32-bit PE), runs it with `<proton>/proton run … /install /quiet
  /norestart` (env `STEAM_COMPAT_DATA_PATH`, `STEAM_COMPAT_CLIENT_INSTALL_PATH`), then sets the winetricks `vcrun2022` DLL
  overrides (native,builtin) via `regedit /S`. `steam::vc_runtime_ok` checks system32 DLLs (no "Wine builtin" marker, version)
  plus the overrides in `pfx/user.reg`. The prefix only exists after the game's first start. The Proton build is read from
  `compatdata/1091500/config_info`.
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
$env:NCH_PAGE="versions"; cargo run   # open a page directly: home, play (together), versions, saves, host (server), troubleshoot, ai, guide, settings
$env:NCH_AI_ASK="question"            # with NCH_PAGE="ai": sends the question once the AI is connected (testing)
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
- `NightCoop.dll` embeds its **build id** as a NUL-terminated string followed by the build date
  (`\0a7c537d\0Oct 23 2026 17:26:20`). `ops::identify_nightcoop` matches it against the feed's `build` field, so a build
  unpacked by hand gets its version without a reinstall (run during update checks and setup when the version is unknown).
- The DLL also names `hub.nightcoop.net` (official hub, UDP 47800) and public STUN servers (Google, Cloudflare, Twilio, Nextcloud).
  The hub speaks encrypted ENet — don't try to ping it; the connection check uses STUN only.
- Self-hosted hubs: https://github.com/Twenfys/nightcoop-hub (UDP 47800 + 47810–47873, address `host#FINGERPRINT`).
- The **clipboard needs eframe's `clipboard` feature** (it is not on by default with `default-features = false`). Without it every
  Copy button and Ctrl+V silently does nothing — this shipped broken in 0.1.0–0.3.1.

`cargo test -- --include-ignored` runs `real_nightcoop_install`, which installs the live latest build into a temp folder.
Live tests that need more: `real_native_hub` (downloads zig ~100 MB, builds and runs the hub, ~80 s), and with a model
running (`NCH_AI_URL=http://localhost:11434/v1`) `live_chat`, `live_tools` and `live_agent_in_app` (the whole agent loop in
the real `App`, headless; changes are declined).

### 0.1.2-beta (verified 2026-11-02)

- `protocol: 143` (0.1.0/0.1.1 were 128 — they can't play together), `build: "bacb705"`, notes are a list per language.
- Two `mod` files: `NightCoop-0.1.2-beta-2.5.zip` (`game: ["2.5"]`, `red4ext: "1.31.0"`) and `…-2.31.zip` (`game: ["2.31"]`,
  `red4ext: "1.30.0"`). Same zip layout as before.

## Self-hosted hub facts

- Hub CLI (official): `nightcoop_hub <port> --shards N --max-clients 512 --sign-key-auto <key> --hub-name X --region Y`;
  thread ports are `port+10 … port+10+N-1`; `--hub-public <key>` prints `# fingerprint XXXX-XXXX-XXXX-XXXX`;
  `--gen-sign-key <file>` makes a key. The key is the server's identity — the helper keeps it in `data/hub/` and a copy in
  `data/backups/hub-sign.key`, restored on rebuild.
- **The source builds natively on Windows** (it has `_WIN32` paths: `windows.h`, `bcrypt`, `io.h`). Recipe = the official
  Dockerfile's: ENet (commit from `ARG ENET_COMMIT=` in `tools/hub/docker/Dockerfile`, fallback `5a9c537…`) as C files
  (`win32.c` on Windows, `unix.c` + the `HAS_*` defines on Linux), `monocypher.c`, then `src/Log.cpp src/NetCrypto.cpp
  src/HubServer.cpp tools/hub/main.cpp` with `-std=c++20 -O2`; link `-lws2_32 -lwinmm -lbcrypt` (Windows) or `-pthread`.
  zig 0.16.0 (`zig cc` / `zig c++`, target `x86_64-windows-gnu`) — verified 2026-11-02: builds, `--gen-sign-key`/`--hub-public`
  work, listens on the entry and thread ports. Caches go to `data/tools/zig-cache` (portable). The hub repo has no licence
  file, so the helper builds on the player's machine instead of redistributing binaries.
- GitHub source zips carry the commit SHA as the zip comment; `hub/commit.txt` stores it, and `hub::latest_commit()` (API,
  `Accept: application/vnd.github.sha`) drives the update check (`Settings.hub_auto_update`: rebuild + restart automatically).
- Home hubs use `--shards 2` → UDP 47800, 47810, 47811. A VPS uses `auto` (one thread per core).
- The game is pointed at a server by writing `address#fingerprint` to `hub.txt` next to `NightCoop.dll` (the mod moves it into
  its own servers on the next start — documented in the official hub guide). While mods are off, that's the parked folder.
- Not run end-to-end yet: the VPS script on a fresh VPS, UPnP add/remove on a real router, the Linux native build (same recipe
  as the verified Linux cross-compile), a player connecting to a home hub from outside.

## Open items

- [x] On the Deck: "Install everything" from Desktop Mode wrote the launch option; the loader ran (winmm.dll loaded under Proton).
- [ ] On the Deck: the VC++ runtime install via `proton run` works outside Steam's container, and the Co-op menu appears.
- [ ] "Add to Steam" shortcut appears in Gaming Mode and launches.
- [ ] File picker (xdg-desktop-portal) works in Desktop Mode.
- [ ] NightCoop officially supports Windows 10/11 only; collect Deck/Proton reports.
- [ ] Host a server: a partner joins a home hub from outside (firewall + router), the native build on Linux, the setup script on
      a fresh Ubuntu 24.04 VPS, and UPnP add/remove on a real router.
- [ ] AI helper: try more models (qwen2.5:7b, llama3.1:8b) for tool use; gemma4:12b was verified on CPU.
- Ideas: Windows exe/window icon, Proton version recommendation, detect Heroic/Lutris installs on Linux,
  controller-friendly focus navigation for Gaming Mode, identify manually installed builds by hashing the DLL inside cached zips.

## Changelog

- **0.6.0** — **AI helper** page: connect a local AI (Ollama, LM Studio, llama.cpp, Jan; "Find my AI"; Deck → PC on the LAN).
  It gets a rundown of NightCoop and the app, the player's diagnostics, and 32 tools covering every page (status, logs,
  updates, install/switch builds, mods off/on, saves, VC++ runtime, launch option, connection/router checks, the whole server
  setup); changes need an Allow click unless "act without asking" is on, destructive ones always ask. **Native hub** on
  Windows and Linux replaces WSL: one-button build from the official source with a downloaded compiler, firewall, start/stop,
  start with the computer, automatic updates when the source changes. **NightCoop 0.1.2-beta support**: per-game-version
  downloads, reinstall when the game version changes. Fixes: servers.txt, "Use this server" and bug-report logs while mods
  are off; downloads cut off mid-way without a checksum were accepted; "2.3" counted as covering game "2.31".

- **0.5.0** — **Host a server** page: WSL2 automation on Windows (install, mirrored networking, firewall, build, start/stop,
  fingerprint, key backup), VPS guide + one-command setup script (`scripts/nightcoop-hub-setup.sh`) with an SSH launcher,
  Linux PC path, router check via UPnP (CGNAT detection, automatic port forwarding), "Use this server in my game" (`hub.txt`).
  Future-proofing: installs mirror "unpack the whole archive" — every file a build ships is installed and tracked (other
  plugins, archive/r6 mods, loader files), stale ones removed on update, all covered by uninstall and Mods off; RED4ext files
  are installed as-is when a build loads RED4ext some new way; a newer release-list format is flagged; the helper's own update
  check falls back to github.com when the API is rate-limited (still SHA-256 verified via SHA256SUMS.txt). Fixes: the VC++
  runtime step runs before the Steam step (a Steam failure no longer skips it); never offers a Steam restart in Gaming Mode.
  Verified against NightCoop 0.1.1-beta.2 (same layout, build id `8b2fe3e`).

- **0.4.1** — Fix for "RED4ext (error 998) / RED4ext could not be loaded" on the Steam Deck: the helper detects and installs
  Microsoft's Visual C++ runtime into the game's Proton prefix (plus winetricks-style DLL overrides) during "Install everything",
  with a Home status row, a Troubleshoot button and a help entry. `util::pe_version` now reads 32-bit PE files too.

- **0.4.0** — New **Play together** page: paste a partner's versions/invite/message → match verdict, one-click switch to their
  build, decoded invite with Copy; "message for my partner"; STUN connection check; Deck paste (Ctrl+V back-button) tip;
  servers.txt editor moved here with the self-host link. Fixes: **copy/paste never worked** (missing eframe clipboard feature);
  repairs no longer push real builds out of the rollback list; diagnostics/bug reports hide account names and the home path.
  Builds unpacked by hand are recognised by their embedded build id. Automatic backups are skipped when saves didn't change;
  the 15 MB DLL isn't re-hashed on every refresh; the three update lookups run in parallel; Settings shows data usage with a
  "Clear download cache" button.

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
