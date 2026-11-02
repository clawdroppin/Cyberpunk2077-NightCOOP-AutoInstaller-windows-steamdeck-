//! The GUI.

use crate::config::{self, Channel, InstallState, Settings};
use crate::game::{self, Status};
use crate::ops::{self, Event, Reporter};
use crate::remote::{self, GhRelease, NcManifest};
use crate::selfupdate::{self, HelperUpdate};
use crate::{ai, hub, invite, net, shortcuts, steam, upnp, util};
use anyhow::{anyhow, Result};
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
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
    Ai,
    Guide,
    Settings,
}

impl Page {
    const ALL: [Page; 9] = [Page::Home, Page::Together, Page::Versions, Page::Saves, Page::Server, Page::Troubleshoot, Page::Ai, Page::Guide, Page::Settings];
    fn label(self) -> &'static str {
        match self {
            Page::Home => "🏠  Home",
            Page::Versions => "📦  Mods & Versions",
            Page::Saves => "💾  Saves",
            Page::Together => "👥  Play together",
            Page::Server => "🖥  Host a server",
            Page::Troubleshoot => "🔧  Troubleshoot",
            Page::Ai => "💬  AI helper",
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
    /// Newest commit of the official hub source (only looked up when a hub is built here).
    hub: Option<Result<String, String>>,
    checked_at: Option<String>,
}

enum Extra {
    None,
    Remote(RemoteInfo),
    OpenFolderOf(PathBuf),
    HelperUpdated,
    Connection(net::CheckResult),
    Router(RouterCheck),
    Hub(HubStatus),
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
    ai: AiUi,
    /// The official hub source changed and the player wants the server kept up to date.
    hub_update_due: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::with_ctx(&cc.egui_ctx)
    }

    fn with_ctx(ctx: &egui::Context) -> Self {
        let settings = Settings::load();
        let state = InstallState::load();
        style(ctx, settings.ui_scale);
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
            ai: AiUi::default(),
            hub_update_due: false,
        };
        // NCH_PAGE=versions|saves|… opens a page directly (handy for screenshots and testing).
        if let Ok(p) = std::env::var("NCH_PAGE") {
            if let Some(page) = Page::ALL.into_iter().find(|pg| pg.label().to_ascii_lowercase().contains(&p.to_ascii_lowercase())) {
                app.page = page;
            }
        }
        if app.settings.check_updates_on_start {
            app.check_updates(ctx);
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
                            Extra::Hub(h) => {
                                if let (Some(fp), Some(ip)) = (&h.fingerprint, &h.public_ip) {
                                    if hub::parse_address(&self.server.address).is_none() {
                                        self.server.address = format!("{ip}#{fp}");
                                    }
                                }
                                self.server.hub = Some(h);
                            }
                            Extra::None => {}
                        }
                        if !["Checking for updates", "Checking the connection", "Checking your network", "Checking the server setup", "Changing server autostart"].contains(&job.title.as_str()) {
                            self.banner = Some((true, format!("{} — done.", job.title)));
                        }
                    }
                    Err(e) => {
                        self.log.push(format!("ERROR: {e}"));
                        self.banner = Some((false, format!("{} failed: {e}", job.title)));
                    }
                }
            }
            Err(_) => {
                self.log.push(format!("ERROR: {} crashed unexpectedly.", job.title));
                self.banner = Some((false, format!("{} crashed unexpectedly.", job.title)));
            }
        }
        self.rescan();
        self.ai_job_finished();
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
        if let Some(Ok(latest)) = &info.hub {
            if hub::update_available(latest) {
                self.hub_update_due = self.settings.hub_auto_update;
                if let Some(h) = &mut self.server.hub {
                    h.update_available = true;
                }
            }
        }
        if info.hub.is_some() {
            self.remote.hub = info.hub;
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
                        // Same version but a zip made for another game version (the game was updated)?
                        let wanted_sha = rel.mod_file_for(util::pe_version(&game.exe()).as_deref()).and_then(|f| f.sha256.clone());
                        let right_file = match (&wanted_sha, &state.nightcoop_sha256) {
                            (Some(w), Some(have)) => w.eq_ignore_ascii_case(have),
                            _ => true,
                        };
                        if !right_file {
                            rep.log(format!("The installed NightCoop {} was made for another game version; switching to the right one.", rel.version));
                        }
                        let current = right_file && nc_installed_version.as_deref().map(|v| util::same_version(v, &rel.version)).unwrap_or(false);
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
        if !self.settings.hub_auto_update && matches!(&self.remote.hub, Some(Ok(c)) if hub::update_available(c)) {
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("A newer version of the NightCoop hub (your server) is out.").color(CYAN));
                    if ui.add_enabled(!self.busy(), egui::Button::new("⬆  Update the server")).clicked() {
                        self.server_action(&ctx, "update");
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
                        let file = r.mod_file_for(self.status.game_version.as_deref());
                        if let Some(sz) = file.and_then(|f| f.size) {
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
                        if r.mod_file().is_some() && file.is_none() {
                            ui.label(RichText::new(format!("No download for your game version — this build comes for game {} only.", r.file_game_versions().join(", "))).color(WARN));
                        }
                        if file.is_some() {
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
                "Steam uploads saves by itself when you quit the game, and downloads them when you start it — there is no button to force it. To sync now: start Cyberpunk, wait for the main menu, quit normally. Make sure “Keep game saves in the Steam Cloud” is on in Cyberpunk → Properties → General. Steam Cloud mirrors mistakes too, so the backups above are your undo button.",
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
                ui.selectable_value(&mut self.server.place, HostPlace::ThisPc, "This computer");
                ui.selectable_value(&mut self.server.place, HostPlace::Vps, "A cheap Linux VPS (~$5/month)");
                ui.selectable_value(&mut self.server.place, HostPlace::LinuxPc, "Another Linux computer");
            });
            let hint = match self.server.place {
                HostPlace::ThisPc => "Free and automatic. This computer must be on while you play, and your router has to let players in.",
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
            warn_card(ui, "The Steam Deck makes a poor server: it sleeps, and SteamOS updates can reset things. A VPS or a Windows / Linux PC is better — but it works for a quick session.");
        }
        if self.server.hub.is_none() && !self.server.checked {
            self.server.checked = true;
            self.server_action(ctx, "check");
        }
        let h = self.server.hub.clone().unwrap_or_default();
        let busy = self.busy();
        let mut action: Option<&'static str> = None;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("RUN THE SERVER ON THIS PC").color(ACCENT).strong());
                if ui.add_enabled(!busy, egui::Button::new("↻  Check")).clicked() {
                    action = Some("check");
                }
            });
            ui.label(RichText::new(
                "The helper downloads the official hub source and a portable compiler, builds the hub on this computer and runs it — \
                 no Linux, no virtual machine, no installer. Everything stays in the helper's own folder.",
            )
            .color(DIM));
            let build_detail = match (&h.build_info, h.update_available) {
                (Some(i), true) => format!("Built ({i}). A newer hub source is out — press Update."),
                (Some(i), false) => format!("Built ({i}). Press again any time to rebuild from the newest source."),
                (None, _) => "Downloads about 100 MB once (the compiler) and builds in a few minutes.".to_string(),
            };
            let build_label = if h.update_available { "⬆ Update" } else if h.built { "Rebuild" } else { "Build" };
            if step_row(ui, 1, Some(h.built && !h.update_available), "Build the official hub", &build_detail, Some(build_label), !busy) {
                action = Some(if h.built { "update" } else { "build" });
            }
            if cfg!(windows) {
                if step_row(ui, 2, Some(h.firewall == Some(true)), "Let the hub through Windows Firewall (admin)", &format!("Opens {} for incoming players. One Windows permission prompt.", hub::ports_text(hub::HOME_THREADS)), Some("Open ports"), !busy) {
                    action = Some("firewall");
                }
            } else {
                step_row(ui, 2, None, "Firewall", &format!("Most desktop Linux has none. If you use ufw: sudo ufw allow {}/udp && sudo ufw allow {}:{}/udp", hub::PORT, hub::PORT + 10, hub::PORT + 9 + hub::HOME_THREADS), None, false);
            }
            if step_row(ui, 3, Some(h.running), "Run the server", if h.running {
                "Running. It keeps running after you close the helper, until you press Stop or the computer shuts down."
            } else {
                "Start it before you host a session."
            }, Some(if h.running { "Stop" } else { "Start" }), !busy && h.built)
            {
                action = Some(if h.running { "stop" } else { "start" });
            }
            ui.horizontal_wrapped(|ui| {
                let mut auto = h.autostart;
                if ui.add_enabled(h.built && !busy, egui::Checkbox::new(&mut auto, "Start the server when this computer starts")).changed() {
                    action = Some(if auto { "autostart_on" } else { "autostart_off" });
                }
                let mut upd = self.settings.hub_auto_update;
                if ui.checkbox(&mut upd, "Update the server automatically").on_hover_text("When the helper checks for updates and the official hub source changed, it rebuilds and restarts the server").changed() {
                    self.settings.hub_auto_update = upd;
                    self.settings.save();
                }
            });
            if let Some(fp) = &h.fingerprint {
                ui.label(RichText::new(format!("Your server's key fingerprint: {fp}")).color(CYAN));
                ui.label(RichText::new("The key is backed up in the helper's data folder, so the fingerprint stays the same after a rebuild.").color(DIM).small());
            }
            let log = hub::log_file();
            if h.built && log.is_file() {
                ui.collapsing("Server log", |ui| {
                    for l in hub::log_tail(&log, 12) {
                        ui.label(RichText::new(l).monospace().size(11.0));
                    }
                });
            }
        });
        self.router_card(ui, ctx);
        if let Some(a) = action {
            self.server_action(ctx, a);
        }
    }

    fn server_action(&mut self, ctx: &egui::Context, action: &'static str) {
        let name = hub::computer_name();
        let title = match action {
            "build" => "Building the server",
            "update" => "Updating the server",
            "firewall" => "Opening the firewall",
            "start" => "Starting the server",
            "stop" => "Stopping the server",
            "autostart_on" | "autostart_off" => "Changing server autostart",
            _ => "Checking the server setup",
        };
        self.spawn(ctx, title, move |rep, _, _| {
            match action {
                "build" | "update" => {
                    let info = hub::update(rep, &name)?;
                    rep.log(format!("Hub built: {info}."));
                }
                "firewall" => {
                    #[cfg(windows)]
                    {
                        rep.progress(None, "Opening the ports (accept the Windows prompt)…");
                        hub::firewall::open(hub::HOME_THREADS)?;
                        rep.log("Firewall rule added for the hub's ports.");
                    }
                }
                "start" => {
                    rep.progress(None, "Starting the server…");
                    hub::start(&name)?;
                    rep.log("Server running.");
                }
                "stop" => {
                    hub::stop()?;
                    rep.log("Server stopped.");
                }
                "autostart_on" => hub::set_autostart(true)?,
                "autostart_off" => hub::set_autostart(false)?,
                _ => {}
            }
            rep.progress(None, "Checking the setup…");
            Ok(Extra::Hub(hub_status()))
        });
    }

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
            ui.label("Easiest: copy this helper to that computer and use “This computer” there. Or, on an Ubuntu 24.04 / Debian 13 computer, run this in a terminal to install it as a system service:");
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
                if ui.button("💬  Ask the AI helper").on_hover_text("Your own local AI looks at this setup and explains what to do").clicked() {
                    if self.ai.messages.is_empty() && self.ai.input.is_empty() {
                        self.ai.input = "Something isn't working. Look at my setup: what's wrong and what should I do?".into();
                    }
                    self.page = Page::Ai;
                }
                if let Some(g) = &self.status.game.clone() {
                    if ui.button("Open game folder").clicked() {
                        util::open_path(&g.dir);
                    }
                    if ui.button("Open logs").clicked() {
                        util::open_path(g.active_red4ext_dir().join("logs"));
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

    // ---------- AI helper ----------

    fn poll_ai(&mut self) {
        if let Some((rx, _)) = &self.ai.stream {
            let mut finished = false;
            loop {
                match rx.try_recv() {
                    Ok(ai::Event::Delta(t)) => match self.ai.messages.last_mut() {
                        Some(m) if m.role == "assistant" => m.content.push_str(&t),
                        _ if t.trim().is_empty() => {}
                        _ => self.ai.messages.push(ai::Message { role: "assistant", content: t }),
                    },
                    Ok(ai::Event::Tools(calls)) => {
                        for call in calls {
                            let state = match ai::risk(&call.name) {
                                ai::Risk::Look => ActState::Ready,
                                ai::Risk::Change if self.settings.ai_auto_approve => ActState::Ready,
                                _ => ActState::Ask,
                            };
                            let label = self.ai_label(&call);
                            self.ai.actions.push(AiAction { call, label, state });
                        }
                    }
                    Ok(ai::Event::Notice(n)) => self.ai.notice = Some(n),
                    Ok(ai::Event::Done) | Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                    Ok(ai::Event::Error(e)) => {
                        self.ai.error = Some(e);
                        finished = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            if finished {
                self.ai.stream = None;
                self.ai.results = None;
                self.ai.actions.clear();
                self.ai.messages.retain(|m| !m.content.trim().is_empty());
            }
        }
        if let Some(rx) = &self.ai.probe {
            match rx.try_recv() {
                Ok(Ok((url, models))) => {
                    self.ai.probe = None;
                    if url != self.settings.ai_url {
                        self.settings.ai_url = url;
                        self.settings.save();
                    }
                    self.ai.connection = Some(Ok(if models.is_empty() {
                        "Connected, but no model is downloaded yet (e.g. ollama pull llama3.1:8b).".into()
                    } else {
                        format!("Connected · {} model(s)", models.len())
                    }));
                    self.ai.models = models;
                }
                Ok(Err(e)) => {
                    self.ai.probe = None;
                    self.ai.connection = Some(Err(e));
                }
                Err(TryRecvError::Disconnected) => self.ai.probe = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    /// Checks the AI server in the background; `find` also tries the usual local addresses.
    fn ai_probe(&mut self, ctx: &egui::Context, find: bool) {
        if self.ai.probe.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (url, key) = (self.settings.ai_url.trim().to_string(), self.settings.ai_key.clone());
        let c = ctx.clone();
        std::thread::spawn(move || {
            let mut result = ai::list_models(&url, &key).map(|m| (url.clone(), m));
            if find && result.is_err() {
                result = match ai::find_local() {
                    Some((_, found, models)) => Ok((found.to_string(), models)),
                    None => Err("No AI found on this computer. Install Ollama or LM Studio (steps above), or enter the address of the computer running it.".into()),
                };
            }
            let _ = tx.send(result);
            c.request_repaint();
        });
        self.ai.probe = Some(rx);
        self.ai.connection = None;
    }

    fn ai_send(&mut self, ctx: &egui::Context) {
        let question = self.ai.input.trim().to_string();
        if question.is_empty() || self.ai.stream.is_some() {
            return;
        }
        self.ai.input.clear();
        self.ai.error = None;
        self.ai.messages.push(ai::Message { role: "user", content: question });
        let history = self.ai.messages.clone();
        let diagnostics = self.settings.ai_share_status.then(|| self.status.diagnostics(&self.state));
        // So the AI knows about updates without having to ask for them first.
        if self.remote.nightcoop.is_none() && !self.busy() {
            self.check_updates(ctx);
        }
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (url, key, model) = (self.settings.ai_url.clone(), self.settings.ai_key.clone(), self.settings.ai_model.clone());
        let (c, s) = (ctx.clone(), stop.clone());
        let (results_tx, results_rx) = mpsc::channel();
        self.ai.results = Some(results_tx);
        self.ai.notice = None;
        std::thread::spawn(move || ai::chat(url, key, model, diagnostics, history, Some(results_rx), s, tx, move || c.request_repaint()));
        self.ai.stream = Some((rx, stop));
    }

    fn ai_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(RichText::new("AI helper").size(28.0).strong());
        ui.label(RichText::new(
            "Ask anything about setting up NightCoop, errors, or playing together. It runs on an AI you host yourself \
             (Ollama or LM Studio): free, private, and it can see this helper's status checks to pinpoint problems.",
        )
        .color(DIM));
        ui.add_space(6.0);
        if !self.ai.probed {
            self.ai.probed = true;
            self.ai_probe(&ctx, false);
        }
        // NCH_AI_ASK="question" sends a question as soon as the AI answers (for testing).
        if let Ok(q) = std::env::var("NCH_AI_ASK") {
            if !self.ai.test_asked && matches!(self.ai.connection, Some(Ok(_))) && !q.is_empty() {
                self.ai.test_asked = true;
                self.ai.input = q;
                self.ai_send(&ctx);
            }
        }
        let connected = matches!(self.ai.connection, Some(Ok(_)));

        egui::CollapsingHeader::new(RichText::new("SET UP YOUR AI").color(ACCENT).strong())
            .id_salt("ai-setup")
            .default_open(!self.settings_has_ai())
            .show(ui, |ui| {
                ui.label(RichText::new("On a Windows or Linux PC").strong());
                ui.label("1. Install Ollama (ollama.com) or LM Studio (lmstudio.ai).");
                ui.horizontal_wrapped(|ui| {
                    ui.label("2. Download a model. Ollama, in a terminal:");
                    ui.label(RichText::new("ollama pull llama3.1:8b").monospace().color(CYAN));
                    if ui.small_button("📋").clicked() {
                        ui.ctx().copy_text("ollama pull llama3.1:8b".into());
                    }
                });
                ui.label(RichText::new("qwen2.5:7b works well too; on a weak laptop try llama3.2:3b. In LM Studio: download a model, then Developer → Start server.").color(DIM));
                ui.label("3. Press “Find my AI”.");
                ui.label(RichText::new("On a Steam Deck").strong());
                ui.label(RichText::new(
                    "Easiest: use the AI on your PC. On the PC, let other devices in — LM Studio: Developer → “Serve on local network”; \
                     Ollama: set the environment variable OLLAMA_HOST=0.0.0.0 and restart it. Then enter the PC's address here, \
                     like http://192.168.1.20:11434/v1 (LM Studio uses port 1234). The Deck can also run LM Studio's AppImage itself with a small (3B) model.",
                )
                .color(DIM));
                ui.add_space(4.0);
                let mut changed = false;
                ui.horizontal_wrapped(|ui| {
                    ui.label("Address:");
                    changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.ai_url).desired_width(300.0).hint_text(ai::DEFAULT_URL)).lost_focus();
                    let probing = self.ai.probe.is_some();
                    if ui.add_enabled(!probing, egui::Button::new("🔎  Find my AI")).on_hover_text("Tries this address, then Ollama, LM Studio, llama.cpp and Jan on this computer").clicked() {
                        self.settings.save();
                        self.ai_probe(&ctx, true);
                    }
                    if ui.add_enabled(!probing, egui::Button::new("Test")).clicked() {
                        self.settings.save();
                        self.ai_probe(&ctx, false);
                    }
                    if probing {
                        ui.spinner();
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Model:");
                    let shown = if self.settings.ai_model.is_empty() { "automatic (first listed)".to_string() } else { self.settings.ai_model.clone() };
                    egui::ComboBox::from_id_salt("ai-model").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                        changed |= ui.selectable_value(&mut self.settings.ai_model, String::new(), "automatic (first listed)").changed();
                        for m in self.ai.models.clone() {
                            changed |= ui.selectable_value(&mut self.settings.ai_model, m.clone(), m).changed();
                        }
                    });
                    ui.label("API key:");
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut self.settings.ai_key).password(true).desired_width(140.0).hint_text("not needed"))
                        .on_hover_text("Only for servers that ask for one. Ollama and LM Studio don't.")
                        .lost_focus();
                });
                changed |= ui
                    .checkbox(&mut self.settings.ai_share_status, "Let the AI see my status checks (versions, Proton, launch option, log summary)")
                    .on_hover_text("The same text as Troubleshoot → Diagnostics; account names and your home folder are hidden")
                    .changed();
                if changed {
                    self.settings.save();
                }
                let url = self.settings.ai_url.trim();
                if !url.is_empty() && !url.contains("localhost") && !url.contains("127.0.0.1") {
                    ui.label(RichText::new("Questions (and your status checks, if allowed) go to this address — make sure it's a computer you trust.").color(DIM).small());
                }
            });
        match &self.ai.connection {
            Some(Ok(t)) => {
                ui.label(RichText::new(format!("✔ {t}")).color(OK));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("✖ {e}")).color(WARN));
            }
            None => {}
        }
        ui.add_space(4.0);

        card(ui, |ui| {
            let streaming = self.ai.stream.is_some();
            if self.ai.messages.is_empty() {
                ui.label(RichText::new("WHAT YOUR AI CAN DO").color(ACCENT).strong());
                ui.label(RichText::new(
                    "It knows how NightCoop and this helper work, and it can use the helper for you: check your setup and logs, \
                     install or update everything, switch builds to match your partner, turn mods off/on, back up saves, fix the \
                     Steam Deck's Visual C++ runtime and launch option, test your connection, and build, start and update your own server. \
                     Anything that changes something asks you first (unless you allow it below); removing mods or restoring saves always asks.",
                )
                .color(DIM));
                ui.label(RichText::new(if connected { "Try one of these, or type your own question below:" } else { "Set up your AI above, then try one of these or type your own question:" }).color(DIM));
                ui.horizontal_wrapped(|ui| {
                    for q in [
                        "There's no Co-op menu in the game. What's wrong?",
                        "Explain my status checks.",
                        "How do my partner and I join each other?",
                        "RED4ext error 998 on my Steam Deck",
                        "Should I host my own server?",
                    ] {
                        if ui.add_enabled(!streaming, egui::Button::new(q)).clicked() {
                            self.ai.input = q.into();
                            self.ai_send(&ctx);
                        }
                    }
                });
            }
            let count = self.ai.messages.len();
            for (i, m) in self.ai.messages.iter().enumerate() {
                ui.add_space(4.0);
                if m.role == "tool" {
                    ui.label(RichText::new(format!("🔧 {}", m.content)).color(DIM).small());
                } else if m.role == "user" {
                    ui.label(RichText::new("You").color(CYAN).strong());
                    ui.label(&m.content);
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("AI").color(ACCENT).strong());
                        if streaming && i + 1 == count {
                            ui.spinner();
                        } else if ui.small_button("📋 Copy").clicked() {
                            ui.ctx().copy_text(m.content.clone());
                        }
                    });
                    markdown(ui, &m.content);
                }
            }
            let mut decide: Option<(usize, bool)> = None;
            for (i, a) in self.ai.actions.iter().enumerate() {
                match &a.state {
                    ActState::Ask => {
                        egui::Frame::new().stroke(egui::Stroke::new(1.0, ACCENT)).corner_radius(6).inner_margin(8).show(ui, |ui| {
                            ui.label(RichText::new("The AI wants to:").color(DIM).small());
                            ui.label(RichText::new(&a.label).strong());
                            ui.horizontal(|ui| {
                                if ui.add(egui::Button::new(RichText::new("✔ Allow").strong().color(Color32::BLACK)).fill(ACCENT)).clicked() {
                                    decide = Some((i, true));
                                }
                                if ui.button("✖ Decline").clicked() {
                                    decide = Some((i, false));
                                }
                            });
                        });
                    }
                    ActState::Ready | ActState::Running => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new(&a.label).color(DIM));
                        });
                    }
                    _ => {}
                }
            }
            if let Some((i, yes)) = decide {
                self.ai.actions[i].state = if yes { ActState::Ready } else { ActState::Declined };
            }
            if let Some(n) = &self.ai.notice {
                ui.label(RichText::new(format!("ℹ {n}")).color(CYAN).small());
            }
            if let Some(e) = &self.ai.error {
                ui.label(RichText::new(format!("⚠ {e}")).color(BAD));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let input = ui.add(
                    egui::TextEdit::singleline(&mut self.ai.input)
                        .desired_width((ui.available_width() - 190.0).max(200.0))
                        .hint_text("Ask a question and press Enter"),
                );
                let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if streaming {
                    if ui.button("■  Stop").clicked() {
                        if let Some((_, stop)) = &self.ai.stream {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                } else {
                    let has_text = !self.ai.input.trim().is_empty();
                    let send = ui.add_enabled(has_text, egui::Button::new(RichText::new("Send").strong().color(Color32::BLACK)).fill(ACCENT)).clicked();
                    if (send || enter) && has_text {
                        self.ai_send(&ctx);
                        input.request_focus();
                    }
                    if !self.ai.messages.is_empty() && ui.button("Clear").clicked() {
                        self.ai.messages.clear();
                        self.ai.error = None;
                        self.ai.notice = None;
                    }
                }
            });
            if ui
                .checkbox(&mut self.settings.ai_auto_approve, "Let the AI act without asking (uninstalling and restoring saves still ask)")
                .changed()
            {
                self.settings.save();
            }
            ui.label(RichText::new("AI answers can be wrong. Every action it takes shows up here and in the activity log on Home.").color(DIM).small());
        });
    }

    /// The player changed the AI address or picked a model at some point.
    fn settings_has_ai(&self) -> bool {
        self.settings.ai_url.trim() != ai::DEFAULT_URL || !self.settings.ai_model.is_empty()
    }

    // ---------- AI agent: the helper's actions as tools ----------

    /// Runs the AI's approved tool calls one at a time and hands the results back.
    fn ai_drive(&mut self, ctx: &egui::Context) {
        if self.ai.actions.is_empty() {
            return;
        }
        for i in 0..self.ai.actions.len() {
            match self.ai.actions[i].state {
                ActState::Done(_) | ActState::Declined => continue,
                ActState::Ask | ActState::Running => return,
                ActState::Ready => {
                    if !self.busy() {
                        self.ai_run(ctx, i);
                    }
                    return;
                }
            }
        }
        let mut results = Vec::new();
        for a in self.ai.actions.drain(..) {
            let (mark, text) = match a.state {
                ActState::Done(r) => (if r.starts_with("FAILED") { "⚠" } else { "✔" }, r),
                _ => ("✖", "The player declined this. Don't retry it; ask what they'd like instead.".to_string()),
            };
            self.ai.messages.push(ai::Message { role: "tool", content: format!("{mark}  {}", a.label) });
            results.push((a.call.id, ai::shorten(&text, 3000)));
        }
        if let Some(tx) = &self.ai.results {
            let _ = tx.send(results);
        }
    }

    fn ai_run(&mut self, ctx: &egui::Context, i: usize) {
        let call = self.ai.actions[i].call.clone();
        let log_start = self.log.len();
        self.banner = None;
        let instant = self.ai_tool(ctx, &call);
        let state = match instant {
            Some(r) => ActState::Done(r),
            None if self.busy() => {
                self.ai.job = Some((i, log_start));
                ActState::Running
            }
            None => ActState::Done(match &self.banner {
                Some((ok, msg)) => format!("{}{msg}", if *ok { "" } else { "FAILED: " }),
                None => "Nothing happened — check get_status (is the game found and installed?).".into(),
            }),
        };
        self.ai.actions[i].state = state;
    }

    /// A finished job that the AI started: its log and outcome become the tool's result.
    fn ai_job_finished(&mut self) {
        let Some((i, start)) = self.ai.job.take() else { return };
        let lines = &self.log[start.min(self.log.len())..];
        let failed = lines.iter().any(|l| l.starts_with("ERROR"));
        let name = self.ai.actions.get(i).map(|a| a.call.name.clone()).unwrap_or_default();
        let mut r = format!("{}\n{}", if failed { "FAILED" } else { "Finished" }, lines.join("\n"));
        r.push_str(&self.ai_followup(&name));
        if let Some(a) = self.ai.actions.get_mut(i) {
            a.state = ActState::Done(r);
        }
    }

    /// Fresh facts after a job, so the AI sees what changed.
    fn ai_followup(&self, name: &str) -> String {
        match name {
            "check_for_updates" => format!("\n{}", self.update_summary()),
            "check_connection" => self.connection.as_ref().map(|r| format!("\nResult: {} (server found: {}, UDP works: {})", r.summary(), r.hub_resolves, r.udp_works)).unwrap_or_default(),
            "check_router" | "open_router_ports" => self.server.router.as_ref().map(router_summary).unwrap_or_default(),
            n if n.contains("server") => self.server.hub.as_ref().map(hub_summary).unwrap_or_default(),
            _ => format!("\nNow: {} · mods {} · ready to play: {}", self.share_text(), if self.status.mods_off { "OFF" } else { "on" }, self.status.ready()),
        }
    }

    /// Carries out one tool call. `Some` is an immediate result; `None` means a job was started
    /// (or nothing happened, which the banner explains).
    fn ai_tool(&mut self, ctx: &egui::Context, call: &ai::ToolCall) -> Option<String> {
        let game = self.status.game.clone();
        let no_game = || Some("FAILED: Cyberpunk 2077 wasn't found. The player can set the game folder in Settings.".to_string());
        match call.name.as_str() {
            "get_status" => Some(format!(
                "{}\n{}\nMods off: {} · Game running: {} · Steam running: {} · Gaming Mode: {} · Ready to play: {}",
                self.status.diagnostics(&self.state),
                self.update_summary(),
                self.status.mods_off,
                self.status.game_running,
                self.status.steam_running,
                steam::in_gaming_mode(),
                self.status.ready()
            )),
            "read_logs" => {
                let mut out = String::from("RED4ext log:\n");
                out.push_str(&self.status.log_lines.iter().rev().take(40).rev().cloned().collect::<Vec<_>>().join("\n"));
                if let Some(g) = &game {
                    let newest = std::fs::read_dir(g.active_plugin_dir().join("logs"))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter(|e| e.path().is_file())
                        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
                    if let Some(f) = newest {
                        out.push_str(&format!("\n\nNightCoop log {}:\n", f.file_name().to_string_lossy()));
                        out.push_str(&hub::log_tail(&f.path(), 40).join("\n"));
                    }
                }
                Some(game::redact_home(&out))
            }
            "check_for_updates" => {
                self.check_updates(ctx);
                None
            }
            "list_nightcoop_builds" => Some(match &self.remote.nightcoop {
                Some(Ok(m)) => {
                    let mut list: Vec<String> = m
                        .releases
                        .iter()
                        .map(|r| {
                            format!(
                                "{} ({}{}) protocol {} · game {} · {}",
                                r.version,
                                if r.is_stable() { "stable" } else { "beta" },
                                if r.is_yanked() { ", withdrawn" } else { "" },
                                r.protocol_str().unwrap_or_else(|| "?".into()),
                                r.game.join("/"),
                                r.date.clone().unwrap_or_default()
                            )
                        })
                        .collect();
                    list.push(format!("Installed: {}", self.status.nightcoop_version.clone().unwrap_or_else(|| "none".into())));
                    list.join("\n")
                }
                Some(Err(e)) => format!("FAILED: couldn't load the list: {e}"),
                None => "Not loaded yet: call check_for_updates first.".into(),
            }),
            "list_save_backups" => Some(if self.backups.is_empty() {
                "No backups yet.".into()
            } else {
                self.backups.iter().map(|(p, size)| format!("{} ({})", p.file_stem().unwrap_or_default().to_string_lossy(), util::human_size(*size))).collect::<Vec<_>>().join("\n")
            }),
            "check_connection" => {
                self.spawn(ctx, "Checking the connection", |rep, _, _| {
                    rep.progress(None, "Checking the connection…");
                    Ok(Extra::Connection(net::connection_check()))
                });
                None
            }
            "compare_with_partner" => {
                let text = call.arg("text");
                let rows = compare_versions(&self.share_text(), &text);
                let mut out: Vec<String> = rows
                    .iter()
                    .map(|(k, mine, theirs, same)| format!("{k}: mine {} · theirs {} · {}", if mine.is_empty() { "?" } else { mine }, theirs.as_deref().unwrap_or("?"), if theirs.is_none() { "unknown" } else if *same { "same" } else { "DIFFERENT" }))
                    .collect();
                if let Some(inv) = invite::find(&text) {
                    out.push(format!("Invite found: {} (code {})", inv.line(), inv.code));
                }
                Some(out.join("\n"))
            }
            "open_page" => {
                let want = call.arg("page").to_ascii_lowercase();
                let page = Page::ALL.into_iter().find(|p| {
                    let l = p.label().to_ascii_lowercase();
                    !want.is_empty() && (l.contains(&want) || (want == "server" && *p == Page::Server) || (want == "versions" && *p == Page::Versions))
                });
                Some(match page {
                    Some(p) => {
                        self.page = p;
                        format!("Showing {}.", p.label().trim_start_matches(|c: char| !c.is_ascii_alphabetic()))
                    }
                    None => "Unknown page.".into(),
                })
            }
            "get_server_status" => {
                self.server_action(ctx, "check");
                None
            }
            "check_router" | "open_router_ports" => {
                let open = (call.name == "open_router_ports").then(|| call.flag("open"));
                self.spawn(ctx, "Checking your network", move |rep, _, _| {
                    rep.progress(None, "Asking your router…");
                    Ok(Extra::Router(router_check(open)))
                });
                None
            }
            "install_or_update_everything" => {
                if game.is_none() {
                    return no_game();
                }
                self.start_setup(ctx, call.flag("restart_steam") && !steam::in_gaming_mode());
                None
            }
            "install_nightcoop_version" => {
                let v = call.arg("version").trim_start_matches(['v', 'V']).to_string();
                match &self.remote.nightcoop {
                    Some(Ok(m)) if m.find(&v).is_some() => {
                        self.run_confirm(ctx, Confirm::InstallNcVersion(v), 1);
                        None
                    }
                    Some(Ok(_)) => Some(format!("FAILED: there is no build {v}. Call list_nightcoop_builds.")),
                    _ => Some("FAILED: the build list isn't loaded: call check_for_updates first.".into()),
                }
            }
            "mods_off" | "mods_on" => {
                let Some(g) = game else { return no_game() };
                if call.name == "mods_on" {
                    if !self.status.mods_off {
                        return Some("Mods are already on.".into());
                    }
                    self.spawn(ctx, "Turning mods on", move |rep, _, _| ops::mods_on(&g, rep).map(|_| Extra::None));
                } else {
                    if self.status.mods_off {
                        return Some("Mods are already off.".into());
                    }
                    let extras = self.state.nightcoop_extra_files.clone();
                    self.spawn(ctx, "Turning mods off", move |rep, _, _| ops::mods_off(&g, &extras, rep).map(|_| Extra::None));
                }
                None
            }
            "backup_saves" => {
                let Some(g) = game else { return no_game() };
                self.spawn(ctx, "Backing up saves", move |rep, state, settings| ops::backup_saves(&g, settings, state, rep, "", false).map(|_| Extra::None));
                None
            }
            "install_vc_runtime" => {
                let Some(g) = game else { return no_game() };
                if !g.uses_proton() {
                    return Some("Not needed: the game doesn't run through Proton here.".into());
                }
                self.spawn(ctx, "Installing the Visual C++ runtime", move |rep, _, _| ops::install_vc_runtime(&g, rep).map(|_| Extra::None));
                None
            }
            "set_steam_launch_option" => {
                let Some(g) = game else { return no_game() };
                if !g.uses_proton() {
                    return Some("Not needed: launch options are only for Proton (Steam Deck/Linux).".into());
                }
                if steam::in_gaming_mode() {
                    return Some("FAILED: Steam can't be restarted in Gaming Mode. The player can switch to Desktop Mode, or paste the line in Cyberpunk → Properties → Launch Options.".into());
                }
                self.run_confirm(ctx, Confirm::LaunchOption { enable: call.flag("enable") }, 1);
                None
            }
            "use_bundled_red4ext" => {
                let Some(g) = game else { return no_game() };
                self.use_bundled_red4ext(ctx, &g);
                None
            }
            "set_update_channel" => {
                let beta = call.arg("channel").eq_ignore_ascii_case("beta");
                self.settings.channel = if beta { Channel::Beta } else { Channel::Stable };
                self.settings.save();
                Some(format!("Update channel: {}.", if beta { "beta" } else { "stable" }))
            }
            "launch_game" => {
                if self.status.game_running {
                    return Some("The game is already running.".into());
                }
                if game.as_ref().map(|g| g.steam.is_none()).unwrap_or(true) {
                    return Some("FAILED: only the Steam version can be started from here.".into());
                }
                steam::launch_game();
                Some("Cyberpunk 2077 is starting through Steam.".into())
            }
            "add_helper_to_steam" => {
                if steam::in_gaming_mode() {
                    return Some("FAILED: needs Desktop Mode (Steam restarts).".into());
                }
                self.run_confirm(ctx, Confirm::AddToSteam, 1);
                None
            }
            "update_helper" => match self.remote.helper.clone() {
                Some(Ok(Some(u))) => {
                    self.spawn(ctx, "Updating helper", move |rep, _, _| {
                        selfupdate::apply(&u, rep)?;
                        Ok(Extra::HelperUpdated)
                    });
                    None
                }
                Some(Ok(None)) => Some("The helper is already up to date.".into()),
                _ => Some("Unknown: call check_for_updates first.".into()),
            },
            "build_server" => {
                self.server_action(ctx, "update");
                None
            }
            "start_server" | "stop_server" => {
                self.server_action(ctx, if call.name == "start_server" { "start" } else { "stop" });
                None
            }
            "open_server_firewall" => {
                if !cfg!(windows) {
                    return Some("Only Windows needs this; Linux desktops usually have no firewall (ufw users: allow the UDP ports).".into());
                }
                self.server_action(ctx, "firewall");
                None
            }
            "set_server_autostart" => {
                self.server_action(ctx, if call.flag("on") { "autostart_on" } else { "autostart_off" });
                None
            }
            "use_server_in_game" => {
                let Some(addr) = hub::parse_address(&call.arg("address")) else {
                    return Some("FAILED: that isn't address#FINGERPRINT.".into());
                };
                let Some(g) = game else { return no_game() };
                self.server.address = addr.clone();
                Some(match hub::use_in_game(&g, &addr) {
                    Ok(()) => format!("The game will use {addr} the next time it starts."),
                    Err(e) => format!("FAILED: {e:#}"),
                })
            }
            "uninstall_nightcoop" => {
                self.run_confirm(ctx, Confirm::UninstallNightCoop, 1);
                None
            }
            "remove_everything" => {
                self.run_confirm(ctx, Confirm::RemoveAll, 1);
                None
            }
            "restore_save_backup" => {
                let name = call.arg("name");
                match self.backups.iter().find(|(p, _)| !name.is_empty() && p.file_name().unwrap_or_default().to_string_lossy().contains(name.as_str())) {
                    Some((p, _)) => {
                        let p = p.clone();
                        self.run_confirm(ctx, Confirm::RestoreSaves(p), 1);
                        None
                    }
                    None => Some("FAILED: no backup by that name. Call list_save_backups.".into()),
                }
            }
            other => Some(format!("FAILED: there is no tool called {other}.")),
        }
    }

    /// What's available online compared with what's installed.
    fn update_summary(&self) -> String {
        let mut out = Vec::new();
        match (&self.remote.nightcoop, self.nc_latest()) {
            (None, _) => out.push("Updates: not checked yet (call check_for_updates).".to_string()),
            (Some(Err(e)), _) => out.push(format!("NightCoop list: couldn't load ({e})")),
            (Some(Ok(_)), Some(l)) => out.push(format!(
                "NightCoop latest ({:?} channel): {}{}",
                self.settings.channel,
                l.version,
                if self.nc_update_available() { " — UPDATE AVAILABLE" } else { "" }
            )),
            (Some(Ok(_)), None) => out.push("NightCoop: no build listed".into()),
        }
        if let Some(l) = self.r4_latest() {
            out.push(format!("RED4ext on GitHub: {} (the bundled one is recommended)", l.version()));
        }
        match &self.remote.helper {
            Some(Ok(Some(u))) => out.push(format!("Helper update available: {}", u.version)),
            Some(Ok(None)) => out.push("Helper: up to date".into()),
            _ => {}
        }
        if let Some(Ok(c)) = &self.remote.hub {
            out.push(if hub::update_available(c) { "Own server (hub): UPDATE AVAILABLE".into() } else { "Own server (hub): up to date".into() });
        }
        out.join("\n")
    }

    /// The approval card text: what the AI wants to do, in the player's words.
    fn ai_label(&self, call: &ai::ToolCall) -> String {
        match call.name.as_str() {
            "get_status" => "Looked at your setup".into(),
            "read_logs" => "Read the RED4ext and NightCoop logs".into(),
            "check_for_updates" => "Checked for updates".into(),
            "list_nightcoop_builds" => "Looked at the NightCoop builds".into(),
            "list_save_backups" => "Looked at your save backups".into(),
            "check_connection" => "Tested your connection".into(),
            "compare_with_partner" => "Compared your versions with your partner's".into(),
            "open_page" => format!("Opened the {} page", call.arg("page")),
            "get_server_status" => "Checked your server".into(),
            "check_router" => "Asked your router".into(),
            "install_or_update_everything" => "Install / update everything (NightCoop, RED4ext, Visual C++ runtime, launch option)".into(),
            "install_nightcoop_version" => format!("Install NightCoop {}", call.arg("version")),
            "mods_off" => "Turn mods off".into(),
            "mods_on" => "Turn mods back on".into(),
            "backup_saves" => "Back up your saves".into(),
            "install_vc_runtime" => "Install the Visual C++ runtime into the game's Proton files".into(),
            "set_steam_launch_option" => format!("{} the Steam launch option (Steam closes and restarts)", if call.flag("enable") { "Set" } else { "Remove" }),
            "use_bundled_red4ext" => "Switch to the RED4ext bundled with NightCoop".into(),
            "set_update_channel" => format!("Switch the update channel to {}", call.arg("channel")),
            "launch_game" => "Start Cyberpunk 2077".into(),
            "add_helper_to_steam" => "Add this helper to your Steam library (Steam restarts)".into(),
            "update_helper" => "Update this helper app".into(),
            "build_server" => "Build / update your own server on this computer".into(),
            "start_server" => "Start your server".into(),
            "stop_server" => "Stop your server".into(),
            "open_server_firewall" => "Open the server's ports in Windows Firewall (admin prompt)".into(),
            "open_router_ports" => format!("{} the server's ports on your router", if call.flag("open") { "Forward" } else { "Close" }),
            "set_server_autostart" => format!("{} starting the server with this computer", if call.flag("on") { "Turn on" } else { "Turn off" }),
            "use_server_in_game" => format!("Use the server {} in your game", call.arg("address")),
            "uninstall_nightcoop" => "Uninstall NightCoop (saves stay)".into(),
            "restore_save_backup" => format!("Replace your saves with the backup {} (current saves are backed up first)", call.arg("name")),
            "remove_everything" => "Remove everything: NightCoop, RED4ext and the launch option".into(),
            other => ai::TOOLS.iter().find(|t| t.0 == other).map(|t| t.1.to_string()).unwrap_or_else(|| other.replace('_', " ")),
        }
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
        if c != 0 {
            self.run_confirm(ctx, which, c);
        }
    }

    /// Carries out a confirmed action (1 = yes, 2 = the alternative button).
    fn run_confirm(&mut self, ctx: &egui::Context, which: Confirm, c: u8) {
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
    hub: Option<HubStatus>,
    /// The hub status was looked up once for this page.
    checked: bool,
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
struct HubStatus {
    built: bool,
    running: bool,
    /// Windows Firewall rule present (None elsewhere).
    firewall: Option<bool>,
    autostart: bool,
    build_info: Option<String>,
    update_available: bool,
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

fn hub_status() -> HubStatus {
    let built = hub::built();
    let running = built && hub::running();
    #[cfg(windows)]
    let firewall = Some(hub::firewall::ok());
    #[cfg(not(windows))]
    let firewall = None;
    HubStatus {
        built,
        running,
        firewall,
        autostart: hub::autostart(),
        build_info: hub::build_info(),
        update_available: built && hub::latest_commit().map(|c| hub::update_available(&c)).unwrap_or(false),
        fingerprint: hub::fingerprint(),
        public_ip: if running { net::public_ip().map(|ip| ip.to_string()) } else { None },
    }
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

#[derive(Default)]
struct AiUi {
    messages: Vec<ai::Message>,
    input: String,
    /// The answer being streamed, and the switch that stops it.
    stream: Option<(Receiver<ai::Event>, Arc<AtomicBool>)>,
    /// A running connection check: Ok((address, models)).
    probe: Option<Receiver<Result<(String, Vec<String>), String>>>,
    probed: bool,
    test_asked: bool,
    connection: Option<Result<String, String>>,
    models: Vec<String>,
    error: Option<String>,
    notice: Option<String>,
    /// Where tool results go back to the AI.
    results: Option<mpsc::Sender<Vec<(String, String)>>>,
    /// The AI's tool calls of the current step.
    actions: Vec<AiAction>,
    /// (action index, first log line) of the job an action started.
    job: Option<(usize, usize)>,
}

struct AiAction {
    call: ai::ToolCall,
    label: String,
    state: ActState,
}

enum ActState {
    /// Waiting for the player's OK.
    Ask,
    Ready,
    Running,
    Done(String),
    Declined,
}

fn router_summary(r: &RouterCheck) -> String {
    let router = match &r.router {
        Ok((name, local, ext)) => format!("Router: {name}, this computer {local}, router's public address {}", ext.map(|e| e.to_string()).unwrap_or_else(|| "?".into())),
        Err(e) => format!("Router: not reachable by UPnP ({e})"),
    };
    format!(
        "\n{router}\nPublic address (STUN): {}\nCGNAT (home hosting impossible): {}\nPorts forwarded now: {}",
        r.public_ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
        r.cgnat(),
        r.ports_opened
    )
}

fn hub_summary(h: &HubStatus) -> String {
    format!(
        "\nServer: built {} ({}) · running {} · firewall {} · starts with computer {} · update available {} · fingerprint {} · address {}",
        h.built,
        h.build_info.as_deref().unwrap_or("-"),
        h.running,
        h.firewall.map(|f| f.to_string()).unwrap_or_else(|| "n/a".into()),
        h.autostart,
        h.update_available,
        h.fingerprint.as_deref().unwrap_or("-"),
        match (&h.public_ip, &h.fingerprint) {
            (Some(ip), Some(fp)) => format!("{ip}#{fp}"),
            _ => "-".into(),
        }
    )
}

/// Just enough Markdown for chat answers: code blocks (with Copy), headings, bullets, `code` and **bold**.
fn markdown(ui: &mut egui::Ui, text: &str) {
    fn code_block(ui: &mut egui::Ui, code: &mut String) {
        let block = code.trim_end().to_string();
        code.clear();
        if block.is_empty() {
            return;
        }
        egui::Frame::new().fill(Color32::from_rgb(16, 16, 24)).corner_radius(6).inner_margin(8).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&block).monospace().color(CYAN));
                if ui.small_button("📋").on_hover_text("Copy").clicked() {
                    ui.ctx().copy_text(block.clone());
                }
            });
        });
    }
    let mut in_code = false;
    let mut code = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                code_block(ui, &mut code);
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let t = line.trim_end();
        if t.trim().is_empty() {
            ui.add_space(2.0);
            continue;
        }
        let s = t.trim_start();
        let (body, heading) = match s.strip_prefix('#') {
            Some(h) => (h.trim_start_matches('#').trim().to_string(), true),
            None => {
                let indent = " ".repeat((t.len() - s.len()).min(6));
                match s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")) {
                    Some(rest) => (format!("{indent}•  {rest}"), false),
                    None => (t.to_string(), false),
                }
            }
        };
        let mut job = egui::text::LayoutJob::default();
        for (i, seg) in body.split('`').enumerate() {
            let rt = if i % 2 == 1 {
                RichText::new(seg).monospace().color(CYAN)
            } else {
                let rt = RichText::new(seg.replace("**", ""));
                if heading { rt.strong().color(ACCENT) } else { rt }
            };
            rt.append_to(&mut job, ui.style(), egui::FontSelection::Default, egui::Align::Center);
        }
        ui.label(job);
    }
    if in_code {
        code_block(ui, &mut code);
    }
}

fn fetch_remote(settings: &Settings) -> RemoteInfo {
    let repo = settings.helper_repo.trim();
    let (red4ext, nightcoop, helper, hub) = std::thread::scope(|sc| {
        let r4 = sc.spawn(|| remote::github_releases(remote::RED4EXT_REPO).map_err(|e| format!("{e:#}")));
        let nc = sc.spawn(|| remote::nightcoop_manifest(&settings.manifest_url).map_err(|e| format!("{e:#}")));
        let hp = sc.spawn(|| (!repo.is_empty()).then(|| selfupdate::check(repo).map_err(|e| format!("{e:#}"))));
        let hb = sc.spawn(|| hub::built().then(|| hub::latest_commit().map_err(|e| format!("{e:#}"))));
        let failed = "the check stopped unexpectedly".to_string();
        (
            Some(r4.join().unwrap_or_else(|_| Err(failed.clone()))),
            Some(nc.join().unwrap_or_else(|_| Err(failed))),
            hp.join().unwrap_or(None),
            hb.join().unwrap_or(None),
        )
    });
    RemoteInfo { red4ext, nightcoop, helper, hub, checked_at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()) }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        self.poll_ai();
        self.ai_drive(&ctx);
        if self.hub_update_due && !self.busy() {
            self.hub_update_due = false;
            self.log.push("A newer NightCoop hub is out; updating your server.".into());
            self.server_action(&ctx, "update");
        }
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
                    Page::Ai => self.ai_page(ui),
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

    /// The whole agent loop inside the real app: the model calls tools, the app runs them (changes are
    /// declined here), results go back. Needs a model: NCH_AI_URL=http://localhost:11434/v1
    #[test]
    #[ignore]
    fn live_agent_in_app() {
        let ctx = egui::Context::default();
        let mut app = App::with_ctx(&ctx);
        app.settings.ai_url = std::env::var("NCH_AI_URL").unwrap_or_else(|_| ai::DEFAULT_URL.into());
        app.settings.ai_auto_approve = false;
        app.ai.input = "Check for updates, then tell me whether a newer NightCoop build is out than the one I have.".into();
        app.ai_send(&ctx);
        let start = Instant::now();
        while (app.ai.stream.is_some() || app.busy()) && start.elapsed() < Duration::from_secs(400) {
            app.poll_job(&ctx);
            app.poll_ai();
            for a in &mut app.ai.actions {
                if matches!(a.state, ActState::Ask) {
                    println!("declining: {}", a.label);
                    a.state = ActState::Declined;
                }
            }
            app.ai_drive(&ctx);
            std::thread::sleep(Duration::from_millis(50));
        }
        for m in &app.ai.messages {
            println!("[{}] {}", m.role, m.content);
        }
        assert!(app.ai.error.is_none(), "{:?}", app.ai.error);
        assert!(app.ai.messages.iter().any(|m| m.role == "tool"), "the AI used a tool");
        assert!(app.ai.messages.last().is_some_and(|m| m.role == "assistant"), "and answered after it");
    }
}
