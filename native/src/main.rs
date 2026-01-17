#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use directories::ProjectDirs;
use eframe::egui;
use rpc_core::{ButtonCfg, DiscordRpcClient, PresenceCfg, UserProfile};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcStatus {
    Inactive,
    Connecting,
    Active,
    Error,
}

impl RpcStatus {
    fn as_str(&self) -> &'static str {
        match self {
            RpcStatus::Inactive => "inactive",
            RpcStatus::Connecting => "connecting",
            RpcStatus::Active => "active",
            RpcStatus::Error => "error",
        }
    }
}

struct RpcWorker {
    running: AtomicBool,
    thread_alive: AtomicBool,
    status: Mutex<RpcStatus>,
    last_error: Mutex<Option<String>>,
    cfg: Mutex<Option<PresenceCfg>>,
    start_ts: Mutex<Option<i64>>,
}

impl Default for RpcWorker {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            thread_alive: AtomicBool::new(false),
            status: Mutex::new(RpcStatus::Inactive),
            last_error: Mutex::new(None),
            cfg: Mutex::new(None),
            start_ts: Mutex::new(None),
        }
    }
}

impl RpcWorker {
    fn status(&self) -> RpcStatus {
        *self.status.lock().unwrap()
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    fn enable(self: &Arc<Self>, cfg: PresenceCfg, signal: &Arc<RpcSignal>) -> Result<(), String> {
        {
            let mut lock = self.cfg.lock().unwrap();
            *lock = Some(cfg);
        }

        {
            let mut st = self.start_ts.lock().unwrap();
            if st.is_none() {
                *st = Some(rpc_core::now_unix_ts());
            }
        }

        self.running.store(true, Ordering::SeqCst);

        if self.thread_alive.load(Ordering::SeqCst) {
            signal.poke();
            return Ok(());
        }

        self.thread_alive.store(true, Ordering::SeqCst);
        let w = Arc::clone(self);
        let sig = Arc::clone(signal);

        thread::spawn(move || {
            let fast_schedule = [
                Duration::from_secs(0),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ];
            let keepalive_tick = Duration::from_secs(10);

            *w.status.lock().unwrap() = RpcStatus::Connecting;
            *w.last_error.lock().unwrap() = None;

            let mut client: Option<DiscordRpcClient> = None;

            while w.running.load(Ordering::SeqCst) {
                let cfg_opt = { w.cfg.lock().unwrap().clone() };
                let cfg = match cfg_opt {
                    Some(c) => c,
                    None => {
                        *w.status.lock().unwrap() = RpcStatus::Inactive;
                        break;
                    }
                };

                let start_ts = *w.start_ts.lock().unwrap().get_or_insert_with(rpc_core::now_unix_ts);

                if client.is_none() {
                    *w.status.lock().unwrap() = RpcStatus::Connecting;
                    match DiscordRpcClient::connect_and_handshake(&cfg.client_id) {
                        Ok((c, _hs)) => {
                            client = Some(c);
                            *w.last_error.lock().unwrap() = None;
                        }
                        Err(e) => {
                            *w.status.lock().unwrap() = RpcStatus::Error;
                            *w.last_error.lock().unwrap() = Some(e.to_string());
                            sig.wait_or_timeout(Duration::from_secs(2));
                            continue;
                        }
                    }
                }

                {
                    let mut ok_streak = 0u8;
                    for d in fast_schedule {
                        if !w.running.load(Ordering::SeqCst) {
                            break;
                        }
                        if d.as_secs() > 0 {
                            thread::sleep(d);
                        }

                        let cfg2 = { w.cfg.lock().unwrap().clone() }.unwrap_or_else(|| cfg.clone());

                        let res = match client.as_mut() {
                            Some(c) => c.set_activity(&cfg2, start_ts),
                            None => Err(anyhow::anyhow!("client is None")),
                        };

                        match res {
                            Ok(_) => {
                                ok_streak = ok_streak.saturating_add(1);
                                *w.last_error.lock().unwrap() = None;
                                if ok_streak >= 2 {
                                    *w.status.lock().unwrap() = RpcStatus::Active;
                                    break;
                                } else {
                                    *w.status.lock().unwrap() = RpcStatus::Connecting;
                                }
                            }
                            Err(e) => {
                                *w.status.lock().unwrap() = RpcStatus::Error;
                                *w.last_error.lock().unwrap() = Some(e.to_string());
                                client = None;
                                break;
                            }
                        }
                    }
                }

                if !w.running.load(Ordering::SeqCst) {
                    break;
                }

                sig.wait_or_timeout(keepalive_tick);
                if !w.running.load(Ordering::SeqCst) {
                    break;
                }

                let cfg3 = { w.cfg.lock().unwrap().clone() }.unwrap_or_else(|| cfg.clone());
                let res = match client.as_mut() {
                    Some(c) => c.set_activity(&cfg3, start_ts),
                    None => Err(anyhow::anyhow!("client is None")),
                };

                match res {
                    Ok(_) => {
                        *w.status.lock().unwrap() = RpcStatus::Active;
                        *w.last_error.lock().unwrap() = None;
                    }
                    Err(e) => {
                        *w.status.lock().unwrap() = RpcStatus::Error;
                        *w.last_error.lock().unwrap() = Some(e.to_string());
                        client = None;
                        sig.wait_or_timeout(Duration::from_secs(2));
                    }
                }
            }

            if let Some(mut c) = client {
                let _ = c.clear_activity();
            }

            *w.start_ts.lock().unwrap() = None;
            *w.status.lock().unwrap() = RpcStatus::Inactive;
            *w.last_error.lock().unwrap() = None;
            w.thread_alive.store(false, Ordering::SeqCst);
        });

        Ok(())
    }

    fn update(&self, cfg: PresenceCfg, signal: &Arc<RpcSignal>) -> Result<(), String> {
        {
            let mut lock = self.cfg.lock().unwrap();
            *lock = Some(cfg);
        }

        if self.running.load(Ordering::SeqCst) {
            signal.poke();
        }

        Ok(())
    }

    fn disable(&self, signal: &Arc<RpcSignal>) -> Result<(), String> {
        self.running.store(false, Ordering::SeqCst);
        signal.poke();
        Ok(())
    }
}

struct RpcSignal {
    cv: Condvar,
    flag: Mutex<bool>,
}

impl Default for RpcSignal {
    fn default() -> Self {
        Self {
            cv: Condvar::new(),
            flag: Mutex::new(false),
        }
    }
}

impl RpcSignal {
    fn poke(&self) {
        let mut f = self.flag.lock().unwrap();
        *f = true;
        self.cv.notify_all();
    }

    fn wait_or_timeout(&self, dur: Duration) {
        let mut f = self.flag.lock().unwrap();
        if *f {
            *f = false;
            return;
        }
        let (mut f2, _) = self.cv.wait_timeout(f, dur).unwrap();
        *f2 = false;
    }
}

#[derive(Default)]
struct RateState {
    last: Option<Instant>,
}

fn rate_check(state: &Mutex<RateState>, min_delay: Duration) -> Result<(), String> {
    let mut st = state.lock().unwrap();
    if let Some(last) = st.last {
        if last.elapsed() < min_delay {
            return Err("Rate limit: please wait a moment before repeating the action.".to_string());
        }
    }
    st.last = Some(Instant::now());
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct StoredConfig {
    client_id: String,
    details: String,
    state: String,
    large_image: String,
    large_text: String,
    small_image: String,
    small_text: String,
    b1label: String,
    b1url: String,
    b2label: String,
    b2url: String,
    with_timestamp: bool,
    last_user_name: String,
    last_user_avatar: String,
    last_app_name: String,
    last_app_icon: String,
}

#[derive(Default, Clone)]
struct FormConfig {
    client_id: String,
    details: String,
    state: String,
    large_image: String,
    large_text: String,
    small_image: String,
    small_text: String,
    b1label: String,
    b1url: String,
    b2label: String,
    b2url: String,
    with_timestamp: bool,
}

impl FormConfig {
    fn to_presence_cfg(&self) -> PresenceCfg {
        let mut buttons = Vec::new();
        if !self.b1label.trim().is_empty() || !self.b1url.trim().is_empty() {
            buttons.push(ButtonCfg {
                label: self.b1label.trim().to_string(),
                url: self.b1url.trim().to_string(),
            });
        }
        if !self.b2label.trim().is_empty() || !self.b2url.trim().is_empty() {
            buttons.push(ButtonCfg {
                label: self.b2label.trim().to_string(),
                url: self.b2url.trim().to_string(),
            });
        }

        let details = self.details.trim().to_string();
        let state = self.state.trim().to_string();

        PresenceCfg {
            client_id: self.client_id.trim().to_string(),
            details: if details.len() >= 2 { details } else { String::new() },
            state: if state.len() >= 2 { state } else { String::new() },
            large_image: opt_str(&self.large_image),
            large_text: opt_str(&self.large_text),
            small_image: opt_str(&self.small_image),
            small_text: opt_str(&self.small_text),
            buttons,
            with_timestamp: self.with_timestamp,
        }
    }

    fn from_stored(s: &StoredConfig) -> Self {
        Self {
            client_id: s.client_id.clone(),
            details: s.details.clone(),
            state: s.state.clone(),
            large_image: s.large_image.clone(),
            large_text: s.large_text.clone(),
            small_image: s.small_image.clone(),
            small_text: s.small_text.clone(),
            b1label: s.b1label.clone(),
            b1url: s.b1url.clone(),
            b2label: s.b2label.clone(),
            b2url: s.b2url.clone(),
            with_timestamp: s.with_timestamp,
        }
    }
}

fn opt_str(v: &str) -> Option<String> {
    let s = v.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

#[derive(Debug, Clone)]
struct AppMeta {
    name: String,
    icon_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RpcAppResp {
    name: String,
    icon: Option<String>,
}

enum AppEvent {
    UserProfile(Result<UserProfile, String>),
    AppMeta(Result<AppMeta, String>),
}

struct AppState {
    worker: Arc<RpcWorker>,
    signal: Arc<RpcSignal>,
    rate: Mutex<RateState>,
    events_tx: mpsc::Sender<AppEvent>,
    events_rx: mpsc::Receiver<AppEvent>,
    cfg_path: Option<PathBuf>,
    form: FormConfig,
    last_user_name: String,
    last_user_avatar: String,
    last_app_name: String,
    last_app_icon: String,
    last_message: String,
    last_error: String,
    dirty_since: Option<Instant>,
}

impl AppState {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let cfg_path = config_path();
        let mut stored = StoredConfig::default();
        if let Some(path) = &cfg_path {
            if let Ok(raw) = fs::read_to_string(path) {
                if let Ok(parsed) = serde_json::from_str::<StoredConfig>(&raw) {
                    stored = parsed;
                }
            }
        }

        let form = FormConfig::from_stored(&stored);

        Self {
            worker: Arc::new(RpcWorker::default()),
            signal: Arc::new(RpcSignal::default()),
            rate: Mutex::new(RateState::default()),
            events_tx: tx,
            events_rx: rx,
            cfg_path,
            form,
            last_user_name: stored.last_user_name,
            last_user_avatar: stored.last_user_avatar,
            last_app_name: stored.last_app_name,
            last_app_icon: stored.last_app_icon,
            last_message: String::new(),
            last_error: String::new(),
            dirty_since: None,
        }
    }

    fn save_config(&mut self) {
        let Some(path) = &self.cfg_path else { return; };
        let stored = StoredConfig {
            client_id: self.form.client_id.clone(),
            details: self.form.details.clone(),
            state: self.form.state.clone(),
            large_image: self.form.large_image.clone(),
            large_text: self.form.large_text.clone(),
            small_image: self.form.small_image.clone(),
            small_text: self.form.small_text.clone(),
            b1label: self.form.b1label.clone(),
            b1url: self.form.b1url.clone(),
            b2label: self.form.b2label.clone(),
            b2url: self.form.b2url.clone(),
            with_timestamp: self.form.with_timestamp,
            last_user_name: self.last_user_name.clone(),
            last_user_avatar: self.last_user_avatar.clone(),
            last_app_name: self.last_app_name.clone(),
            last_app_icon: self.last_app_icon.clone(),
        };

        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(raw) = serde_json::to_string_pretty(&stored) {
            let _ = fs::write(path, raw);
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn maybe_autosave(&mut self) {
        let Some(at) = self.dirty_since else { return; };
        if at.elapsed() >= Duration::from_millis(500) {
            self.save_config();
            self.dirty_since = None;
        }
    }

    fn sync_user(&mut self) {
        let client_id = self.form.client_id.trim().to_string();
        if client_id.is_empty() {
            self.last_error = "Client ID is required.".to_string();
            return;
        }
        if let Err(e) = rate_check(&self.rate, Duration::from_millis(650)) {
            self.last_error = e;
            return;
        }

        let tx = self.events_tx.clone();
        thread::spawn(move || {
            let res = rpc_core::get_user_profile_via_handshake(&client_id)
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::UserProfile(res));
        });
    }

    fn sync_app(&mut self) {
        let client_id = self.form.client_id.trim().to_string();
        if client_id.is_empty() {
            self.last_error = "Client ID is required.".to_string();
            return;
        }
        if let Err(e) = rate_check(&self.rate, Duration::from_millis(650)) {
            self.last_error = e;
            return;
        }

        let tx = self.events_tx.clone();
        thread::spawn(move || {
            let res = fetch_app_meta(&client_id).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::AppMeta(res));
        });
    }

    fn enable_rpc(&mut self) {
        let cfg = self.form.to_presence_cfg();
        if cfg.client_id.is_empty() {
            self.last_error = "Client ID is required.".to_string();
            return;
        }
        if let Err(e) = rate_check(&self.rate, Duration::from_millis(900)) {
            self.last_error = e;
            return;
        }
        if let Err(e) = self.worker.enable(cfg, &self.signal) {
            self.last_error = e;
            return;
        }
        self.last_message = "RPC enabled.".to_string();
        self.save_config();
    }

    fn update_rpc(&mut self) {
        let cfg = self.form.to_presence_cfg();
        if cfg.client_id.is_empty() {
            self.last_error = "Client ID is required.".to_string();
            return;
        }
        if let Err(e) = rate_check(&self.rate, Duration::from_millis(350)) {
            self.last_error = e;
            return;
        }
        if let Err(e) = self.worker.update(cfg, &self.signal) {
            self.last_error = e;
            return;
        }
        self.last_message = "RPC updated.".to_string();
        self.save_config();
    }

    fn disable_rpc(&mut self) {
        if let Err(e) = rate_check(&self.rate, Duration::from_millis(900)) {
            self.last_error = e;
            return;
        }
        if let Err(e) = self.worker.disable(&self.signal) {
            self.last_error = e;
            return;
        }
        self.last_message = "RPC disabled.".to_string();
        self.save_config();
    }

    fn handle_events(&mut self) {
        while let Ok(evt) = self.events_rx.try_recv() {
            match evt {
                AppEvent::UserProfile(res) => match res {
                    Ok(profile) => {
                        let display = if let Some(g) = profile.global_name.as_ref() {
                            if !g.trim().is_empty() { g.clone() } else { profile.username.clone() }
                        } else {
                            profile.username.clone()
                        };
                        self.last_user_name = display;
                        self.last_user_avatar = profile.avatar_url.unwrap_or_default();
                        self.last_message = "User synced.".to_string();
                        self.last_error.clear();
                        self.save_config();
                    }
                    Err(e) => {
                        self.last_error = e;
                    }
                },
                AppEvent::AppMeta(res) => match res {
                    Ok(meta) => {
                        self.last_app_name = meta.name;
                        self.last_app_icon = meta.icon_url.unwrap_or_default();
                        self.last_message = "App synced.".to_string();
                        self.last_error.clear();
                        self.save_config();
                    }
                    Err(e) => {
                        self.last_error = e;
                    }
                },
            }
        }
    }
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events();
        self.maybe_autosave();

        let status = self.worker.status();
        let err = self.worker.last_error();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Custom Rich Presence (Native)");
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label(format!("RPC status: {}", status.as_str()));
                if let Some(e) = err {
                    ui.label(format!("error: {}", e));
                }
            });

            if !self.last_error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), &self.last_error);
            } else if !self.last_message.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(60, 170, 90), &self.last_message);
            }

            ui.separator();
            egui::Grid::new("cfg_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Client ID");
                if ui.text_edit_singleline(&mut self.form.client_id).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Details");
                if ui.text_edit_singleline(&mut self.form.details).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("State");
                if ui.text_edit_singleline(&mut self.form.state).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Large image");
                if ui.text_edit_singleline(&mut self.form.large_image).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Large text");
                if ui.text_edit_singleline(&mut self.form.large_text).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Small image");
                if ui.text_edit_singleline(&mut self.form.small_image).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Small text");
                if ui.text_edit_singleline(&mut self.form.small_text).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Button 1 label");
                if ui.text_edit_singleline(&mut self.form.b1label).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Button 1 url");
                if ui.text_edit_singleline(&mut self.form.b1url).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Button 2 label");
                if ui.text_edit_singleline(&mut self.form.b2label).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Button 2 url");
                if ui.text_edit_singleline(&mut self.form.b2url).changed() { self.mark_dirty(); }
                ui.end_row();

                ui.label("Timestamp");
                if ui.checkbox(&mut self.form.with_timestamp, "enabled").changed() { self.mark_dirty(); }
                ui.end_row();
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let active = matches!(status, RpcStatus::Active | RpcStatus::Connecting);
                if ui.button(if active { "Disable" } else { "Enable" }).clicked() {
                    self.last_error.clear();
                    if active {
                        self.disable_rpc();
                    } else {
                        self.enable_rpc();
                    }
                }
                if ui.button("Update").clicked() {
                    self.last_error.clear();
                    self.update_rpc();
                }
                if ui.button("Sync user").clicked() {
                    self.last_error.clear();
                    self.sync_user();
                }
                if ui.button("Sync app").clicked() {
                    self.last_error.clear();
                    self.sync_app();
                }
                if ui.button("Save").clicked() {
                    self.save_config();
                    self.last_message = "Configuration saved.".to_string();
                    self.last_error.clear();
                }
            });

            ui.separator();
            ui.label(format!("Last user: {}", if self.last_user_name.is_empty() { "-" } else { &self.last_user_name }));
            ui.label(format!("User avatar URL: {}", if self.last_user_avatar.is_empty() { "-" } else { &self.last_user_avatar }));
            ui.label(format!("Last app: {}", if self.last_app_name.is_empty() { "-" } else { &self.last_app_name }));
            ui.label(format!("App icon URL: {}", if self.last_app_icon.is_empty() { "-" } else { &self.last_app_icon }));
        });

        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

fn config_path() -> Option<PathBuf> {
    let proj = ProjectDirs::from("com", "Watashi", "CustomRichPresence")?;
    Some(proj.config_dir().join("config.json"))
}

fn fetch_app_meta(client_id: &str) -> anyhow::Result<AppMeta> {
    let url = format!("https://discord.com/api/v10/oauth2/applications/{}/rpc", client_id);
    let resp = reqwest::blocking::Client::new()
        .get(url)
        .send()
        .context("Failed to call Discord API")?
        .error_for_status()
        .context("HTTP error while fetching app metadata")?
        .json::<RpcAppResp>()
        .context("Failed to decode response")?;

    let icon_url = resp.icon.as_ref().map(|h| {
        format!("https://cdn.discordapp.com/app-icons/{}/{}.png?size=256", client_id, h)
    });
    Ok(AppMeta { name: resp.name, icon_url })
}

fn main() -> eframe::Result<()> {
    let app = AppState::new();
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Custom Rich Presence (Native)",
        options,
        Box::new(|_cc| Box::new(app)),
    )
}
