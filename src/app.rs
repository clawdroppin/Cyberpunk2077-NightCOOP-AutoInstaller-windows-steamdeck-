//! The GUI.

use crate::config::{self, Channel, InstallState, Settings};
use crate::game::{self, Status};
use crate::ops::{self, Event, Reporter};
use crate::remote::{self, GhRelease, NcManifest};
use crate::selfupdate::{self, HelperUpdate};
use crate::{shortcuts, steam, util};
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
    Servers,
    Troubleshoot,
    Guide,
    Settings,
}

impl Page {
    const ALL: [Page; 7] = [Page::Home, Page::Versions, Page::Saves, Page::Servers, Page::Troubleshoot, Page::Guide, Page::Settings];
    fn label(self) -> &'static str {
        match self {
            Page::Home => "🏠  Home",
            Page::Versions => "📦  Mods & Versions",
            Page::Saves => "💾  Saves",
            Page::Servers => "🌐  Servers",
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
        while let Ok(Event::Log(l)) = job.rx.try_recv() {
            self.log.push(l);
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
                            Extra::None => {}
                        }
                        if job.title != "Checking for updates" {
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
        self.servers_text = None;
        self.servers_dirty = false;
    }

    fn check_updates(&mut self, ctx: &egui::Context) {
        self.spawn(ctx, "Checking for updates", |rep, _state, settings| {
            rep.progress(None, "Checking RED4ext, NightCoop and helper versions…");
            Ok(Extra::Remote(fetch_remote(settings)))
        });
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

    fn r4_update_available(&self) -> bool {
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

    fn share_text(&self) -> String {
        let s = &self.status;
        format!(
            "NightCoop {} · RED4ext {} · Game {} · Phantom Liberty: {}",
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
        let r4_installed = self.status.red4ext_version.is_some() && self.status.loader_present;
        let nc_installed_version = self.status.nightcoop_version.clone().filter(|_| self.status.nightcoop_installed);
        let nc_disabled = self.status.nightcoop_disabled;
        self.spawn(ctx, "Install / update everything", move |rep, state, settings| {
            if steam::game_running() {
                return Err(anyhow!("Cyberpunk 2077 is running. Quit the game first."));
            }
            rep.log("Checking for the latest versions…");
            let info = fetch_remote(settings);

            if settings.backup_saves_before_install {
                ops::backup_saves(&game, settings, state, rep, "before-install")?;
            }

            // RED4ext
            let r4 = match &info.red4ext {
                Some(Ok(list)) => list.iter().find(|r| !r.prerelease && remote::red4ext_asset(r).is_some()).cloned(),
                Some(Err(e)) => {
                    rep.log(format!("Could not check RED4ext releases: {e}"));
                    None
                }
                None => None,
            };
            match (&r4, r4_installed) {
                (Some(rel), false) => ops::install_red4ext_release(&game, rel, state, rep)?,
                (Some(rel), true) => {
                    let cur = util::pe_version(&game.red4ext_dir().join("RED4ext.dll")).or(state.red4ext_version.clone()).unwrap_or_default();
                    if util::cmp_versions(&rel.version(), &cur).is_gt() {
                        ops::install_red4ext_release(&game, rel, state, rep)?;
                    } else {
                        rep.log(format!("RED4ext {cur} is up to date."));
                    }
                }
                (None, false) => return Err(anyhow!("RED4ext is not installed and could not be downloaded")),
                (None, true) => {}
            }

            // NightCoop
            if nc_disabled {
                ops::set_nightcoop_enabled(&game, true, rep)?;
            }
            match &info.nightcoop {
                Some(Ok(m)) => match m.latest_for(settings.channel) {
                    Some(rel) => {
                        if nc_installed_version.as_deref().map(|v| util::same_version(v, &rel.version)).unwrap_or(false) {
                            rep.log(format!("NightCoop {} is up to date.", rel.version));
                        } else {
                            ops::install_nightcoop_release(&game, rel, &settings.manifest_url, state, rep)?;
                        }
                    }
                    None => rep.log("NightCoop has no downloadable build yet. Check again once it's released (or use “Install from file”)."),
                },
                Some(Err(e)) => rep.log(format!("Could not check NightCoop releases: {e}")),
                None => {}
            }

            // Proton launch option
            if needs_lo {
                if steam::steam_running() && !restart_steam {
                    rep.log("Skipped the Steam launch option (Steam is running). Set it on the Mods & Versions page, or paste it yourself.");
                } else {
                    ops::apply_launch_option(&game, true, restart_steam, rep)?;
                }
            }
            Ok(Extra::Remote(info))
        });
    }

    fn request_setup(&mut self, ctx: &egui::Context) {
        let needs_lo = self.status.needs_launch_option() && !self.status.launch_option_ok();
        if needs_lo && steam::steam_running() {
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
                    &format!(
                        "{} · {}{}",
                        g.store.label(),
                        s.game_version.clone().map(|v| format!("exe {v}")).unwrap_or_default(),
                        if s.phantom_liberty { " · Phantom Liberty" } else { "" }
                    ),
                ),
                None => check_row(ui, Some(false), "Cyberpunk 2077", "not found — set the game folder in Settings"),
            }
            let r4_detail = match (&s.red4ext_version, s.loader_present) {
                (Some(v), true) if self.r4_update_available() => format!("{v} → update to {} available", self.r4_latest().unwrap().version()),
                (Some(v), true) => format!("{v}"),
                (Some(_), false) => "loader (bin/x64/winmm.dll) missing — reinstall".into(),
                (None, _) => "not installed".into(),
            };
            let r4_ok = if s.red4ext_version.is_some() && s.loader_present { Some(true).filter(|_| !self.r4_update_available()).or(None) } else { Some(false) };
            check_row(ui, r4_ok, "RED4ext (mod loader)", &r4_detail);

            let nc_detail = if s.nightcoop_disabled {
                "disabled (solo mode) — enable it on Mods & Versions".to_string()
            } else {
                match (&s.nightcoop_version, s.nightcoop_installed) {
                    (Some(v), true) if self.nc_update_available() => format!("{v} → {} available", self.nc_latest().unwrap().version),
                    (Some(v), true) => v.clone(),
                    _ => match (&self.remote.nightcoop, self.nc_latest()) {
                        (Some(Ok(_)), None) => "not installed — no public build yet".into(),
                        _ => "not installed".into(),
                    },
                }
            };
            let nc_ok = if s.nightcoop_installed { if self.nc_update_available() { None } else { Some(true) } } else { Some(false) };
            check_row(ui, nc_ok, "NightCoop", &nc_detail);

            if s.needs_launch_option() {
                let ok = s.launch_option_ok();
                check_row(ui, Some(ok), "Proton DLL override", if ok { "set in Steam launch options" } else { "not set — RED4ext won't load under Proton" });
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
        let all_good = self.status.ready() && !self.nc_update_available() && !self.r4_update_available();
        ui.horizontal_wrapped(|ui| {
            let enabled = !self.busy() && self.status.game.is_some() && !self.status.game_running;
            let label = if all_good {
                "✔  Re-check & repair"
            } else if self.status.red4ext_version.is_some() || self.status.nightcoop_installed {
                "⬆  Update everything"
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
            if ui.button("📋  Copy my versions").on_hover_text("Send this to your co-op partner: everyone needs the same build").clicked() {
                ui.ctx().copy_text(self.share_text());
                self.banner = Some((true, "Versions copied — paste them to your partner to compare.".into()));
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
                    "NightCoop hasn't published a build yet (open beta: 23 October, 15:00 UTC). “Install everything” sets up {now} now; \
                     press it again once the build is out — or install a build you downloaded from Discord via Mods & Versions → Install from file."
                ),
            );
        }
        if self.status.ready() {
            ui.add_space(6.0);
            info_card(ui, "Ready! In the game's main menu open Co-op → Host a session, then send the invite to your partner. Guests use Co-op → Join by code.");
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
                if self.status.nightcoop_installed && ui.add_enabled(!self.busy(), egui::Button::new("⏸  Disable (play solo)")).clicked() {
                    let g = game.clone();
                    self.spawn(&ctx, "Disabling NightCoop", move |rep, _, _| ops::set_nightcoop_enabled(&g, false, rep).map(|_| Extra::None));
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
                        if let Some(sz) = r.mod_file().and_then(|f| f.size) {
                            meta.push(util::human_size(sz));
                        }
                        if !meta.is_empty() {
                            ui.label(RichText::new(meta.join("  ·  ")).color(DIM));
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
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("📂  Install from file…")).clicked() {
                    if let Some(file) = rfd::FileDialog::new().add_filter("RED4ext zip", &["zip"]).pick_file() {
                        let g = game.clone();
                        self.spawn(&ctx, "Installing RED4ext from file", move |rep, state, _| {
                            let ver = file.file_stem().map(|s| s.to_string_lossy().trim_start_matches("red4ext-").to_string()).unwrap_or_default();
                            ops::install_red4ext_archive(&g, &file, &ver, state, rep).map(|_| Extra::None)
                        });
                    }
                }
                if self.status.red4ext_version.is_some() && ui.add_enabled(!self.busy(), egui::Button::new("🗑  Uninstall")).clicked() {
                    self.confirm = Some(Confirm::UninstallRed4ext);
                }
            });
            ui.label(RichText::new("After a Cyberpunk patch, RED4ext usually needs an update before mods load again.").color(DIM).small());
        });

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
                    self.spawn(&ctx, "Backing up saves", move |rep, state, settings| ops::backup_saves(&g, settings, state, rep, "").map(|_| Extra::None));
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
                    if ui.add_enabled(!self.busy(), egui::Button::new("Delete")).clicked() {
                        self.confirm = Some(Confirm::DeleteBackup(path.clone()));
                    }
                });
            }
            ui.label(RichText::new(format!("Keeping the newest {} backups (change in Settings).", self.settings.keep_backups)).color(DIM).small());
        });
    }

    fn servers(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Servers").size(28.0).strong());
        ui.label(RichText::new(
            "By default the mod picks the nearest official server. A custom server is normally added in-game \
             (Co-op → Server → Add an own server) using an address with a key fingerprint, like hub.example.org#K3QF-9X2M-TV7A-0D4R. \
             The choice is stored in servers.txt next to NightCoop.dll — you can view or copy it here.",
        )
        .color(DIM));
        let Some(game) = self.status.game.clone() else {
            warn_card(ui, "Game not found.");
            return;
        };
        if !self.status.nightcoop_installed {
            info_card(ui, "Install NightCoop first; servers.txt lives in its folder.");
            return;
        }
        let file = game.servers_file();
        if self.servers_text.is_none() {
            self.servers_text = Some(std::fs::read_to_string(&file).unwrap_or_default());
            self.servers_dirty = false;
        }
        card(ui, |ui| {
            ui.label(RichText::new(file.display().to_string()).monospace().color(DIM).small());
            if !file.exists() {
                ui.label(RichText::new("No servers.txt yet — it appears after you pick a server in-game.").color(DIM));
            }
            let text = self.servers_text.as_mut().unwrap();
            if ui.add(egui::TextEdit::multiline(text).code_editor().desired_rows(10).desired_width(f32::INFINITY)).changed() {
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
        info_card(ui, "Joining a friend: the host presses “Copy the invite” in the session lobby (a line like ABC234@hub.example.org#K3QF-…). The guest pastes it into Co-op → Join by code.");
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
                if let Some(g) = &self.status.game {
                    if ui.button("Open game folder").clicked() {
                        util::open_path(&g.dir);
                    }
                    if ui.button("Open logs").clicked() {
                        util::open_path(g.red4ext_dir().join("logs"));
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
            let items: [(&str, &str); 7] = [
                ("No “Co-op” item in the main menu", "RED4ext didn't load the plugin. Check the diagnostics above: RED4ext and NightCoop must be installed, and on Steam Deck/Linux the Proton launch option must be set. The RED4ext log says why a plugin was skipped."),
                ("Worked before, broke after a game update", "Cyberpunk patches break RED4ext and plugins until they're updated. Press “Check for updates”, then “Update everything”. NightCoop lists the game versions each build supports."),
                ("Can't join each other", "Everyone must run the same NightCoop build (same protocol number). Use “Copy my versions” on Home and compare."),
                ("Game crashes on start (Deck)", "Try Proton Experimental or the latest Proton in Cyberpunk → Properties → Compatibility. As a last resort install vcrun2022 with Protontricks."),
                ("Stutter every few seconds", "Use Borderless window instead of exclusive fullscreen, and turn off the Discord overlay / frame generation."),
                ("“Server failed the key check”", "The server fingerprint doesn't match: a typo or the owner made a new key. Ask them for the current address#fingerprint."),
                ("Want to play solo again", "Mods & Versions → Disable (play solo). Enable it again any time."),
            ];
            for (q, a) in items {
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
            ("4. Host or join", "Main menu → Co-op. The host picks Host a session and copies the invite; the guest chooses Join by code and pastes it."),
            ("Typing the code", "Press STEAM + X for the on-screen keyboard. To paste an invite, the guest can also type the short session code."),
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
            let possible = self.status.game.as_ref().and_then(|g| g.steam.as_ref()).is_some() || !steam::steam_roots().is_empty();
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
            ui.label(RichText::new(format!("Helper data: {}", config::data_dir().display())).color(DIM).small());
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
                if steam::steam_running() { "Steam will close and start again to save the change.".into() } else { "The launch option for Cyberpunk 2077 will be changed.".into() },
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
            Confirm::AddToSteam => (
                "Add helper to Steam",
                if steam::steam_running() { "Steam will close, the shortcut is added, then Steam starts again.".into() } else { "A non-Steam game shortcut will be added.".into() },
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
                            ops::backup_saves(&g, settings, state, rep, "before-install")?;
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
                    self.spawn(ctx, &format!("Installing RED4ext {}", rel.version()), move |rep, state, _| ops::install_red4ext_release(&g, &rel, state, rep).map(|_| Extra::None));
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

fn fetch_remote(settings: &Settings) -> RemoteInfo {
    let red4ext = Some(remote::github_releases(remote::RED4EXT_REPO).map_err(|e| format!("{e:#}")));
    let nightcoop = Some(remote::nightcoop_manifest(&settings.manifest_url).map_err(|e| format!("{e:#}")));
    let repo = settings.helper_repo.trim();
    let helper = (!repo.is_empty()).then(|| selfupdate::check(repo).map_err(|e| format!("{e:#}")));
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
                    Page::Servers => self.servers(ui),
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
