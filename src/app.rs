//! The GUI.

use crate::config::{self, Channel, InstallState, Settings};
use crate::game::{self, Status};
use crate::ops::{self, Event, Reporter};
use crate::remote::{self, GhRelease, NcManifest};
use crate::selfupdate::{self, HelperUpdate};
use crate::{hub, invite, net, shortcuts, steam, upnp, util};
use anyhow::{anyhow, Result};
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(252, 238, 10); // Cyberpunk yellow
const CYAN: Color32 = Color32::from_rgb(0, 240, 255);
const OK: Color32 = Color32::from_rgb(80, 220, 120);
const WARN: Color32 = Color32::from_rgb(255, 176, 60);
const BAD: Color32 = Color32::from_rgb(255, 90, 90);
const DIM: Color32 = Color32::from_gray(150);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Versions,
    Saves,
    Together,
    Server,
    Troubleshoot,
    Guide,
    Settings,
}

impl Page {
    const ALL: [Page; 8] = [Page::Home, Page::Together, Page::Versions, Page::Saves, Page::Server, Page::Troubleshoot, Page::Guide, Page::Settings];
    fn label(self) -> &'static str {
        match self {
            Page::Home => "🏠  Home",
            Page::Versions => "📦  Mods & Versions",
            Page::Saves => "💾  Saves",
            Page::Together => "👥  Play together",
            Page::Server => "🖥  Host a server",
            Page::Troubleshoot => "🔧  Troubleshoot",
            Page::Guide => "🎮  Deck Guide",
            Page::Settings => "⚙  Settings",
        }
    }
}

#[derive(Default)]
struct RemoteInfo {
    red4ext: Option<Result<Vec<GhRelease>, String>>,
    nightcoop: Option<Result<NcManifest, String>>,
    helper: Option<Result<Option<HelperUpdate>, String>>,
    checked_at: Option<String>,
}

enum Extra {
    None,
    Remote(RemoteInfo),
    OpenFolderOf(PathBuf),
    HelperUpdated,
    Connection(net::CheckResult),
    Router(RouterCheck),
    #[cfg_attr(not(windows), allow(dead_code))]
    Wsl(WslStatus),
}

struct JobDone {
    state: InstallState,
    result: Result<Extra, String>,
}

struct Job {
    title: String,
    rx: Receiver<Event>,
    handle: JoinHandle<JobDone>,
}

enum Confirm {
    Setup,
    LaunchOption { enable: bool },
    RestoreSaves(PathBuf),
    DeleteBackup(PathBuf),
    UninstallNightCoop,
    UninstallRed4ext,
    RemoveAll,
    AddToSteam,
    InstallNcVersion(String),
    InstallR4Version(usize),
    Rollback(PathBuf),
}

pub struct App {
    settings: Settings,
    state: InstallState,
    status: Status,
    remote: RemoteInfo,
    page: Page,
    job: Option<Job>,
    log: Vec<String>,
    progress: Option<(Option<f32>, String)>,
    banner: Option<(bool, String)>,
    confirm: Option<Confirm>,
    servers_text: Option<String>,
    servers_dirty: bool,
    path_input: String,
    r4_pick: usize,
    backups: Vec<(PathBuf, u64)>,
    rollbacks: Vec<PathBuf>,
    last_poll: Instant,
    helper_restart_ready: bool,
    partner_text: String,
    /// The invite the host pasted, to send along with their versions.
    my_invite: String,
    connection: Option<net::CheckResult>,
    server: ServerUi,
    /// Sizes of backups / rollbacks / download cache, computed when Settings is first shown.
    data_sizes: Option<(u64, u64, u64)>,
    /// Any Steam install found (cached; looking it up hits the disk).
    steam_found: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
        let state = InstallState::load();
        style(&cc.egui_ctx, settings.ui_scale);
        let status = Status::scan(&settings, &state);
        let mut app = App {
            path_input: settings.game_path.clone(),
            settings,
            state,
            status,
            remote: RemoteInfo::default(),
            page: Page::Home,
            job: None,
            log: Vec::new(),
            progress: None,
            banner: None,
            confirm: None,
            servers_text: None,
            servers_dirty: false,
            r4_pick: 0,
            backups: ops::list_backups(),
            rollbacks: ops::list_rollbacks(),
            last_poll: Instant::now(),
            helper_restart_ready: false,
            partner_text: String::new(),
            my_invite: String::new(),
            connection: None,
            server: ServerUi { vps_user: "root".into(), ..Default::default() },
            data_sizes: None,
            steam_found: !steam::steam_roots().is_empty(),
        };
        // NCH_PAGE=versions|saves|… opens a page directly (handy for screenshots and testing).
        if let Ok(p) = std::env::var("NCH_PAGE") {
            if let Some(page) = Page::ALL.into_iter().find(|pg| pg.label().to_ascii_lowercase().contains(&p.to_ascii_lowercase())) {
                app.page = page;
            }
        }
        if app.settings.check_updates_on_start {
            app.check_updates(&cc.egui_ctx);
        }
        app
    }

    // ---------- jobs ----------

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn spawn(
        &mut self,
        ctx: &egui::Context,
        title: &str,
        f: impl FnOnce(&Reporter, &mut InstallState, &Settings) -> Result<Extra> + Send + 'static,
    ) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let rep = Reporter { tx, ctx: ctx.clone() };
        let mut state = self.state.clone();
        let settings = self.settings.clone();
        self.log.push(format!("── {title} ──"));
        self.banner = None;
        self.progress = Some((None, title.to_string()));
        let handle = std::thread::spawn(move || {
            let result = f(&rep, &mut state, &settings).map_err(|e| format!("{e:#}"));
            rep.ctx.request_repaint();
            JobDone { state, result }
        });
        self.job = Some(Job { title: title.to_string(), rx, handle });
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        while let Ok(ev) = job.rx.try_recv() {
            match ev {
                Event::Log(l) => self.log.push(l),
                Event::Progress(f, l) => self.progress = Some((f, l)),
            }
        }
        if !job.handle.is_finished() {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let job = self.job.take().unwrap();
        while let Ok(ev) = job.rx.try_recv() {
            if let Event::Log(l) = ev {
                self.log.push(l);
            }
        }
        self.progress = None;
        match job.handle.join() {
            Ok(done) => {
                self.state = done.state;
                self.state.save();
                match done.result {
                    Ok(extra) => {
                        match extra {
                            Extra::Remote(info) => self.merge_remote(info),
                            Extra::OpenFolderOf(p) => {
                                if let Some(parent) = p.parent() {
                                    util::open_path(parent);
                                }
                            }
                            Extra::HelperUpdated => self.helper_restart_ready = true,
                            Extra::Connection(r) => self.connection = Some(r),
                            Extra::Router(r) => self.server.router = Some(r),
                            Extra::Wsl(w) => {
                                if let (Some(fp), Some(ip)) = (&w.fingerprint, &w.public_ip) {
                                    if hub::parse_address(&self.server.address).is_none() {
                                        self.server.address = format!("{ip}#{fp}");
                                    }
                                }
                                self.server.wsl = Some(w);
                            }
                            Extra::None => {}
                        }
                        if !["Checking for updates", "Checking the connection", "Checking your network", "Checking the server setup"].contains(&job.title.as_str()) {
                            self.banner = Some((true, format!("{} — done.", job.title)));
                        }
                    }
                    Err(e) => {
                        self.log.push(format!("ERROR: {e}"));
                        self.banner = Some((false, format!("{} failed: {e}", job.title)));
                    }
                }
            }
            Err(_) => self.banner = Some((false, format!("{} crashed unexpectedly.", job.title))),
        }
        self.rescan();
    }

    fn merge_remote(&mut self, info: RemoteInfo) {
        if info.red4ext.is_some() {
            self.remote.red4ext = info.red4ext;
            self.r4_pick = 0;
        }
        if info.nightcoop.is_some() {
            self.remote.nightcoop = info.nightcoop;
        }
        if info.helper.is_some() {
            self.remote.helper = info.helper;
        }
        if info.checked_at.is_some() {
            self.remote.checked_at = info.checked_at;
        }
    }

    fn rescan(&mut self) {
        self.status = Status::scan(&self.settings, &self.state);
        self.backups = ops::list_backups();
        self.rollbacks = ops::list_rollbacks();
        if !self.servers_dirty {
            self.servers_text = None;
        }
        self.data_sizes = None;
    }

    fn check_updates(&mut self, ctx: &egui::Context) {
        let unknown = self.unknown_build();
        self.spawn(ctx, "Checking for updates", move |rep, state, settings| {
            rep.progress(None, "Checking NightCoop, RED4ext and helper versions…");
            let info = fetch_remote(settings);
            if let (Some(game), Some(Ok(m))) = (&unknown, &info.nightcoop) {
                if let Some(v) = ops::identify_nightcoop(game, m, state) {
                    rep.log(format!("Recognised the installed NightCoop as {v}."));
                }
            }
            Ok(Extra::Remote(info))
        });
    }

    /// The game, when NightCoop is installed but the helper doesn't know which build it is.
    fn unknown_build(&self) -> Option<game::Game> {
        let unknown = self.status.nightcoop_installed && self.status.nightcoop_version.as_deref().map(|v| v.starts_with("unknown")).unwrap_or(true);
        unknown.then(|| self.status.game.clone()).flatten()
    }

    // ---------- derived info ----------

    fn r4_latest(&self) -> Option<&GhRelease> {
        match &self.remote.red4ext {
            Some(Ok(list)) => list.iter().find(|r| !r.prerelease && remote::red4ext_asset(r).is_some()),
            _ => None,
        }
    }

    fn nc_latest(&self) -> Option<&remote::NcRelease> {
        match &self.remote.nightcoop {
            Some(Ok(m)) => m.latest_for(self.settings.channel),
            _ => None,
        }
    }

    /// GitHub has a newer RED4ext than the installed one. Informational only: NightCoop builds
    /// ship the RED4ext they were tested with, so setup doesn't chase newer releases.
    fn r4_newer_on_github(&self) -> bool {
        match (self.r4_latest(), &self.status.red4ext_version) {
            (Some(l), Some(cur)) => cur.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) && util::cmp_versions(&l.version(), cur).is_gt(),
            _ => false,
        }
    }

    fn nc_update_available(&self) -> bool {
        match (self.nc_latest(), &self.status.nightcoop_version) {
            (Some(l), Some(cur)) => !util::same_version(&l.version, cur),
            _ => false,
        }
    }

    /// The manifest entry for the installed NightCoop build, if it's listed.
    fn installed_release(&self) -> Option<&remote::NcRelease> {
        let v = self.status.nightcoop_version.as_deref()?;
        match &self.remote.nightcoop {
            Some(Ok(m)) => m.releases.iter().find(|r| util::same_version(&r.version, v)),
            _ => None,
        }
    }

    fn share_text(&self) -> String {
        let s = &self.status;
        let protocol = self.installed_release().and_then(|r| r.protocol_str()).map(|p| format!(" · Protocol {p}")).unwrap_or_default();
        format!(
            "NightCoop {}{protocol} · RED4ext {} · Game {} · Phantom Liberty: {}",
            s.nightcoop_version.as_deref().unwrap_or("not installed"),
            s.red4ext_version.as_deref().unwrap_or("not installed"),
            s.game_version.as_deref().unwrap_or("?"),
            if s.phantom_liberty { "yes" } else { "no" }
        )
    }

    // ---------- the big button ----------

    fn start_setup(&mut self, ctx: &egui::Context, restart_steam: bool) {
        let Some(game) = self.status.game.clone() else { return };
        let needs_lo = self.status.needs_launch_option() && !self.status.launch_option_ok();
        let nc_installed_version = self.status.nightcoop_version.clone().filter(|_| self.status.nightcoop_installed);
        let nc_disabled = self.status.nightcoop_disabled;
        self.spawn(ctx, "Install / update everything", move |rep, state, settings| {
            if steam::game_running() {
                return Err(anyhow!("Cyberpunk 2077 is running. Quit the game first."));
            }
            if game.park_dir().exists() {
                ops::mods_on(&game, rep)?;
            }
            rep.log("Checking for the latest versions…");
            let info = fetch_remote(settings);
            let mut nc_installed_version = nc_installed_version;
            if let Some(Ok(m)) = &info.nightcoop {
                if nc_installed_version.as_deref().map(|v| v.starts_with("unknown")).unwrap_or(false) {
                    nc_installed_version = ops::identify_nightcoop(&game, m, state);
                }
            }

            if settings.backup_saves_before_install {
                ops::backup_saves(&game, settings, state, rep, "before-install", true)?;
            }

            // NightCoop first: each build ships the RED4ext it was tested with.
            if nc_disabled {
                ops::set_nightcoop_enabled(&game, true, rep)?;
            }
            let loader_ok = |g: &game::Game| g.dir.join("bin").join("x64").join("winmm.dll").is_file() && g.red4ext_dir().join("RED4ext.dll").is_file();
            match &info.nightcoop {
                Some(Ok(m)) => match m.latest_for(settings.channel) {
                    Some(rel) => {
                        let current = nc_installed_version.as_deref().map(|v| util::same_version(v, &rel.version)).unwrap_or(false);
                        // With the bundled choice, RED4ext must match what the build ships.
                        let r4_matches = state.red4ext_custom
                            || matches!((&state.red4ext_bundled, ops::installed_red4ext(&game)), (Some(b), Some(i)) if util::same_version(b, &i));
                        if current && loader_ok(&game) && r4_matches {
                            rep.log(format!("NightCoop {} is up to date.", rel.version));
                        } else {
                            ops::install_nightcoop_release(&game, rel, &settings.manifest_url, state, rep)?;
                        }
                    }
                    None => rep.log("NightCoop has no downloadable build yet. Check again later (or use “Install from file”)."),
                },
                Some(Err(e)) => rep.log(format!("Could not check NightCoop releases: {e}")),
                None => {}
            }

            // Fallback: RED4ext from GitHub when no NightCoop build provided it.
            if !loader_ok(&game) {
                let r4 = match &info.red4ext {
                    Some(Ok(list)) => list.iter().find(|r| !r.prerelease && remote::red4ext_asset(r).is_some()).cloned(),
                    _ => None,
                };
                match r4 {
                    Some(rel) => ops::install_red4ext_release(&game, &rel, state, rep)?,
                    None => return Err(anyhow!("RED4ext could not be downloaded. Check the internet connection and try again.")),
                }
            }

            // RED4ext needs Microsoft's VC++ runtime inside the Proton prefix (else: "error 998").
            // Done before the launch option: that step may stop at Steam, and this one doesn't need it.
            if game.uses_proton() {
                match game.steam.as_ref().and_then(steam::vc_runtime_ok) {
                    Some(false) => ops::install_vc_runtime(&game, rep)?,
                    Some(true) => rep.log("Visual C++ runtime is in place."),
                    None => rep.log("Start Cyberpunk once (until the menu), quit, then press this button again — the Visual C++ runtime goes into the game's Proton files, which appear on first start."),
                }
            }
            // Proton launch option
            if needs_lo {
                if steam::steam_running() && (!restart_steam || steam::in_gaming_mode()) {
                    rep.log(if steam::in_gaming_mode() {
                        "Skipped the Steam launch option: Steam can't be restarted in Gaming Mode. Switch to Desktop Mode and press this button again, or paste it in Cyberpunk → Properties → Launch Options."
                    } else {
                        "Skipped the Steam launch option (Steam is running). Set it on the Mods & Versions page, or paste it yourself."
                    });
                } else {
                    ops::apply_launch_option(&game, true, restart_steam, rep)?;
                }
            }
            // Everything is current for this game build now.
            state.game_build_id = game.steam.as_ref().and_then(|g| g.build_id.clone());
            Ok(Extra::Remote(info))
        });
    }

    /// Switches back to the RED4ext shipped with NightCoop and puts it in place now when possible.
    fn use_bundled_red4ext(&mut self, ctx: &egui::Context, game: &game::Game) {
        self.state.red4ext_custom = false;
        self.state.save();
        let rel = match &self.remote.nightcoop {
            Some(Ok(m)) => self
                .status
                .nightcoop_version
                .as_deref()
                .and_then(|v| m.releases.iter().find(|r| util::same_version(&r.version, v)))
                .or_else(|| m.latest_for(self.settings.channel))
                .cloned(),
            _ => None,
        };
        match rel {
            Some(rel) if self.status.nightcoop_installed => {
                let g = game.clone();
                self.spawn(ctx, "Restoring the bundled RED4ext", move |rep, state, settings| {
                    state.red4ext_custom = false;
                    ops::install_nightcoop_release(&g, &rel, &settings.manifest_url, state, rep).map(|_| Extra::None)
                });
            }
            _ => self.banner = Some((true, "Bundled RED4ext selected — it's installed with NightCoop on the next “Install everything”.".into())),
        }
    }

    fn request_setup(&mut self, ctx: &egui::Context) {
        let needs_lo = self.status.needs_launch_option() && !self.status.launch_option_ok();
        // Never offer to restart Steam in Gaming Mode: that would end the Deck's session.
        if needs_lo && steam::steam_running() && !steam::in_gaming_mode() {
            self.confirm = Some(Confirm::Setup);
        } else {
            self.start_setup(ctx, false);
        }
    }

    // ---------- UI pieces ----------

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("NIGHTCOOP").color(ACCENT).size(26.0).strong());
        ui.label(RichText::new("HELPER").color(CYAN).size(14.0).strong());
        ui.label(RichText::new(format!("v{}", selfupdate::VERSION)).color(DIM).small());
        ui.add_space(4.0);
        let platform = if steam::is_steam_deck() {
            "Steam Deck"
        } else if cfg!(windows) {
            "Windows"
        } else {
            "Linux"
        };
        ui.label(RichText::new(platform).color(DIM));
        ui.separator();
        for p in Page::ALL {
            let selected = self.page == p;
            let text = RichText::new(p.label()).size(16.0).color(if selected { ACCENT } else { Color32::from_gray(220) });
            if ui.add_sized([ui.available_width(), 38.0], egui::Button::selectable(selected, text)).clicked() {
                self.page = p;
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            if ui.link("Helper on GitHub").clicked() {
                util::open_path(format!("https://github.com/{}", selfupdate::HOME_REPO));
            }
            if ui.link("nightcoop.net").clicked() {
                util::open_path(remote::NIGHTCOOP_SITE);
            }
            if ui.link("NightCoop Discord").clicked() {
                util::open_path(remote::NIGHTCOOP_DISCORD);
            }
        });
    }

    fn job_bar(&mut self, ui: &mut egui::Ui) {
        if let Some((frac, label)) = &self.progress {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(label).color(CYAN));
                });
                if let Some(f) = frac {
                    ui.add(egui::ProgressBar::new(*f).show_percentage());
                }
            });
        }
        if let Some((ok, msg)) = self.banner.clone() {
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(if ok { "✔" } else { "⚠" }).color(if ok { OK } else { BAD }).size(18.0));
                    ui.label(RichText::new(msg).color(if ok { OK } else { BAD }));
                    if ui.small_button("✖").clicked() {
                        self.banner = None;
                    }
                });
            });
        }
        if self.helper_restart_ready {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("The helper was updated.").color(OK));
                    if ui.button("Restart now").clicked() {
                        selfupdate::restart();
                    }
                });
            });
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Co-op setup").size(28.0).strong());
        ui.label(RichText::new("Everything needed to play NightCoop, checked and installed for you.").color(DIM));
        ui.add_space(8.0);

        if self.status.game_running {
            warn_card(ui, "Cyberpunk 2077 is running. Quit the game before installing or updating.");
        }
        if self.status.mods_off {
            let enabled = !self.busy() && !self.status.game_running;
            card(ui, |ui| {
                ui.label(RichText::new("MODS ARE OFF").color(WARN).strong().size(18.0));
                ui.label("Cyberpunk starts unmodded (no Co-op menu). RED4ext and NightCoop are kept safe and come back with one click.");
                let b = egui::Button::new(RichText::new("🔌  Turn mods back ON").size(18.0).strong().color(Color32::BLACK)).fill(OK).min_size(egui::vec2(240.0, 44.0));
                if ui.add_enabled(enabled, b).clicked() {
                    if let Some(g) = self.status.game.clone() {
                        self.spawn(&ctx, "Turning mods on", move |rep, _, _| ops::mods_on(&g, rep).map(|_| Extra::None));
                    }
                }
            });
        }
        if self.status.game_updated && !self.status.mods_off {
            card(ui, |ui| {
                ui.label(RichText::new("⚠  Cyberpunk 2077 was updated since the mods were installed").color(WARN).strong());
                ui.label("Game patches often break RED4ext and NightCoop until they're updated too. Check for updates, then press Update everything.");
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy(), egui::Button::new("⟳  Check for updates")).clicked() {
                        self.check_updates(&ctx);
                    }
                    if ui.button("It works, dismiss").clicked() {
                        self.state.game_build_id = self.status.steam_build.clone();
                        self.state.save();
                        self.status.game_updated = false;
                    }
                });
            });
        }
        if matches!(&self.remote.nightcoop, Some(Ok(m)) if m.newer_format()) {
            warn_card(ui, "NightCoop changed the format of its release list. If installing or updating misbehaves, update this helper (Settings → Helper updates → Check now).");
        }
        if let Some(Ok(Some(u))) = self.remote.helper.clone() {
            let v = u.version.clone();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("A new helper version is available: {v}")).color(CYAN));
                    let notes = u.notes.clone().unwrap_or_default();
                    if ui.add_enabled(!self.busy(), egui::Button::new("Update helper")).on_hover_text(notes).clicked() {
                        let u = u.clone();
                        self.spawn(&ctx, "Updating helper", move |rep, _, _| {
                            selfupdate::apply(&u, rep)?;
                            Ok(Extra::HelperUpdated)
                        });
                    }
                });
            });
        }

        let s = self.status.clone();
        card(ui, |ui| {
            ui.label(RichText::new("STATUS").color(ACCENT).strong());
            ui.add_space(4.0);
            match &s.game {
                Some(g) => check_row(
                    ui,
                    Some(true),
                    "Cyberpunk 2077",
                    &[
                        Some(g.store.label().to_string()),
                        s.game_version.clone().map(|v| format!("exe {v}")),
                        s.phantom_liberty.then(|| "Phantom Liberty".to_string()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
                ),
                None => check_row(ui, Some(false), "Cyberpunk 2077", "not found — set the game folder in Settings"),
            }
            let r4_detail = match (&s.red4ext_version, s.loader_present) {
                (Some(v), true) if self.r4_newer_on_github() => format!("{v} · {} on GitHub (optional, Mods & Versions)", self.r4_latest().unwrap().version()),
                (Some(v), true) => format!("{v}{}", if self.state.red4ext_custom { " · your choice" } else if self.state.red4ext_bundled.is_some() { " · bundled with NightCoop" } else { "" }),
                (Some(_), false) => "loader (bin/x64/winmm.dll) missing — reinstall".into(),
                (None, _) => "not installed".into(),
            };
            let r4_ok = Some(s.red4ext_version.is_some() && s.loader_present);
            check_row(ui, r4_ok, "RED4ext (mod loader)", &r4_detail);

            let nc_detail = if s.mods_off && s.nightcoop_installed {
                format!("{} — switched off", s.nightcoop_version.clone().unwrap_or_default())
            } else if s.nightcoop_disabled {
                "disabled — enable it on Mods & Versions".to_string()
            } else {
                match (&s.nightcoop_version, s.nightcoop_installed) {
                    (Some(v), true) if self.nc_update_available() => format!("{v} → {} available", self.nc_latest().unwrap().version),
                    (Some(v), true) => v.clone(),
                    _ => match (&self.remote.nightcoop, self.nc_latest()) {
                        (_, Some(l)) => format!("not installed — v{} available", l.version),
                        (Some(Ok(_)), None) => "not installed — no build listed yet".into(),
                        _ => "not installed".into(),
                    },
                }
            };
            let nc_ok = if s.nightcoop_installed { if self.nc_update_available() { None } else { Some(true) } } else { Some(false) };
            check_row(ui, nc_ok, "NightCoop", &nc_detail);
            if let (Some(rel), Some(gv)) = (self.installed_release(), &s.game_version) {
                if !rel.game.is_empty() {
                    let ok = rel.game.iter().any(|g| util::version_covers(g, gv));
                    check_row(
                        ui,
                        Some(ok),
                        "Game version",
                        &if ok { format!("{gv} — supported by this build") } else { format!("{gv} — this build lists {} only; it may not load", rel.game.join(", ")) },
                    );
                }
            }

            if s.needs_launch_option() {
                let ok = s.launch_option_ok();
                check_row(ui, Some(ok), "Proton DLL override", if ok { "set in Steam launch options" } else { "not set — RED4ext won't load under Proton" });
                match s.vc_runtime {
                    Some(true) => check_row(ui, Some(true), "Visual C++ runtime", "installed in the game's Proton prefix"),
                    Some(false) => check_row(ui, Some(false), "Visual C++ runtime", "missing — RED4ext fails with “error 998”. Press the yellow button to add it."),
                    None => check_row(ui, None, "Visual C++ runtime", "start Cyberpunk once (until the menu), quit, then press the yellow button"),
                }
                if let Some(p) = &s.proton {
                    check_row(ui, Some(true), "Proton", p);
                }
            }
            let backup = self.backups.first().map(|(p, _)| p.file_stem().unwrap_or_default().to_string_lossy().to_string());
            check_row(
                ui,
                if backup.is_some() { Some(true) } else { None },
                "Save backup",
                &backup.map(|b| format!("latest: {}", b.trim_start_matches("saves-"))).unwrap_or_else(|| format!("none yet ({} saves)", s.save_count)),
            );
        });

        ui.add_space(6.0);
        let updates = self.nc_update_available();
        let all_good = self.status.ready() && !updates;
        ui.horizontal_wrapped(|ui| {
            let enabled = !self.busy() && self.status.game.is_some() && !self.status.game_running;
            let label = if all_good {
                "✔  Re-check & repair"
            } else if updates {
                "⬆  Install updates"
            } else if self.status.red4ext_version.is_some() || self.status.nightcoop_installed {
                "⬆  Update & repair"
            } else {
                "⬇  Install everything"
            };
            let big = egui::Button::new(RichText::new(label).size(20.0).strong().color(Color32::BLACK)).fill(ACCENT).min_size(egui::vec2(260.0, 52.0));
            if ui.add_enabled(enabled, big).clicked() {
                self.request_setup(&ctx);
            }
            let play = egui::Button::new(RichText::new("▶  Play Cyberpunk 2077").size(20.0).strong()).min_size(egui::vec2(240.0, 52.0));
            if ui.add_enabled(!self.status.game_running && self.status.game.as_ref().map(|g| g.steam.is_some()).unwrap_or(false), play).clicked() {
                steam::launch_game();
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new("⟳  Check for updates")).clicked() {
                self.check_updates(&ctx);
            }
            if ui.button("↻  Refresh status").clicked() {
                self.rescan();
            }
            if ui.button("👥  Play together").on_hover_text("Check you and your partner match, share invites, test your connection").clicked() {
                self.page = Page::Together;
            }
            let installed = (self.status.red4ext_version.is_some() || self.status.nightcoop_installed) && !self.status.mods_off;
            if installed
                && ui
                    .add_enabled(!self.busy() && !self.status.game_running, egui::Button::new("🔌  Mods off"))
                    .on_hover_text("Play unmodded. Everything is kept and comes back with one click — even after closing the helper.")
                    .clicked()
            {
                if let Some(g) = self.status.game.clone() {
                    self.spawn(&ctx, "Turning mods off", move |rep, state, _| ops::mods_off(&g, &state.nightcoop_extra_files.clone(), rep).map(|_| Extra::None));
                }
            }
        });
        if let Some(t) = &self.remote.checked_at {
            ui.label(RichText::new(format!("Last checked for updates: {t}")).color(DIM).small());
        }

        if matches!(&self.remote.nightcoop, Some(Ok(m)) if m.releases.is_empty()) {
            ui.add_space(6.0);
            let now = if self.status.needs_launch_option() { "RED4ext and the Steam launch option" } else { "RED4ext" };
            info_card(
                ui,
                &format!(
                    "No NightCoop build is listed right now. “Install everything” sets up {now}; press it again once a build is listed — \
                     or install a build you downloaded from Discord via Mods & Versions → Install from file."
                ),
            );
        }
        if self.status.ready() {
            ui.add_space(6.0);
            info_card(ui, "Ready! Open 👥 Play together to check you and your partner match, then: Co-op → Host a session (host) or Co-op → Join by code (guest).");
        }

        ui.add_space(10.0);
        egui::CollapsingHeader::new(RichText::new("Activity log").color(DIM)).default_open(self.busy()).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).show(ui, |ui| {
                for l in &self.log {
                    let c = if l.starts_with("ERROR") { BAD } else if l.starts_with("──") { CYAN } else { Color32::from_gray(200) };
                    ui.label(RichText::new(l).monospace().color(c));
                }
            });
        });
    }

    fn versions(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Mods & Versions").size(28.0).strong());
        let Some(game) = self.status.game.clone() else {
            warn_card(ui, "Game not found. Set the game folder in Settings.");
            return;
        };
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new("⟳  Check for updates")).clicked() {
                self.check_updates(&ctx);
            }
            ui.label("Channel:");
            let before = self.settings.channel;
            ui.selectable_value(&mut self.settings.channel, Channel::Stable, "Stable");
            ui.selectable_value(&mut self.settings.channel, Channel::Beta, "Beta");
            if before != self.settings.channel {
                self.settings.save();
            }
        });
        ui.add_space(6.0);

        // ----- NightCoop -----
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("NIGHTCOOP").color(ACCENT).strong());
                let cur = self.status.nightcoop_version.clone().unwrap_or_else(|| "not installed".into());
                ui.label(RichText::new(format!("installed: {cur}{}", if self.status.nightcoop_disabled { " (disabled)" } else { "" })).color(DIM));
            });
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("📂  Install from file…")).on_hover_text("A .zip or .7z build, e.g. from the NightCoop Discord").clicked() {
                    if let Some(file) = rfd::FileDialog::new().add_filter("NightCoop build", &["zip", "7z"]).pick_file() {
                        let g = game.clone();
                        self.spawn(&ctx, "Installing NightCoop from file", move |rep, state, _| {
                            ops::install_nightcoop_archive(&g, &file, None, None, state, rep)?;
                            Ok(Extra::None)
                        });
                    }
                }
                if self.status.nightcoop_disabled && ui.add_enabled(!self.busy(), egui::Button::new("▶  Enable NightCoop")).clicked() {
                    let g = game.clone();
                    self.spawn(&ctx, "Enabling NightCoop", move |rep, _, _| ops::set_nightcoop_enabled(&g, true, rep).map(|_| Extra::None));
                }
                if (self.status.nightcoop_installed || self.status.nightcoop_disabled) && ui.add_enabled(!self.busy(), egui::Button::new("🗑  Uninstall")).clicked() {
                    self.confirm = Some(Confirm::UninstallNightCoop);
                }
            });
            if !self.rollbacks.is_empty() {
                ui.collapsing("Previous builds (rollback)", |ui| {
                    for snap in self.rollbacks.clone() {
                        ui.horizontal(|ui| {
                            ui.label(snap.file_name().unwrap_or_default().to_string_lossy().replace("__", "  ·  build "));
                            if ui.add_enabled(!self.busy(), egui::Button::new("Restore")).clicked() {
                                self.confirm = Some(Confirm::Rollback(snap.clone()));
                            }
                        });
                    }
                });
            }
            ui.separator();
            match &self.remote.nightcoop {
                None => {
                    ui.label(RichText::new("Press “Check for updates” to see the available builds.").color(DIM));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("Couldn't load the build list: {e}")).color(BAD));
                }
                Some(Ok(m)) if m.releases.is_empty() => {
                    ui.label(RichText::new("No builds published yet.").color(DIM));
                }
                Some(Ok(m)) => {
                    let latest = m.latest_for(self.settings.channel).map(|r| r.version.clone());
                    let installed = self.status.nightcoop_version.clone();
                    let mut releases = m.releases.clone();
                    releases.sort_by(|a, b| util::cmp_versions(&b.version, &a.version));
                    for r in releases {
                        if self.settings.channel == Channel::Stable && !r.is_stable() && latest.as_deref() != Some(&r.version) {
                            continue;
                        }
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("v{}", r.version)).size(18.0).strong());
                            chip(ui, if r.is_stable() { "stable" } else { "beta" }, if r.is_stable() { OK } else { CYAN });
                            if latest.as_deref() == Some(&r.version) {
                                chip(ui, "latest", ACCENT);
                            }
                            if installed.as_deref().map(|v| util::same_version(v, &r.version)).unwrap_or(false) {
                                chip(ui, "installed", OK);
                            }
                            if r.is_yanked() {
                                chip(ui, "withdrawn", BAD);
                            }
                            if let Some(d) = &r.date {
                                ui.label(RichText::new(d).color(DIM));
                            }
                        });
                        if let Some(t) = r.title_text() {
                            ui.label(t);
                        }
                        let mut meta = Vec::new();
                        if !r.game.is_empty() {
                            meta.push(format!("Game: {}", r.game.join(", ")));
                        }
                        if let Some(p) = r.protocol_str() {
                            meta.push(format!("Protocol: {p}"));
                        }
                        if let Some(b) = &r.build {
                            meta.push(format!("Build {b}"));
                        }
                        if let Some(sz) = r.mod_file().and_then(|f| f.size) {
                            meta.push(util::human_size(sz));
                        }
                        if !meta.is_empty() {
                            ui.label(RichText::new(meta.join("  ·  ")).color(DIM));
                        }
                        if let Some(gv) = &self.status.game_version {
                            if !r.game.is_empty() && !r.game.iter().any(|g| util::version_covers(g, gv)) {
                                ui.label(RichText::new(format!("Not listed for your game version ({gv}) — it may not load.")).color(WARN));
                            }
                        }
                        if let Some(reason) = r.yank_reason() {
                            ui.label(RichText::new(format!("Withdrawn: {reason}")).color(BAD));
                        }
                        if let Some(n) = r.notes_text() {
                            ui.collapsing(format!("Notes for v{}", r.version), |ui| {
                                ui.label(n);
                            });
                        }
                        if r.mod_file().is_some() {
                            let label = if r.is_yanked() { "Install anyway" } else { "Install this version" };
                            if ui.add_enabled(!self.busy(), egui::Button::new(label)).clicked() {
                                self.confirm = Some(Confirm::InstallNcVersion(r.version.clone()));
                            }
                        }
                        ui.separator();
                    }
                    ui.label(RichText::new("Builds with the same protocol number can play together.").color(DIM).small());
                }
            }
        });

        // ----- RED4ext -----
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("RED4EXT").color(ACCENT).strong());
                let cur = self.status.red4ext_version.clone().unwrap_or_else(|| "not installed".into());
                ui.label(RichText::new(format!("installed: {cur}")).color(DIM));
            });
            ui.label("Which RED4ext should the helper keep installed?");
            let custom = self.state.red4ext_custom;
            let bundled_label = match &self.state.red4ext_bundled {
                Some(v) => format!("Bundled with NightCoop — {v} (recommended)"),
                None => "Bundled with NightCoop (recommended)".to_string(),
            };
            if ui.radio(!custom, bundled_label).on_hover_text("Each NightCoop build ships the RED4ext it was tested with; updates keep them matched.").clicked() && custom {
                self.use_bundled_red4ext(&ctx, &game);
            }
            if ui.radio(custom, "My own version").on_hover_text("Pick any release below. NightCoop updates will leave it alone.").clicked() && !custom {
                self.state.red4ext_custom = true;
                self.state.save();
            }
            if self.state.red4ext_custom {
                ui.add_space(4.0);
                match &self.remote.red4ext {
                    None => {
                        ui.label(RichText::new("Press “Check for updates” to see RED4ext releases.").color(DIM));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(format!("Couldn't reach GitHub: {e}")).color(BAD));
                    }
                    Some(Ok(list)) => {
                        let list: Vec<GhRelease> = list.iter().filter(|r| remote::red4ext_asset(r).is_some()).cloned().collect();
                        if !list.is_empty() {
                            self.r4_pick = self.r4_pick.min(list.len() - 1);
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt("r4pick")
                                    .selected_text(format!("{}  ({})", list[self.r4_pick].tag_name, list[self.r4_pick].date()))
                                    .show_ui(ui, |ui| {
                                        for (i, r) in list.iter().enumerate() {
                                            ui.selectable_value(&mut self.r4_pick, i, format!("{}  ({}){}", r.tag_name, r.date(), if r.prerelease { " pre-release" } else { "" }));
                                        }
                                    });
                                if ui.add_enabled(!self.busy(), egui::Button::new("Install selected")).clicked() {
                                    self.confirm = Some(Confirm::InstallR4Version(self.r4_pick));
                                }
                            });
                            if let Some(body) = &list[self.r4_pick].body {
                                ui.collapsing("Release notes", |ui| {
                                    egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| ui.label(body));
                                });
                            }
                        }
                    }
                }
                if ui.add_enabled(!self.busy(), egui::Button::new("📂  Install from file…")).clicked() {
                    if let Some(file) = rfd::FileDialog::new().add_filter("RED4ext zip", &["zip"]).pick_file() {
                        let g = game.clone();
                        self.spawn(&ctx, "Installing RED4ext from file", move |rep, state, _| {
                            let ver = file.file_stem().map(|s| s.to_string_lossy().trim_start_matches("red4ext-").to_string()).unwrap_or_default();
                            state.red4ext_custom = true;
                            ops::install_red4ext_archive(&g, &file, &ver, state, rep).map(|_| Extra::None)
                        });
                    }
                }
                ui.label(RichText::new("A RED4ext that doesn't match the NightCoop build may stop the mod from loading. Switch back to Bundled any time.").color(WARN).small());
            }
            if self.status.red4ext_version.is_some() && ui.add_enabled(!self.busy(), egui::Button::new("🗑  Uninstall RED4ext")).clicked() {
                self.confirm = Some(Confirm::UninstallRed4ext);
            }
        });

        // ----- remove everything -----
        if self.status.red4ext_version.is_some() || self.status.nightcoop_installed || self.status.nightcoop_disabled || self.status.mods_off {
            card(ui, |ui| {
                ui.label(RichText::new("REMOVE EVERYTHING").color(ACCENT).strong());
                ui.label(RichText::new("Want Cyberpunk stock again for good? This uninstalls NightCoop and RED4ext and removes the launch option in one go. For a break, use 🔌 Mods off on Home instead.").color(DIM));
                if ui.add_enabled(!self.busy() && !self.status.game_running, egui::Button::new(RichText::new("🗑  Remove everything").color(BAD))).clicked() {
                    self.confirm = Some(Confirm::RemoveAll);
                }
            });
        }

        // ----- launch options (only Proton needs them) -----
        if game.uses_proton() {
            card(ui, |ui| {
                ui.label(RichText::new("STEAM LAUNCH OPTIONS").color(ACCENT).strong());
                ui.label("Under Proton, Steam has to be told to load RED4ext's winmm.dll:");
                ui.horizontal(|ui| {
                    let text = format!("{} %command%", steam::DLL_OVERRIDE);
                    ui.label(RichText::new(&text).monospace().color(CYAN));
                    if ui.small_button("📋 Copy").clicked() {
                        ui.ctx().copy_text(text);
                    }
                });
                for u in &self.status.users {
                    let name = u.persona.clone().unwrap_or_else(|| format!("user {}", u.user_id));
                    let cur = u.launch_options.clone().unwrap_or_default();
                    let ok = steam::has_override(&cur);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(if ok { "✔" } else { "✖" }).color(if ok { OK } else { BAD }));
                        ui.label(format!("{name}:"));
                        ui.label(RichText::new(if cur.is_empty() { "(empty)".into() } else { cur }).monospace().color(DIM));
                    });
                }
                if steam::in_gaming_mode() {
                    ui.label(RichText::new("In Gaming Mode the helper can't restart Steam. Paste the line above into Cyberpunk → ⚙ Properties → Launch Options, or switch to Desktop Mode.").color(WARN));
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy() && !steam::in_gaming_mode(), egui::Button::new("Apply")).clicked() {
                        self.confirm = Some(Confirm::LaunchOption { enable: true });
                    }
                    if ui.add_enabled(!self.busy() && !steam::in_gaming_mode(), egui::Button::new("Remove")).clicked() {
                        self.confirm = Some(Confirm::LaunchOption { enable: false });
                    }
                });
            });
        }
    }

    fn saves(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Saves").size(28.0).strong());
        ui.label(RichText::new("The mod protects a guest's saves, but a copy costs nothing. Backups live in the helper's own folder.").color(DIM));
        let Some(game) = self.status.game.clone() else {
            warn_card(ui, "Game not found.");
            return;
        };
        card(ui, |ui| {
            match &self.status.saves_dir {
                Some(d) => {
                    ui.label(format!("{} saves · {}", self.status.save_count, util::human_size(self.status.saves_size)));
                    ui.label(RichText::new(d.display().to_string()).monospace().color(DIM).small());
                }
                None => {
                    ui.label("Saves folder unknown for this install.");
                }
            }
            if self.status.phantom_liberty {
                ui.label(RichText::new("Phantom Liberty saves won't load in a session hosted without the expansion.").color(WARN).small());
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new(RichText::new("💾  Back up now").strong())).clicked() {
                    let g = game.clone();
                    self.spawn(&ctx, "Backing up saves", move |rep, state, settings| ops::backup_saves(&g, settings, state, rep, "", false).map(|_| Extra::None));
                }
                if let Some(d) = &self.status.saves_dir {
                    if ui.button("Open saves folder").clicked() {
                        util::open_path(d);
                    }
                }
                if ui.button("Open backups folder").clicked() {
                    util::open_path(config::sub("backups").join("saves"));
                }
            });
        });
        card(ui, |ui| {
            ui.label(RichText::new("BACKUPS").color(ACCENT).strong());
            if self.backups.is_empty() {
                ui.label(RichText::new("No backups yet.").color(DIM));
            }
            for (path, size) in self.backups.clone() {
                ui.horizontal(|ui| {
                    let name = path.file_stem().unwrap_or_default().to_string_lossy().trim_start_matches("saves-").to_string();
                    ui.label(RichText::new(name).monospace());
                    ui.label(RichText::new(util::human_size(size)).color(DIM));
                    if ui.add_enabled(!self.busy(), egui::Button::new("Restore")).clicked() {
                        self.confirm = Some(Confirm::RestoreSaves(path.clone()));
                    }
                    if ui.button("Export…").on_hover_text("Save a copy anywhere: SD card, USB stick, a cloud folder").clicked() {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                        if let Some(dest) = rfd::FileDialog::new().set_file_name(&name).add_filter("Save backup", &["zip"]).save_file() {
                            self.banner = Some(match std::fs::copy(&path, &dest) {
                                Ok(_) => (true, format!("Backup exported to {}", dest.display())),
                                Err(e) => (false, format!("Export failed: {e}")),
                            });
                        }
                    }
                    if ui.add_enabled(!self.busy(), egui::Button::new("Delete")).clicked() {
                        self.confirm = Some(Confirm::DeleteBackup(path.clone()));
                    }
                });
            }
            ui.label(RichText::new(format!("Keeping the newest {} backups (change in Settings).", self.settings.keep_backups)).color(DIM).small());
        });
        card(ui, |ui| {
            ui.label(RichText::new("EXTRA COPY OF EVERY BACKUP").color(ACCENT).strong());
            ui.label(RichText::new(
                "Pick a second place for backups: the SD card, a USB stick, or a folder that syncs to the cloud (Dropbox, OneDrive, Google Drive, Syncthing…). Every new backup is copied there automatically.",
            )
            .color(DIM));
            let mut changed = false;
            ui.horizontal(|ui| {
                changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.backup_mirror).desired_width(380.0).hint_text("off")).lost_focus();
                if ui.button("Browse…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        self.settings.backup_mirror = d.display().to_string();
                        changed = true;
                    }
                }
                if !self.settings.backup_mirror.is_empty() && ui.button("Turn off").clicked() {
                    self.settings.backup_mirror.clear();
                    changed = true;
                }
            });
            let mirror = self.settings.backup_mirror.trim().to_string();
            if !mirror.is_empty() {
                let ok = std::path::Path::new(&mirror).is_dir();
                ui.label(RichText::new(if ok { "✔ Folder found" } else { "✖ Folder not found right now (SD card or drive not connected?)" }).color(if ok { OK } else { WARN }).small());
            }
            if changed {
                self.settings.save();
            }
        });
        card(ui, |ui| {
            ui.label(RichText::new("STEAM CLOUD").color(ACCENT).strong());
            match &self.status.cloud {
                Some(c) => {
                    ui.label(format!("Steam Cloud holds {} Cyberpunk files · last upload {}", c.files, c.newest_text()));
                }
                None if game.steam.is_some() => {
                    ui.label(RichText::new("No Steam Cloud record found for Cyberpunk on this device yet.").color(DIM));
                }
                None => {
                    ui.label(RichText::new("Steam Cloud only applies to the Steam version.").color(DIM));
                }
            }
            ui.label(RichText::new(
                "Steam uploads saves by itself when you quit the game, and downloads them when you start it — there is no button to force it. To sync now: start Cyberpunk, wait for the main menu, quit normally. Make sure “Keep game saves in the Steam Cloud” is on in Cyberpunk → Properties → General. Steam Cloud mirrors mistakes too, so the backups above                  are your undo button.",
            )
            .color(DIM)
            .small());
            if game.steam.is_some() && ui.button("Open Steam Cloud storage page").on_hover_text("Steam's website lists every file it keeps for each game").clicked() {
                util::open_path("https://store.steampowered.com/account/remotestorage");
            }
        });
    }

    fn together(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Play together").size(28.0).strong());
        ui.label(RichText::new("Everything for getting into the same session: matching versions, invites and a connection check.").color(DIM));
        ui.add_space(6.0);
        let mine = self.share_text();

        // 1 · matching versions
        card(ui, |ui| {
            ui.label(RichText::new("1 · SAME BUILD ON BOTH SIDES").color(ACCENT).strong());
            ui.label("Builds with the same protocol number play together. Send your partner your versions, paste theirs below.");
            ui.label(RichText::new(&mine).monospace().color(CYAN));
            ui.horizontal_wrapped(|ui| {
                if ui.button("📋  Copy my versions").clicked() {
                    ui.ctx().copy_text(mine.clone());
                    self.banner = Some((true, "Versions copied — send them to your partner.".into()));
                }
                if ui.button("✉  Copy a message for my partner").on_hover_text("Your versions plus the invite (if you pasted one below), ready to send in any chat").clicked() {
                    let inv = invite::find(&self.my_invite);
                    ui.ctx().copy_text(invite::join_message(inv.as_ref(), &mine));
                    self.banner = Some((true, "Message copied — paste it in Discord, Steam chat or a text message.".into()));
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new("What your partner sent you (their versions, their invite, or their whole message):").color(DIM));
            ui.add(egui::TextEdit::multiline(&mut self.partner_text).desired_rows(3).desired_width(f32::INFINITY).hint_text("Paste here (Ctrl+V)"));
            let partner = self.partner_text.trim().to_string();
            if !partner.is_empty() {
                let rows = compare_versions(&mine, &partner);
                let known = rows.iter().any(|r| r.2.is_some());
                if known {
                    // Same protocol is what decides; without it, the NightCoop version.
                    let verdict = if !self.status.nightcoop_installed {
                        None
                    } else {
                        rows.iter()
                            .find(|r| r.0 == "Protocol" && r.2.is_some() && !r.1.is_empty())
                            .or_else(|| rows.iter().find(|r| r.0 == "NightCoop" && r.2.is_some()))
                            .map(|r| r.3)
                    };
                    match verdict {
                        Some(true) => ui.label(RichText::new("✔  Your NightCoop builds match — you can play together.").color(OK).strong()),
                        Some(false) => ui.label(RichText::new("✖  Your NightCoop builds don't match — one of you needs to switch.").color(BAD).strong()),
                        None if !self.status.nightcoop_installed => ui.label(RichText::new("●  You don't have NightCoop yet — install their build below.").color(WARN).strong()),
                        None => ui.label(RichText::new("●  Couldn't find their NightCoop version in that text.").color(WARN)),
                    };
                    for (label, mine_v, theirs, same) in &rows {
                        let (icon, color) = match (theirs, same) {
                            (Some(_), true) => ("✔", OK),
                            (Some(_), false) => ("✖", if *label == "RED4ext" { WARN } else { BAD }),
                            (None, _) => ("●", DIM),
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(icon).color(color));
                            ui.label(RichText::new(*label).strong());
                            ui.label(RichText::new(format!("you: {}   partner: {}", if mine_v.is_empty() { "?" } else { mine_v }, theirs.as_deref().unwrap_or("?"))).color(DIM));
                        });
                    }
                    // One click to switch to the partner's build when it is listed.
                    let theirs_nc = rows.iter().find(|r| r.0 == "NightCoop").and_then(|r| r.2.clone());
                    if let (Some(t), Some(Ok(m))) = (theirs_nc, &self.remote.nightcoop) {
                        let differs = !self.status.nightcoop_version.as_deref().map(|v| util::same_version(v, &t)).unwrap_or(false);
                        if differs {
                            if let Some(rel) = m.releases.iter().find(|r| util::same_version(&r.version, &t)) {
                                let v = rel.version.clone();
                                let label = if self.status.nightcoop_installed { format!("⬇  Switch to their build (v{v})") } else { format!("⬇  Install their build (v{v})") };
                                if ui.add_enabled(!self.busy() && self.status.game.is_some(), egui::Button::new(label)).clicked() {
                                    self.confirm = Some(Confirm::InstallNcVersion(v));
                                }
                            } else {
                                ui.label(RichText::new("Their build isn't on the official list — maybe a newer one: both press ⟳ Check for updates and install the latest.").color(DIM).small());
                            }
                        }
                    }
                    if rows.iter().any(|r| r.0 == "Phantom Liberty" && r.2.is_some() && !r.3) {
                        ui.label(RichText::new("Phantom Liberty differs: host from a save that doesn't need the expansion.").color(WARN).small());
                    }
                }
                if let Some(inv) = invite::find(&partner) {
                    ui.separator();
                    ui.label(RichText::new("INVITE FOUND").color(ACCENT).strong());
                    invite_details(ui, &inv);
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("📋  Copy invite").clicked() {
                            ui.ctx().copy_text(inv.line());
                            self.banner = Some((true, "Invite copied — in the game: Co-op → Join by code → paste.".into()));
                        }
                        if ui.button("📋  Copy just the code").clicked() {
                            ui.ctx().copy_text(inv.code.clone());
                        }
                    });
                    ui.label(RichText::new("In the game: Co-op → Join by code → paste (Ctrl+V). On the Deck, see the paste tip below.").color(DIM).small());
                } else if !known {
                    ui.label(RichText::new("Nothing recognised yet — paste their versions (from “Copy my versions”) or an invite like ABC234@hub.example.org#K3QF-….").color(DIM));
                }
            }
        });

        // 2 · host and guest
        card(ui, |ui| {
            ui.label(RichText::new("2 · START THE SESSION").color(ACCENT).strong());
            ui.label(RichText::new("Host").strong().color(CYAN));
            ui.label("• Co-op → Host a session. Pick the world save everyone plays in (your save drives quests and the city), or New game.");
            ui.label("• Press “Copy the invite” in the lobby and send it. Everyone marks Ready.");
            ui.label("Paste your invite here to send it together with your versions (✉ button above):");
            ui.add(egui::TextEdit::singleline(&mut self.my_invite).desired_width(f32::INFINITY).hint_text("ABC234@…"));
            if let Some(inv) = invite::find(&self.my_invite) {
                invite_details(ui, &inv);
            } else if !self.my_invite.trim().is_empty() {
                ui.label(RichText::new("That doesn't look like an invite yet.").color(WARN).small());
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Guest").strong().color(CYAN));
            ui.label("• Co-op → Join by code → paste the invite (or type the code). You arrive in the host's world as your own V — your own saves are never changed.");
            ui.label("• Choices in dialogue go to a vote; fights and cutscenes are shared; a downed partner can be helped up.");
        });

        // 3 · connection check
        card(ui, |ui| {
            ui.label(RichText::new("3 · CONNECTION CHECK").color(ACCENT).strong());
            ui.label(RichText::new("Checks that NightCoop's server can be found and that your network lets the game's UDP traffic out. Takes a few seconds; nothing personal is sent.").color(DIM));
            if ui.add_enabled(!self.busy(), egui::Button::new("📡  Check my connection")).clicked() {
                self.spawn(&ctx, "Checking the connection", |rep, _, _| {
                    rep.progress(None, "Checking the connection…");
                    Ok(Extra::Connection(net::connection_check()))
                });
            }
            if let Some(r) = &self.connection {
                let good = r.hub_resolves && r.udp_works;
                ui.label(RichText::new(r.summary()).color(if !good { BAD } else if r.nat == net::Nat::Friendly { OK } else { WARN }).strong());
                check_row(ui, Some(r.hub_resolves), "NightCoop server found", if r.hub_resolves { "the name resolves" } else { "can't look up hub.nightcoop.net" });
                check_row(ui, Some(r.udp_works), "UDP gets out", if r.udp_works { "the game's traffic can leave your network" } else { "blocked or no internet" });
                let (ok, text) = match r.nat {
                    net::Nat::Friendly => (Some(true), "direct connections likely"),
                    net::Nat::Strict => (None, "strict router — the relay will be used"),
                    net::Nat::Unknown => (None, "couldn't tell"),
                };
                check_row(ui, ok, "Router", text);
                ui.label(RichText::new("Both of you can run this. If only one side fails, that network is the problem.").color(DIM).small());
            }
        });

        // 4 · Deck tips
        card(ui, |ui| {
            ui.label(RichText::new("STEAM DECK TIPS").color(ACCENT).strong());
            ui.label("• Paste an invite in Gaming Mode: map a back button to Ctrl+V once — Cyberpunk → 🎮 controller icon → Edit layout → Back grip buttons → L4 → Keyboard → V, then add a second command on the same button: Left Ctrl.");
            ui.label("• On-screen keyboard: STEAM + X. You can also just type the session code (e.g. ABC234); a private server needs the full invite.");
        });

        // 5 · servers
        let game = self.status.game.clone();
        egui::CollapsingHeader::new(RichText::new("🌐  Servers (advanced)").color(DIM)).show(ui, |ui| {
            ui.label(RichText::new(
                "By default the mod picks the nearest official server. A private server is added in-game (Co-op → Server → Add an own server) \
                 with an address and key fingerprint, like hub.example.org#K3QF-9X2M-TV7A-0D4R. Your list lives in servers.txt next to NightCoop.dll.",
            )
            .color(DIM));
            if ui.link("Run your own server → 🖥 Host a server").clicked() {
                self.page = Page::Server;
            }
            let Some(game) = game else { return };
            if !self.status.nightcoop_installed {
                ui.label(RichText::new("Install NightCoop first; servers.txt lives in its folder.").color(DIM));
                return;
            }
            let file = game.servers_file();
            if self.servers_text.is_none() {
                self.servers_text = Some(std::fs::read_to_string(&file).unwrap_or_default());
                self.servers_dirty = false;
            }
            if !file.exists() {
                ui.label(RichText::new("No servers.txt yet — it appears after you pick a server in-game.").color(DIM));
            }
            let text = self.servers_text.as_mut().unwrap();
            if ui.add(egui::TextEdit::multiline(text).code_editor().desired_rows(6).desired_width(f32::INFINITY)).changed() {
                self.servers_dirty = true;
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(self.servers_dirty, egui::Button::new("Save")).clicked() {
                    let _ = std::fs::copy(&file, config::sub("backups").join(format!("servers-{}.txt", util::timestamp())));
                    match std::fs::write(&file, self.servers_text.as_deref().unwrap_or_default()) {
                        Ok(()) => {
                            self.servers_dirty = false;
                            self.banner = Some((true, "servers.txt saved.".into()));
                        }
                        Err(e) => self.banner = Some((false, format!("Couldn't save servers.txt: {e}"))),
                    }
                }
                if ui.button("Reload").clicked() {
                    self.servers_text = None;
                }
                if ui.button("📋  Copy").clicked() {
                    ui.ctx().copy_text(self.servers_text.clone().unwrap_or_default());
                }
            });
        });
    }

    fn server_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Host your own server").size(28.0).strong());
        ui.label(RichText::new(
            "NightCoop players meet through a hub: a small server that hands out session codes, helps two players connect \
             directly, and relays the game when no direct path opens. The official hubs work for most people — your own is for when \
             they're far away or unreliable for you, or you just want your own.",
        )
        .color(DIM));
        info_card(ui, "Do you two play from the same home? Then you don't need this: on the same Wi-Fi your game traffic already goes directly between your devices.");

        // ----- choose where -----
        card(ui, |ui| {
            ui.label(RichText::new("WHERE SHOULD THE SERVER RUN?").color(ACCENT).strong());
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.server.place, HostPlace::ThisPc, if cfg!(windows) { "This Windows PC (WSL2)" } else { "This PC" });
                ui.selectable_value(&mut self.server.place, HostPlace::Vps, "A cheap Linux VPS (~$5/month)");
                ui.selectable_value(&mut self.server.place, HostPlace::LinuxPc, "Another Linux computer");
            });
            let hint = match self.server.place {
                HostPlace::ThisPc => "Free, but this PC must be on while you play, and your router has to let players in.",
                HostPlace::Vps => "Most reliable: always on, its own public address, no router setup at home. Recommended for groups.",
                HostPlace::LinuxPc => "A spare Linux machine at home (Ubuntu 24.04 / Debian 13). Same router setup as hosting on this PC.",
            };
            ui.label(RichText::new(hint).color(DIM));
        });

        match self.server.place {
            HostPlace::ThisPc => self.server_this_pc(ui, &ctx),
            HostPlace::Vps => self.server_vps(ui),
            HostPlace::LinuxPc => self.server_linux_pc(ui, &ctx),
        }

        // ----- use the server -----
        card(ui, |ui| {
            ui.label(RichText::new("USE YOUR SERVER").color(ACCENT).strong());
            ui.label("Your server's address with its key fingerprint (the setup prints it as NIGHTCOOP_HUB_ADDRESS=…):");
            ui.add(egui::TextEdit::singleline(&mut self.server.address).desired_width(f32::INFINITY).hint_text("203.0.113.7#K3QF-9X2M-TV7A-0D4R"));
            match hub::parse_address(&self.server.address) {
                Some(addr) => {
                    ui.label(RichText::new(format!("✔ {addr}")).color(OK));
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(self.status.nightcoop_installed, egui::Button::new("🎮  Use this server in my game")).on_hover_text("NightCoop picks it up the next time the game starts").clicked() {
                            self.banner = Some(match self.status.game.as_ref().map(|g| hub::use_in_game(g, &addr)) {
                                Some(Ok(())) => (true, "Done — start the game: your server is selected (Co-op → Server: … shows it).".into()),
                                Some(Err(e)) => (false, format!("{e:#}")),
                                None => (false, "Game not found.".into()),
                            });
                        }
                        if ui.button("📋  Copy address").clicked() {
                            ui.ctx().copy_text(addr.clone());
                        }
                    });
                    ui.label(RichText::new(
                        "Only the host needs this. Host a session and send the invite as usual — it carries the server, so your partner \
                         just pastes it into Join by code. In the game you can also add it by hand: Co-op → Server: … → Add an own server.",
                    )
                    .color(DIM)
                    .small());
                }
                None if !self.server.address.trim().is_empty() => {
                    ui.label(RichText::new("That doesn't look like address#FINGERPRINT yet.").color(WARN));
                }
                None => {}
            }
        });
        if ui.link("The official hub guide (all the details)").clicked() {
            util::open_path(hub::OFFICIAL_GUIDE);
        }
    }

    /// Router check shared by "this PC" and "a Linux PC at home".
    fn router_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        card(ui, |ui| {
            ui.label(RichText::new("YOUR ROUTER").color(ACCENT).strong());
            ui.label(format!(
                "Players reach a home server through your router: these ports must be forwarded to the computer running it — {}.",
                hub::ports_text(hub::HOME_THREADS)
            ));
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("🔎  Check my network")).clicked() {
                    self.spawn(ctx, "Checking your network", |rep, _, _| {
                        rep.progress(None, "Asking your router…");
                        Ok(Extra::Router(router_check(None)))
                    });
                }
                if ui.add_enabled(!self.busy(), egui::Button::new("🔓  Open the ports automatically (UPnP)")).on_hover_text("Asks your router to forward the ports to this computer").clicked() {
                    self.spawn(ctx, "Opening ports on your router", |rep, _, _| {
                        rep.progress(None, "Asking your router to forward the ports…");
                        let r = router_check(Some(true));
                        if r.ports_opened {
                            rep.log("The router now forwards the hub's ports to this computer.");
                        } else {
                            rep.log("The router didn't accept the forwarding (UPnP may be off). Forward the ports by hand — see the steps below.");
                        }
                        Ok(Extra::Router(r))
                    });
                }
                if ui.add_enabled(!self.busy(), egui::Button::new("Close them")).clicked() {
                    self.spawn(ctx, "Closing ports on your router", |_, _, _| Ok(Extra::Router(router_check(Some(false)))));
                }
            });
            if let Some(r) = self.server.router.clone() {
                match &r.router {
                    Ok((name, local, ext)) => {
                        check_row(ui, Some(true), "Router found", &format!("{name} · this computer is {local}"));
                        if r.cgnat() {
                            check_row(
                                ui,
                                Some(false),
                                "Public address",
                                &format!(
                                    "your router's address ({}) isn't the one the internet sees — your provider shares one address between customers (CGNAT). \
                                     Nobody outside can reach a server at home: use a VPS instead, or ask your provider for a public IPv4 address.",
                                    ext.map(|e| e.to_string()).unwrap_or_else(|| "?".into())
                                ),
                            );
                        } else {
                            check_row(ui, Some(true), "Public address", &format!("{} — hosting from home can work", r.public_ip.or(*ext).map(|e| e.to_string()).unwrap_or_else(|| "?".into())));
                        }
                        if r.ports_opened {
                            check_row(ui, Some(true), "Ports", "forwarded to this computer by UPnP");
                        }
                    }
                    Err(e) => {
                        check_row(ui, None, "Router", &format!("{e} — forward the ports by hand (below)."));
                        if let Some(ip) = r.public_ip {
                            check_row(ui, Some(true), "Public address", &ip.to_string());
                        }
                    }
                }
            }
            ui.collapsing("Forwarding the ports by hand", |ui| {
                ui.label("1. Open your router's page (often http://192.168.1.1 or http://192.168.0.1, or the router's phone app — eero, Google, Deco…).");
                ui.label("2. Find “Port forwarding” (sometimes under Advanced, NAT or Firewall).");
                ui.label(format!("3. Add a rule for {} to this computer's local address (shown above after “Check my network”).", hub::ports_text(hub::HOME_THREADS)));
                ui.label("4. Give this computer a fixed local address (“DHCP reservation”) so the rule keeps pointing at it.");
                ui.label("5. If your public address changes now and then, a free dynamic-DNS name (DuckDNS, No-IP…) can stand in for it in your server address.");
            });
        });
    }

    fn server_this_pc(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if steam::is_steam_deck() {
            warn_card(ui, "The Steam Deck isn't a good server (SteamOS resets system changes and the Deck sleeps). Use a VPS, or a Windows / Linux PC.");
            return;
        }
        if !cfg!(windows) {
            self.server_linux_pc(ui, ctx);
            return;
        }
        self.router_card(ui, ctx);
        let w = self.server.wsl.clone();
        let busy = self.busy();
        let mut action: Option<&'static str> = None;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("SET UP THE SERVER ON THIS PC").color(ACCENT).strong());
                if ui.add_enabled(!busy, egui::Button::new("↻  Check")).clicked() {
                    action = Some("check");
                }
            });
            ui.label(RichText::new(
                "The official hub runs on Linux, so it runs inside WSL2 — the Linux that's built into Windows. Each step below is one button; \
                 the two marked (admin) show one Windows permission prompt.",
            )
            .color(DIM));
            let Some(w) = w else {
                ui.label(RichText::new("Press ↻ Check to see what's already set up.").color(DIM));
                return;
            };
            if step_row(ui, 1, Some(w.installed && w.distro), "Install WSL with Ubuntu 24.04 (admin)", if w.installed && !w.distro {
                "WSL is installed. If Ubuntu doesn't show up, restart Windows, or press “Open Ubuntu once” and answer its first-start questions (any user name and password)."
            } else {
                "Takes a few minutes and may ask you to restart Windows. Then press ↻ Check."
            }, Some("Install"), !busy)
            {
                action = Some("install");
            }
            if w.installed && !w.distro && ui.add_enabled(!busy, egui::Button::new("Open Ubuntu once")).clicked() {
                action = Some("first_start");
            }
            if step_row(ui, 2, Some(w.mirrored), "Share Windows' network with Linux (mirrored mode)", "So players can reach the hub through this PC's address. Restarts WSL once. Needs Windows 11 22H2 or newer.", Some("Set up"), !busy) {
                action = Some("mirrored");
            }
            if step_row(ui, 3, Some(w.firewall), "Let the hub through the firewall (admin)", &format!("Opens {} in Windows Firewall and WSL's own firewall.", hub::ports_text(hub::HOME_THREADS)), Some("Open ports"), !busy) {
                action = Some("firewall");
            }
            if step_row(ui, 4, Some(w.built), "Build the official hub", "Downloads the hub's source from the NightCoop project and builds it inside Ubuntu (a few minutes). Press again later to update it.", Some(if w.built { "Rebuild / update" } else { "Build" }), !busy && w.distro) {
                action = Some("build");
            }
            if step_row(ui, 5, Some(w.running), "Run the server", if w.running {
                "Running. It keeps running after you close the helper, until you press Stop or Windows shuts down."
            } else {
                "Start it before you host a session. It stops when Windows shuts down — start it again next time."
            }, Some(if w.running { "Stop" } else { "Start" }), !busy && w.built)
            {
                action = Some(if w.running { "stop" } else { "start" });
            }
            if let Some(fp) = &w.fingerprint {
                ui.label(RichText::new(format!("Your server's key fingerprint: {fp}")).color(CYAN));
                ui.label(RichText::new("The key is backed up in the helper's data folder, so the fingerprint stays the same after a reinstall.").color(DIM).small());
            }
            let log = config::sub("logs").join("hub.log");
            if w.built && log.is_file() {
                ui.collapsing("Server log", |ui| {
                    for l in hub::log_tail(&log, 12) {
                        ui.label(RichText::new(l).monospace().size(11.0));
                    }
                });
            }
        });
        if let Some(a) = action {
            self.server_action(ctx, a);
        }
    }

    #[cfg(windows)]
    fn server_action(&mut self, ctx: &egui::Context, action: &'static str) {
        use hub::wsl;
        let name = computer_name();
        let title = match action {
            "install" => "Installing WSL and Ubuntu",
            "mirrored" => "Setting up WSL networking",
            "firewall" => "Opening the firewall",
            "build" => "Building the hub",
            "start" => "Starting the server",
            "stop" => "Stopping the server",
            "first_start" => {
                if let Err(e) = wsl::open_first_start() {
                    self.banner = Some((false, format!("{e:#}")));
                }
                return;
            }
            _ => "Checking the server setup",
        };
        self.spawn(ctx, title, move |rep, _, _| {
            match action {
                "install" => {
                    rep.progress(None, "Installing WSL and Ubuntu (accept the Windows prompt)…");
                    wsl::install()?;
                    rep.log("WSL setup finished. If Windows asks for a restart, restart and come back to this page.");
                }
                "mirrored" => {
                    wsl::set_mirrored()?;
                    rep.log("WSL now shares Windows' network (mirrored mode).");
                }
                "firewall" => {
                    rep.progress(None, "Opening the ports (accept the Windows prompt)…");
                    wsl::open_firewall(hub::HOME_THREADS)?;
                    rep.log("Firewall rules added for the hub's ports.");
                }
                "build" => {
                    rep.progress(None, "Building the hub inside Ubuntu (a few minutes)…");
                    let out = wsl::build_hub(&name)?;
                    if let Some(line) = out.lines().find(|l| l.starts_with("Hub source:")) {
                        rep.log(line.to_string());
                    }
                    rep.log("Hub built.");
                }
                "start" => {
                    rep.progress(None, "Starting the server…");
                    wsl::start(&name)?;
                    rep.log("Server running.");
                }
                "stop" => {
                    wsl::stop()?;
                    rep.log("Server stopped.");
                }
                _ => {}
            }
            rep.progress(None, "Checking the setup…");
            Ok(Extra::Wsl(wsl_status()))
        });
    }

    #[cfg(not(windows))]
    fn server_action(&mut self, _ctx: &egui::Context, _action: &'static str) {}

    fn server_vps(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.label(RichText::new("SET UP A VPS — ABOUT 10 MINUTES").color(ACCENT).strong());
            ui.label(RichText::new("1. Rent a small Linux server").strong());
            ui.label(
                "Any provider works (for example Hetzner, DigitalOcean, Vultr, Linode, OVH). Pick the smallest plan — 1 CPU and 512 MB–1 GB of RAM \
                 is plenty — with a public IPv4 address and generous traffic, in a region close to both of you. Choose Ubuntu 24.04 (or Debian 13) as the system. \
                 Set a root password or add an SSH key, and note the server's IP address.",
            );
            ui.label(RichText::new("2. Open the ports in the provider's firewall").strong());
            ui.label("In the provider's panel (Firewall / Security group / Networking), allow incoming UDP 47800 and 47810–47873 (or exactly 47810 plus one port per CPU core).");
            ui.label(RichText::new("3. Run the setup").strong());
            ui.label("Easiest: let the helper open a terminal that logs in and runs it — you only type the server's password there.");
            ui.horizontal_wrapped(|ui| {
                ui.label("Server IP:");
                ui.add(egui::TextEdit::singleline(&mut self.server.vps_host).desired_width(160.0).hint_text("203.0.113.7"));
                ui.label("User:");
                ui.add(egui::TextEdit::singleline(&mut self.server.vps_user).desired_width(80.0));
                if ui.add_enabled(!self.server.vps_host.trim().is_empty(), egui::Button::new("🖥  Open SSH and run the setup")).clicked() {
                    if let Err(e) = hub::open_ssh_setup(&self.server.vps_user, &self.server.vps_host) {
                        self.banner = Some((false, format!("{e:#}")));
                    }
                }
            });
            ui.label(RichText::new("Or paste this into the provider's web console (or your own SSH window):").color(DIM));
            let cmd = hub::setup_command(self.server.vps_user.trim() == "root" || self.server.vps_user.trim().is_empty());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&cmd).monospace().color(CYAN));
                if ui.small_button("📋 Copy").clicked() {
                    ui.ctx().copy_text(cmd.clone());
                }
            });
            ui.label(RichText::new("4. Copy the address it prints").strong());
            ui.label("At the end it prints a line like NIGHTCOOP_HUB_ADDRESS=203.0.113.7#K3QF-9X2M-TV7A-0D4R — paste it into “Use your server” below.");
            ui.label(RichText::new("Later").strong());
            ui.label("• Update the hub: run the same command again (the key, and so the fingerprint, stays).");
            ui.label("• Back up /var/lib/nightcoop/hub-sign.key somewhere safe — it is your server's identity.");
            ui.label("• Remove it: the same command with  | bash -s -- --uninstall  at the end.");
        });
    }

    fn server_linux_pc(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.router_card(ui, ctx);
        card(ui, |ui| {
            ui.label(RichText::new("SET UP A LINUX PC").color(ACCENT).strong());
            ui.label("On an Ubuntu 24.04 / Debian 13 computer, open a terminal and run:");
            let cmd = hub::setup_command(false);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&cmd).monospace().color(CYAN));
                if ui.small_button("📋 Copy").clicked() {
                    ui.ctx().copy_text(cmd.clone());
                }
            });
            ui.label(
                "It builds the official hub, runs it as a service that starts with the computer, and prints NIGHTCOOP_HUB_ADDRESS=… at the end. \
                 Forward the ports on your router to that computer (see above), then paste the address below.",
            );
        });
    }

    fn troubleshoot(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Troubleshoot").size(28.0).strong());
        let diag = self.status.diagnostics(&self.state);
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new(RichText::new("🐞  Create bug report (.zip)").strong())).on_hover_text("Logs from both RED4ext and NightCoop plus a diagnostics summary").clicked() {
                    let g = self.status.game.clone();
                    let d = diag.clone();
                    self.spawn(&ctx, "Creating bug report", move |rep, _, _| ops::bug_report(g.as_ref(), &d, rep).map(Extra::OpenFolderOf));
                }
                if ui.button("📋  Copy diagnostics").clicked() {
                    ui.ctx().copy_text(diag.clone());
                }
                if let Some(g) = &self.status.game.clone() {
                    if ui.button("Open game folder").clicked() {
                        util::open_path(&g.dir);
                    }
                    if ui.button("Open logs").clicked() {
                        util::open_path(g.red4ext_dir().join("logs"));
                    }
                    if g.uses_proton()
                        && ui
                            .add_enabled(!self.busy() && !self.status.game_running, egui::Button::new("Install Visual C++ runtime"))
                            .on_hover_text("Fixes “RED4ext (error 998)” under Proton")
                            .clicked()
                    {
                        let gc = g.clone();
                        self.spawn(&ctx, "Installing the Visual C++ runtime", move |rep, _, _| ops::install_vc_runtime(&gc, rep).map(|_| Extra::None));
                    }
                    if g.steam.is_some() && ui.button("Verify game files (Steam)").on_hover_text("Fixes broken game files. Mods stay installed.").clicked() {
                        util::open_path(format!("steam://validate/{}", steam::CYBERPUNK_APPID));
                    }
                }
                if ui.button("Open helper data folder").clicked() {
                    util::open_path(config::data_dir());
                }
            });
            ui.label(RichText::new("Send the report zip to the NightCoop Discord, with the time it happened and whether you were host or guest.").color(DIM).small());
        });
        card(ui, |ui| {
            ui.label(RichText::new("DIAGNOSTICS").color(ACCENT).strong());
            egui::ScrollArea::vertical().max_height(320.0).id_salt("diag").show(ui, |ui| {
                ui.label(RichText::new(&diag).monospace().size(12.0));
            });
        });
        card(ui, |ui| {
            ui.label(RichText::new("COMMON PROBLEMS").color(ACCENT).strong());
            let items: &[(&str, &str)] = &[
                ("No “Co-op” item in the main menu", "RED4ext didn't load the plugin. Check the diagnostics above: RED4ext and NightCoop must be installed, and on Steam Deck/Linux the Proton launch option must be set. The RED4ext log says why a plugin was skipped."),
                ("Worked before, broke after a game update", "Cyberpunk patches break RED4ext and plugins until they're updated. Press “Check for updates”, then “Update everything”. NightCoop lists the game versions each build supports."),
                ("Can't join each other", "Open 👥 Play together: paste your partner's versions to check the builds match (same protocol number), and both run the connection check — if only one side fails, that network is the problem (school/work Wi-Fi often blocks UDP)."),
                ("“RED4ext (error 998)” / “RED4ext could not be loaded” (Deck)", "Proton is missing Microsoft's Visual C++ runtime, which RED4ext needs. Press “Install everything” on Home (or the Visual C++ button above) — the helper installs it into the game's Proton files. Manual way: install Protontricks from Discover, then run in Konsole: flatpak run com.github.Matoking.protontricks 1091500 --force vcrun2022"),
                ("Game crashes on start (Deck)", "Try Proton Experimental or the latest Proton in Cyberpunk → Properties → Compatibility."),
                ("Stutter every few seconds", "Use Borderless window instead of exclusive fullscreen, and turn off the Discord overlay / frame generation."),
                ("“Server failed the key check”", "The server fingerprint doesn't match: a typo or the owner made a new key. Ask them for the current address#fingerprint."),
                ("Remove the mod for good", "Mods & Versions → 🗑 Remove everything uninstalls NightCoop, RED4ext and the launch option. Saves stay."),
                ("Want to play without mods", "Home → 🔌 Mods off. Cyberpunk then starts completely unmodded; your mods are kept and the Home page offers “Turn mods back ON” whenever you open the helper again."),
            ];
            for &(q, a) in items {
                ui.collapsing(q, |ui| {
                    ui.label(a);
                });
            }
        });
        if !self.status.log_lines.is_empty() {
            card(ui, |ui| {
                ui.label(RichText::new("LATEST RED4EXT LOG").color(ACCENT).strong());
                if let Some(f) = &self.status.log_file {
                    ui.label(RichText::new(f.display().to_string()).color(DIM).small());
                }
                egui::ScrollArea::vertical().max_height(220.0).id_salt("r4log").show(ui, |ui| {
                    for l in &self.status.log_lines {
                        let c = if l.to_ascii_lowercase().contains("error") { BAD } else if l.to_ascii_lowercase().contains("warn") { WARN } else { Color32::from_gray(200) };
                        ui.label(RichText::new(l).monospace().size(12.0).color(c));
                    }
                });
            });
        }
    }

    fn guide(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Steam Deck guide").size(28.0).strong());
        let steps: [(&str, &str); 6] = [
            ("1. Use Desktop Mode for setup", "Steam button → Power → Switch to Desktop. The helper can then change Steam's launch options for you (Steam restarts once)."),
            ("2. Press “Install everything”", "It backs up your saves, installs RED4ext and NightCoop, and sets the Proton DLL override. Press it again whenever an update is out."),
            ("3. Back to Gaming Mode", "Use the “Return to Gaming Mode” icon on the desktop and start Cyberpunk 2077 as usual."),
            ("4. Host or join", "Open 👥 Play together first to check you both match. In the game: Main menu → Co-op. The host picks Host a session and copies the invite; the guest chooses Join by code and pastes it."),
            ("Pasting an invite", "Map a back button to Ctrl+V once (controller layout → L4 → Keyboard V, plus a second command Left Ctrl) and pasting is one press. Or press STEAM + X and type the short session code."),
            ("Menu hotkeys", "If you need a keyboard key in-game, map a back button (L4/R4) to it: Steam button → Controller settings → Edit layout."),
        ];
        for (t, d) in steps {
            card(ui, |ui| {
                ui.label(RichText::new(t).color(ACCENT).strong().size(17.0));
                ui.label(d);
            });
        }
        let ctx = ui.ctx().clone();
        card(ui, |ui| {
            ui.label(RichText::new("Open this helper from Gaming Mode").color(ACCENT).strong().size(17.0));
            ui.label("Adds the helper to your Steam library as a non-Steam game, so you can check for updates without switching to Desktop Mode. Steam restarts once.");
            let possible = self.status.game.as_ref().and_then(|g| g.steam.as_ref()).is_some() || self.steam_found;
            if ui.add_enabled(possible && !self.busy() && !steam::in_gaming_mode(), egui::Button::new("➕  Add to Steam")).clicked() {
                self.confirm = Some(Confirm::AddToSteam);
            }
            let _ = &ctx;
        });
        card(ui, |ui| {
            ui.label(RichText::new("Good to know").color(ACCENT).strong().size(17.0));
            ui.label("• Everyone in a session needs the same NightCoop build. Use “Copy my versions” on Home to compare.");
            ui.label("• Saves made with Phantom Liberty won't load in a session hosted without it.");
            ui.label("• NightCoop officially targets Windows; Steam Deck runs it through Proton. If something only breaks on the Deck, mention that in bug reports.");
        });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("Settings").size(28.0).strong());
        let mut changed = false;
        card(ui, |ui| {
            ui.label(RichText::new("GAME FOLDER").color(ACCENT).strong());
            ui.label(RichText::new("Leave empty to detect automatically (Steam; GOG and Epic on Windows).").color(DIM));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.path_input).desired_width(420.0).hint_text("auto-detect"));
                if ui.button("Browse…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        self.path_input = d.display().to_string();
                    }
                }
                if ui.button("Use").clicked() {
                    let p = self.path_input.trim().to_string();
                    if p.is_empty() || game::is_game_dir(std::path::Path::new(&p)) {
                        self.settings.game_path = p;
                        changed = true;
                    } else {
                        self.banner = Some((false, "That folder doesn't contain bin/x64/Cyberpunk2077.exe.".into()));
                    }
                }
                if ui.button("Auto-detect").clicked() {
                    self.path_input.clear();
                    self.settings.game_path.clear();
                    changed = true;
                }
            });
            if let Some(g) = &self.status.game {
                ui.label(RichText::new(format!("Using: {} ({})", g.dir.display(), g.store.label())).color(OK));
            }
        });
        card(ui, |ui| {
            ui.label(RichText::new("GENERAL").color(ACCENT).strong());
            ui.horizontal(|ui| {
                ui.label("Update channel:");
                changed |= ui.selectable_value(&mut self.settings.channel, Channel::Stable, "Stable").changed();
                changed |= ui.selectable_value(&mut self.settings.channel, Channel::Beta, "Beta").changed();
            });
            changed |= ui.checkbox(&mut self.settings.check_updates_on_start, "Check for updates when the helper starts").changed();
            changed |= ui.checkbox(&mut self.settings.backup_saves_before_install, "Back up saves before installing or updating").changed();
            ui.horizontal(|ui| {
                ui.label("Save backups to keep:");
                changed |= ui.add(egui::DragValue::new(&mut self.settings.keep_backups).range(0..=100)).changed();
                ui.label(RichText::new("(0 = keep all)").color(DIM));
            });
            ui.horizontal(|ui| {
                ui.label("Interface size:");
                let r = ui.add(egui::Slider::new(&mut self.settings.ui_scale, 0.75..=2.5).step_by(0.05));
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    ctx.set_zoom_factor(self.settings.ui_scale);
                    changed = true;
                }
            });
        });
        card(ui, |ui| {
            ui.label(RichText::new("HELPER UPDATES").color(ACCENT).strong());
            ui.label(RichText::new("GitHub repository (owner/name) where new versions of this helper are published. Leave empty to turn off.").color(DIM));
            ui.horizontal(|ui| {
                changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.helper_repo).hint_text("owner/nightcoop-helper").desired_width(300.0)).lost_focus();
                if ui.add_enabled(!self.busy() && !self.settings.helper_repo.trim().is_empty(), egui::Button::new("Check now")).clicked() {
                    self.settings.save();
                    self.check_updates(&ctx);
                }
            });
            match &self.remote.helper {
                Some(Ok(Some(u))) => {
                    ui.label(RichText::new(format!("Version {} is available — see Home.", u.version)).color(CYAN));
                }
                Some(Ok(None)) => {
                    ui.label(RichText::new("The helper is up to date.").color(OK));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(BAD));
                }
                None => {}
            }
        });
        card(ui, |ui| {
            ui.label(RichText::new("ADVANCED").color(ACCENT).strong());
            ui.horizontal(|ui| {
                ui.label("NightCoop release list:");
                changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.manifest_url).desired_width(360.0)).lost_focus();
                if ui.button("Reset").clicked() {
                    self.settings.manifest_url = remote::NIGHTCOOP_MANIFEST.into();
                    changed = true;
                }
            });
            ui.label(RichText::new(format!("Helper data: {}", game::redact_home(&config::data_dir().display().to_string()))).color(DIM).small());
            ui.horizontal_wrapped(|ui| {
                let sizes = self.data_sizes.get_or_insert_with(data_sizes);
                ui.label(RichText::new(format!(
                    "Save backups {} · previous builds {} · download cache {}",
                    util::human_size(sizes.0),
                    util::human_size(sizes.1),
                    util::human_size(sizes.2)
                ))
                .color(DIM)
                .small());
                if ui.add_enabled(!self.busy(), egui::Button::new("🧹  Clear download cache")).on_hover_text("Backups and previous builds are kept").clicked() {
                    let freed = ops::clean_cache();
                    self.data_sizes = None;
                    self.banner = Some((true, format!("Freed {}.", util::human_size(freed))));
                }
            });
        });
        if changed {
            self.settings.save();
            self.rescan();
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(which) = &self.confirm else { return };
        let (title, body, yes, alt): (&str, String, &str, Option<&str>) = match which {
            Confirm::Setup => (
                "Steam needs to restart",
                "To set the Proton launch option, Steam has to be closed while the helper edits its settings, then it starts again automatically. Your downloads and games are not affected.".into(),
                "Restart Steam & install",
                Some("Install without it"),
            ),
            Confirm::LaunchOption { enable } => (
                if *enable { "Set launch option" } else { "Remove launch option" },
                if self.status.steam_running { "Steam will close and start again to save the change.".into() } else { "The launch option for Cyberpunk 2077 will be changed.".into() },
                "Continue",
                None,
            ),
            Confirm::RestoreSaves(p) => (
                "Restore saves",
                format!("Replace your current saves with the backup {}?\nYour current saves are backed up first.", p.file_name().unwrap_or_default().to_string_lossy()),
                "Restore",
                None,
            ),
            Confirm::DeleteBackup(p) => ("Delete backup", format!("Delete {} permanently?", p.file_name().unwrap_or_default().to_string_lossy()), "Delete", None),
            Confirm::UninstallNightCoop => ("Uninstall NightCoop", "Removes the NightCoop plugin. Saves stay where they are; servers.txt is kept in the helper's backups.".into(), "Uninstall", None),
            Confirm::UninstallRed4ext => ("Uninstall RED4ext", "Removes the RED4ext loader. Other RED4ext plugins (if any) will stop loading too.".into(), "Uninstall", None),
            Confirm::RemoveAll => (
                "Remove everything",
                format!(
                    "Uninstalls NightCoop and RED4ext{} so Cyberpunk is completely stock again. Your saves and the helper's backups stay.",
                    if self.status.needs_launch_option() && self.status.launch_option_any() {
                        if self.status.steam_running { " and removes the Steam launch option (Steam restarts)" } else { " and removes the Steam launch option" }
                    } else {
                        ""
                    }
                ),
                "Remove everything",
                None,
            ),
            Confirm::AddToSteam => (
                "Add helper to Steam",
                if self.status.steam_running { "Steam will close, the shortcut is added, then Steam starts again.".into() } else { "A non-Steam game shortcut will be added.".into() },
                "Add",
                None,
            ),
            Confirm::InstallNcVersion(v) => ("Install NightCoop", format!("Install NightCoop v{v}? Everyone in a session needs the same build."), "Install", None),
            Confirm::InstallR4Version(_) => ("Install RED4ext", "Install the selected RED4ext release?".into(), "Install", None),
            Confirm::Rollback(p) => ("Restore previous build", format!("Go back to the build saved at {}?", p.file_name().unwrap_or_default().to_string_lossy()), "Restore", None),
        };
        let mut choice: Option<u8> = None;
        egui::Modal::new(egui::Id::unique("nchelper-confirm")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new(title).size(20.0).strong().color(ACCENT));
            ui.add_space(6.0);
            ui.label(body);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new(yes).strong().color(Color32::BLACK)).fill(ACCENT)).clicked() {
                    choice = Some(1);
                }
                if let Some(a) = alt {
                    if ui.button(a).clicked() {
                        choice = Some(2);
                    }
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(0);
                }
            });
        });
        let Some(c) = choice else { return };
        let which = self.confirm.take().unwrap();
        if c == 0 {
            return;
        }
        let game = self.status.game.clone();
        match which {
            Confirm::Setup => self.start_setup(ctx, c == 1),
            Confirm::LaunchOption { enable } => {
                if let Some(g) = game {
                    self.spawn(ctx, if enable { "Setting launch option" } else { "Removing launch option" }, move |rep, _, _| {
                        ops::apply_launch_option(&g, enable, true, rep).map(|_| Extra::None)
                    });
                }
            }
            Confirm::RestoreSaves(p) => {
                if let Some(g) = game {
                    self.spawn(ctx, "Restoring saves", move |rep, state, settings| ops::restore_saves(&g, settings, state, &p, rep).map(|_| Extra::None));
                }
            }
            Confirm::DeleteBackup(p) => {
                let _ = std::fs::remove_file(&p);
                self.backups = ops::list_backups();
            }
            Confirm::UninstallNightCoop => {
                if let Some(g) = game {
                    self.spawn(ctx, "Uninstalling NightCoop", move |rep, state, _| ops::uninstall_nightcoop(&g, state, rep).map(|_| Extra::None));
                }
            }
            Confirm::RemoveAll => {
                if let Some(g) = game {
                    let launch_option = self.status.needs_launch_option() && self.status.launch_option_any();
                    self.spawn(ctx, "Removing everything", move |rep, state, _| {
                        if g.plugin_dir().exists() || g.disabled_plugin_dir().exists() || g.park_dir().exists() {
                            ops::uninstall_nightcoop(&g, state, rep)?;
                        }
                        if g.dir.join("bin").join("x64").join("winmm.dll").is_file() || g.red4ext_dir().exists() {
                            ops::uninstall_red4ext(&g, state, rep)?;
                        }
                        if launch_option {
                            if steam::in_gaming_mode() {
                                rep.log("The Steam launch option stays (can't restart Steam in Gaming Mode). It's harmless; remove it from Desktop Mode if you like.");
                            } else {
                                ops::apply_launch_option(&g, false, true, rep)?;
                            }
                        }
                        rep.log("Cyberpunk 2077 is back to stock.");
                        Ok(Extra::None)
                    });
                }
            }
            Confirm::UninstallRed4ext => {
                if let Some(g) = game {
                    self.spawn(ctx, "Uninstalling RED4ext", move |rep, state, _| ops::uninstall_red4ext(&g, state, rep).map(|_| Extra::None));
                }
            }
            Confirm::AddToSteam => {
                let root = game.and_then(|g| g.steam.map(|s| s.steam_root)).or_else(|| steam::steam_roots().into_iter().next());
                if let Some(root) = root {
                    self.spawn(ctx, "Adding helper to Steam", move |rep, _, _| {
                        let was_running = steam::steam_running();
                        if was_running {
                            rep.log("Closing Steam…");
                            steam::shutdown_steam(&root)?;
                        }
                        let exe = std::env::current_exe()?;
                        let n = shortcuts::add_to_steam(&root, &exe, "NightCoop Helper")?;
                        rep.log(format!("Added to the library of {n} Steam account(s). Find it under Non-Steam in Gaming Mode."));
                        if was_running {
                            steam::start_steam(&root)?;
                        }
                        Ok(Extra::None)
                    });
                }
            }
            Confirm::InstallNcVersion(v) => {
                let rel = match &self.remote.nightcoop {
                    Some(Ok(m)) => m.find(&v).cloned(),
                    _ => None,
                };
                if let (Some(g), Some(rel)) = (game, rel) {
                    self.spawn(ctx, &format!("Installing NightCoop {v}"), move |rep, state, settings| {
                        if settings.backup_saves_before_install {
                            ops::backup_saves(&g, settings, state, rep, "before-install", true)?;
                        }
                        ops::install_nightcoop_release(&g, &rel, &settings.manifest_url, state, rep).map(|_| Extra::None)
                    });
                }
            }
            Confirm::InstallR4Version(i) => {
                let rel = match &self.remote.red4ext {
                    Some(Ok(list)) => list.iter().filter(|r| remote::red4ext_asset(r).is_some()).nth(i).cloned(),
                    _ => None,
                };
                if let (Some(g), Some(rel)) = (game, rel) {
                    self.spawn(ctx, &format!("Installing RED4ext {}", rel.version()), move |rep, state, _| {
                        state.red4ext_custom = true;
                        ops::install_red4ext_release(&g, &rel, state, rep).map(|_| Extra::None)
                    });
                }
            }
            Confirm::Rollback(p) => {
                if let Some(g) = game {
                    self.spawn(ctx, "Restoring previous NightCoop build", move |rep, state, _| ops::restore_rollback(&g, &p, state, rep).map(|_| Extra::None));
                }
            }
        }
    }
}

/// Lines up the fields of two "Copy my versions" texts: (label, mine, theirs, matching).
fn compare_versions(mine: &str, theirs: &str) -> Vec<(&'static str, String, Option<String>, bool)> {
    /// Finds "<key> <value>" anywhere in the text (start of a part or after a space, "(" or ":"),
    /// keeping only values that look like what that key holds, so "NightCoop Helper" is not a version.
    fn field(text: &str, key: &str) -> Option<String> {
        let plausible = |v: &str| {
            let v = v.trim_start_matches(['v', 'V']);
            key == "Phantom Liberty" && matches!(v.to_ascii_lowercase().as_str(), "yes" | "no")
                || v.starts_with(|c: char| c.is_ascii_digit())
                || ["not installed", "unknown", "?"].iter().any(|w| v.to_ascii_lowercase().starts_with(w))
        };
        let lower_key = key.to_ascii_lowercase();
        text.split(['·', '|', '\n']).find_map(|part| {
            let lower = part.to_ascii_lowercase();
            lower.match_indices(&lower_key).find_map(|(i, _)| {
                let boundary = i == 0 || lower[..i].ends_with([' ', '(', ':', '\t']);
                let rest = &part[i + key.len()..];
                if !boundary || !rest.starts_with([' ', ':']) {
                    return None;
                }
                let value = rest.trim_start_matches([' ', ':']).trim().to_string();
                (!value.is_empty() && plausible(&value)).then_some(value)
            })
        })
    }
    ["NightCoop", "Protocol", "RED4ext", "Game", "Phantom Liberty"]
        .into_iter()
        .map(|key| {
            let m = field(mine, key).unwrap_or_default();
            let t = field(theirs, key);
            let same = t.as_deref().map(|t| {
                let numeric = t.chars().any(|c| c.is_ascii_digit()) && m.chars().any(|c| c.is_ascii_digit());
                if numeric { util::same_version(t, &m) } else { t.eq_ignore_ascii_case(&m) }
            });
            (key, m, t, same.unwrap_or(false))
        })
        .collect()
}

fn data_sizes() -> (u64, u64, u64) {
    let d = config::data_dir();
    (util::dir_size(&d.join("backups")), util::dir_size(&d.join("rollback")), util::dir_size(&d.join("downloads")) + util::dir_size(&d.join("staging")))
}


#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum HostPlace {
    #[default]
    ThisPc,
    Vps,
    LinuxPc,
}

#[derive(Default)]
struct ServerUi {
    place: HostPlace,
    router: Option<RouterCheck>,
    wsl: Option<WslStatus>,
    /// `host#FINGERPRINT` of the player's own server.
    address: String,
    vps_host: String,
    vps_user: String,
}

#[derive(Clone)]
struct RouterCheck {
    /// (router name, this PC's local address, the router's public address)
    router: Result<(String, std::net::Ipv4Addr, Option<std::net::Ipv4Addr>), String>,
    /// The public address the internet sees (STUN).
    public_ip: Option<std::net::Ipv4Addr>,
    ports_opened: bool,
}

impl RouterCheck {
    /// The router itself sits behind the provider's NAT: a home server can't be reached from outside.
    fn cgnat(&self) -> bool {
        match (&self.router, self.public_ip) {
            (Ok((_, _, Some(ext))), Some(public)) => upnp::is_carrier_or_private(*ext) || *ext != public,
            (Ok((_, _, Some(ext))), None) => upnp::is_carrier_or_private(*ext),
            _ => false,
        }
    }
}

#[derive(Clone, Default)]
struct WslStatus {
    installed: bool,
    distro: bool,
    mirrored: bool,
    firewall: bool,
    built: bool,
    running: bool,
    fingerprint: Option<String>,
    public_ip: Option<String>,
}

fn router_check(open_ports: Option<bool>) -> RouterCheck {
    let public_ip = net::public_ip();
    let mut ports_opened = false;
    let router = upnp::discover().map_err(|e| format!("{e:#}")).map(|r| {
        if let Some(open) = open_ports {
            ports_opened = hub::ports(hub::HOME_THREADS)
                .into_iter()
                .map(|p| if open { r.add_udp_port(p, "NightCoop hub") } else { r.remove_udp_port(p) })
                .all(|res| res.is_ok())
                && open;
        }
        (r.name.clone().unwrap_or_else(|| "your router".into()), r.local_ip, r.external_ip().ok())
    });
    RouterCheck { router, public_ip, ports_opened }
}

#[cfg(windows)]
fn wsl_status() -> WslStatus {
    use hub::wsl;
    let installed = wsl::distros().is_some();
    let distro = installed && wsl::distro_ready();
    let built = distro && wsl::hub_built();
    let running = built && wsl::running();
    WslStatus {
        installed,
        distro,
        mirrored: wsl::mirrored(),
        firewall: wsl::firewall_ok(),
        built,
        running,
        fingerprint: if built { wsl::fingerprint() } else { None },
        public_ip: if running { net::public_ip().map(|ip| ip.to_string()) } else { None },
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn wsl_status() -> WslStatus {
    WslStatus::default()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn computer_name() -> String {
    let n = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "home".into());
    let n: String = n.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(30).collect::<String>().to_ascii_lowercase();
    if n.is_empty() { "home".into() } else { n }
}

/// One numbered step: status mark, title, detail and (when given) a button. Returns true when clicked.
fn step_row(ui: &mut egui::Ui, n: usize, done: Option<bool>, title: &str, detail: &str, button: Option<&str>, enabled: bool) -> bool {
    let mut clicked = false;
    ui.horizontal_wrapped(|ui| {
        let (icon, color) = match done {
            Some(true) => ("✔", OK),
            Some(false) => ("●", WARN),
            None => ("·", DIM),
        };
        ui.label(RichText::new(icon).color(color).size(18.0));
        ui.label(RichText::new(format!("{n}. {title}")).strong());
        if let Some(b) = button {
            if done != Some(true) || b.starts_with("Re") || b.starts_with("Stop") || b.starts_with("Start") {
                clicked = ui.add_enabled(enabled, egui::Button::new(b)).clicked();
            }
        }
    });
    ui.label(RichText::new(detail).color(DIM).small());
    ui.add_space(2.0);
    clicked
}

fn fetch_remote(settings: &Settings) -> RemoteInfo {
    let repo = settings.helper_repo.trim();
    let (red4ext, nightcoop, helper) = std::thread::scope(|sc| {
        let r4 = sc.spawn(|| remote::github_releases(remote::RED4EXT_REPO).map_err(|e| format!("{e:#}")));
        let nc = sc.spawn(|| remote::nightcoop_manifest(&settings.manifest_url).map_err(|e| format!("{e:#}")));
        let hp = sc.spawn(|| (!repo.is_empty()).then(|| selfupdate::check(repo).map_err(|e| format!("{e:#}"))));
        let failed = "the check stopped unexpectedly".to_string();
        (Some(r4.join().unwrap_or_else(|_| Err(failed.clone()))), Some(nc.join().unwrap_or_else(|_| Err(failed))), hp.join().unwrap_or(None))
    });
    RemoteInfo { red4ext, nightcoop, helper, checked_at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()) }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        // Keep "Steam/game running" fresh without hammering the process list.
        if !self.busy() && self.last_poll.elapsed() > Duration::from_secs(4) {
            self.last_poll = Instant::now();
            self.status.steam_running = steam::steam_running();
            self.status.game_running = steam::game_running();
        }
        ctx.request_repaint_after(Duration::from_secs(4));

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(210.0)
            .frame(egui::Frame::new().fill(Color32::from_rgb(14, 14, 20)).inner_margin(12))
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(22, 22, 30)).inner_margin(16))
            .show(ui, |ui| {
                self.job_bar(ui);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.page {
                    Page::Home => self.home(ui),
                    Page::Versions => self.versions(ui),
                    Page::Saves => self.saves(ui),
                    Page::Together => self.together(ui),
                    Page::Server => self.server_page(ui),
                    Page::Troubleshoot => self.troubleshoot(ui),
                    Page::Guide => self.guide(ui),
                    Page::Settings => self.settings_page(ui),
                });
            });
        self.confirm_modal(&ctx);
    }
}

// ---------- widgets & style ----------

fn style(ctx: &egui::Context, scale: f32) {
    ctx.set_zoom_factor(scale);
    // Symbols like ● and → only exist in the monospace font; let normal text fall back to it.
    let mut fonts = egui::FontDefinitions::default();
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        if !prop.iter().any(|f| f == "Hack") {
            prop.push("Hack".to_owned());
        }
    }
    ctx.set_fonts(fonts);
    let mut visuals = egui::Visuals::dark();
    visuals.selection.bg_fill = Color32::from_rgb(90, 85, 10);
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = CYAN;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, CYAN);
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|s| {
        s.spacing.button_padding = egui::vec2(12.0, 7.0);
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.scroll.bar_width = 12.0;
    });
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let r = egui::Frame::new()
        .fill(Color32::from_rgb(30, 30, 42))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(55, 55, 75)))
        .corner_radius(8)
        .inner_margin(14)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner;
    ui.add_space(6.0);
    r
}

fn warn_card(ui: &mut egui::Ui, text: &str) {
    card(ui, |ui| {
        ui.label(RichText::new(format!("⚠  {text}")).color(WARN));
    });
}

fn info_card(ui: &mut egui::Ui, text: &str) {
    card(ui, |ui| {
        ui.label(RichText::new(format!("ℹ  {text}")).color(CYAN));
    });
}

fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).small());
        });
}

fn invite_details(ui: &mut egui::Ui, inv: &invite::Invite) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Code").strong());
        ui.label(RichText::new(&inv.code).monospace().size(18.0).color(CYAN));
    });
    let server = match (&inv.server, &inv.fingerprint) {
        (None, _) => "official servers".to_string(),
        (Some(s), Some(_)) => format!("{s} (private, key fingerprint included — verified)"),
        (Some(s), None) => format!("{s} (private, no fingerprint — connects but can't verify the server)"),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Server").strong());
        ui.label(RichText::new(server).color(DIM));
    });
}

/// One status line: ✔ (Some(true)), ✖ (Some(false)) or ● attention (None).
fn check_row(ui: &mut egui::Ui, ok: Option<bool>, title: &str, detail: &str) {
    ui.horizontal_wrapped(|ui| {
        let (icon, color) = match ok {
            Some(true) => ("✔", OK),
            Some(false) => ("✖", BAD),
            None => ("●", WARN),
        };
        ui.label(RichText::new(icon).color(color).size(18.0));
        ui.label(RichText::new(title).strong().size(16.0));
        ui.label(RichText::new(detail).color(DIM));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partner_compare() {
        let mine = "NightCoop 0.1.0-beta.1 · Protocol 128 · RED4ext 1.30.0 · Game 2.5 · Phantom Liberty: yes";
        let theirs = "NightCoop v0.1.0-beta.1 · Protocol 128 · RED4ext 1.29.0 · Game 2.5 · Phantom Liberty: no";
        let r = compare_versions(mine, theirs);
        let same: Vec<bool> = r.iter().map(|x| x.3).collect();
        assert_eq!(same, vec![true, true, false, true, false]);
        // An older helper's text has no protocol: that row is just unknown.
        let old = compare_versions(mine, "NightCoop 0.1.0-beta.1 · RED4ext 1.30.0 · Game 2.5 · Phantom Liberty: yes");
        assert!(old[1].2.is_none() && old[0].3);
        assert!(compare_versions(mine, "hello").iter().all(|x| x.2.is_none()));
        // The partner message from "Copy a message for my partner" is understood too.
        let msg = crate::invite::join_message(None, "NightCoop 0.1.0-beta.1 · Protocol 128 · RED4ext 1.30.0 · Game 2.5 · Phantom Liberty: yes");
        let r = compare_versions(mine, &msg);
        assert!(r.iter().all(|x| x.2.is_some() && x.3), "{r:?}");
        let one_line = compare_versions(mine, "Invite: QX72KD@hub.example.org#K3QF-9X2M  My versions: NightCoop 0.1.0-beta.1 · Protocol 128");
        assert_eq!(one_line[0].2.as_deref(), Some("0.1.0-beta.1"));
        assert!(compare_versions(mine, "open NightCoop Helper and Play together")[0].2.is_none());
    }
}
