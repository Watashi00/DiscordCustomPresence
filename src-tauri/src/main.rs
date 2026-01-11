#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod rpc;

use rpc::{DiscordRpcClient, PresenceCfg};
use std::sync::{Arc, Mutex, Condvar};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// ----------------------------
/// Backend rate limiter
/// ----------------------------
struct RateState {
    last: Option<Instant>,
}
impl Default for RateState {
    fn default() -> Self { Self { last: None } }
}

fn rate_check(state: &Mutex<RateState>, min_delay: Duration) -> Result<(), String> {
    let mut st = state.lock().unwrap();
    if let Some(last) = st.last {
        if last.elapsed() < min_delay {
            return Err("Rate-limit: aguarde um pouco antes de repetir a ação.".to_string());
        }
    }
    st.last = Some(Instant::now());
    Ok(())
}

/// ----------------------------
/// RPC status + worker state
/// ----------------------------
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

    /// Latest config snapshot (updated by rpc_enable/rpc_update)
    cfg: Mutex<Option<PresenceCfg>>,

    /// Fixed start timestamp for elapsed timer (do NOT change while running)
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

fn set_status(w: &Arc<RpcWorker>, st: RpcStatus) {
    *w.status.lock().unwrap() = st;
}
fn set_error(w: &Arc<RpcWorker>, msg: Option<String>) {
    *w.last_error.lock().unwrap() = msg;
}

/// ----------------------------
/// Poke / Signal: allow instant update
/// ----------------------------
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

    /// Wait until:
    /// - someone calls poke()
    /// - or timeout expires
    fn wait_or_timeout(&self, dur: Duration) {
        let mut f = self.flag.lock().unwrap();

        // if already poked, consume immediately
        if *f {
            *f = false;
            return;
        }

        let (mut f2, _) = self.cv.wait_timeout(f, dur).unwrap();
        *f2 = false; // consume poke if any
    }
}

/// ----------------------------
/// Tauri commands
/// ----------------------------

#[tauri::command]
fn rpc_status(worker: tauri::State<'_, Arc<RpcWorker>>) -> String {
    worker.status.lock().unwrap().as_str().to_string()
}

#[tauri::command]
fn rpc_last_error(worker: tauri::State<'_, Arc<RpcWorker>>) -> Option<String> {
    worker.last_error.lock().unwrap().clone()
}

#[tauri::command]
fn get_user_profile(
    client_id: String,
    rate: tauri::State<'_, Mutex<RateState>>,
) -> Result<rpc::UserProfile, String> {
    rate_check(&rate, Duration::from_millis(650))?;
    rpc::get_user_profile_via_handshake(&client_id).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AppMeta {
    name: String,
    icon_hash: Option<String>,
    icon_url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RpcAppResp {
    name: String,
    icon: Option<String>,
}

#[tauri::command]
async fn get_app_meta(
    client_id: String,
    rate: tauri::State<'_, Mutex<RateState>>,
) -> Result<AppMeta, String> {
    rate_check(&rate, Duration::from_millis(650))?;

    let url = format!("https://discord.com/api/v10/oauth2/applications/{}/rpc", client_id);

    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json::<RpcAppResp>()
        .await
        .map_err(|e| e.to_string())?;

    let icon_url = resp.icon.as_ref().map(|h| {
        format!("https://cdn.discordapp.com/app-icons/{}/{}.png?size=256", client_id, h)
    });

    Ok(AppMeta { name: resp.name, icon_hash: resp.icon, icon_url })
}

/// Enable worker (starts thread once).
/// If already running, just updates config and pokes the worker to apply changes quickly.
#[tauri::command]
async fn rpc_enable(
    cfg: PresenceCfg,
    rate: tauri::State<'_, Mutex<RateState>>,
    worker: tauri::State<'_, Arc<RpcWorker>>,
    signal: tauri::State<'_, Arc<RpcSignal>>,
) -> Result<(), String> {
    rate_check(&rate, Duration::from_millis(900))?;

    // Store cfg
    {
        let mut lock = worker.cfg.lock().unwrap();
        *lock = Some(cfg);
    }

    // Start timestamp: set ONCE per "enable session"
    {
        let mut st = worker.start_ts.lock().unwrap();
        if st.is_none() {
            *st = Some(rpc::now_unix_ts());
        }
    }

    worker.running.store(true, Ordering::SeqCst);

    // If thread already running: just poke to apply right now
    if worker.thread_alive.load(Ordering::SeqCst) {
        signal.poke();
        return Ok(());
    }

    // Mark thread alive
    worker.thread_alive.store(true, Ordering::SeqCst);

    let w = worker.inner().clone();
    let sig = signal.inner().clone();

    thread::spawn(move || {
        // Quick "burst" on start to stabilize
        let fast_schedule = [
            Duration::from_secs(0),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
        ];

        // Keepalive interval (stable). Updates will also happen on poke().
        let keepalive_tick = Duration::from_secs(10);

        set_status(&w, RpcStatus::Connecting);
        set_error(&w, None);

        let mut client: Option<DiscordRpcClient> = None;

        while w.running.load(Ordering::SeqCst) {
            // Snapshot config
            let cfg_opt = { w.cfg.lock().unwrap().clone() };
            let cfg = match cfg_opt {
                Some(c) => c,
                None => {
                    set_status(&w, RpcStatus::Inactive);
                    break;
                }
            };

            // Fixed start timestamp (do not change while running)
            let start_ts = *w.start_ts.lock().unwrap().get_or_insert_with(rpc::now_unix_ts);

            // Ensure persistent IPC client
            if client.is_none() {
                set_status(&w, RpcStatus::Connecting);

                match DiscordRpcClient::connect_and_handshake(&cfg.client_id) {
                    Ok((c, _hs)) => {
                        client = Some(c);
                        set_error(&w, None);
                    }
                    Err(e) => {
                        set_status(&w, RpcStatus::Error);
                        set_error(&w, Some(e.to_string()));
                        // Wait a bit (or until poke) and retry
                        sig.wait_or_timeout(Duration::from_secs(2));
                        continue;
                    }
                }
            }

            // Burst apply (helps the Discord client "latch" onto the presence)
            {
                let mut ok_streak = 0u8;

                for d in fast_schedule {
                    if !w.running.load(Ordering::SeqCst) { break; }
                    if d.as_secs() > 0 { thread::sleep(d); }

                    // config may have changed during burst
                    let cfg2 = { w.cfg.lock().unwrap().clone() }.unwrap_or_else(|| cfg.clone());

                    let res = match client.as_mut() {
                        Some(c) => c.set_activity(&cfg2, start_ts),
                        None => Err(anyhow::anyhow!("client is None")),
                    };

                    match res {
                        Ok(_) => {
                            ok_streak = ok_streak.saturating_add(1);
                            set_error(&w, None);
                            if ok_streak >= 2 {
                                set_status(&w, RpcStatus::Active);
                                break;
                            } else {
                                set_status(&w, RpcStatus::Connecting);
                            }
                        }
                        Err(e) => {
                            set_status(&w, RpcStatus::Error);
                            set_error(&w, Some(e.to_string()));
                            client = None; // force reconnect
                            break;
                        }
                    }
                }
            }

            if !w.running.load(Ordering::SeqCst) { break; }

            // Wait for keepalive OR an explicit "poke" (rpc_update)
            sig.wait_or_timeout(keepalive_tick);

            if !w.running.load(Ordering::SeqCst) { break; }

            // Apply latest cfg immediately after wait (whether poke or timeout)
            let cfg3 = { w.cfg.lock().unwrap().clone() }.unwrap_or_else(|| cfg.clone());

            let res = match client.as_mut() {
                Some(c) => c.set_activity(&cfg3, start_ts),
                None => Err(anyhow::anyhow!("client is None")),
            };

            match res {
                Ok(_) => {
                    set_status(&w, RpcStatus::Active);
                    set_error(&w, None);
                }
                Err(e) => {
                    set_status(&w, RpcStatus::Error);
                    set_error(&w, Some(e.to_string()));
                    client = None; // reconnect next loop
                    sig.wait_or_timeout(Duration::from_secs(2));
                }
            }
        }

        // On stop: clear activity (best effort)
        if let Some(mut c) = client {
            let _ = c.clear_activity();
        }

        // Reset start timestamp so next enable starts fresh
        *w.start_ts.lock().unwrap() = None;

        set_status(&w, RpcStatus::Inactive);
        set_error(&w, None);
        w.thread_alive.store(false, Ordering::SeqCst);
    });

    Ok(())
}

/// Update config while worker is running (or even when stopped).
/// If running, this pokes the worker so it applies immediately.
#[tauri::command]
async fn rpc_update(
    cfg: PresenceCfg,
    rate: tauri::State<'_, Mutex<RateState>>,
    worker: tauri::State<'_, Arc<RpcWorker>>,
    signal: tauri::State<'_, Arc<RpcSignal>>,
) -> Result<(), String> {
    rate_check(&rate, Duration::from_millis(350))?;

    {
        let mut lock = worker.cfg.lock().unwrap();
        *lock = Some(cfg);
    }

    if worker.running.load(Ordering::SeqCst) {
        signal.poke();
    }

    Ok(())
}

/// Disable worker (stops loop). Worker clears activity best-effort.
#[tauri::command]
async fn rpc_disable(
    _client_id: String,
    rate: tauri::State<'_, Mutex<RateState>>,
    worker: tauri::State<'_, Arc<RpcWorker>>,
    signal: tauri::State<'_, Arc<RpcSignal>>,
) -> Result<(), String> {
    rate_check(&rate, Duration::from_millis(900))?;
    worker.running.store(false, Ordering::SeqCst);
    signal.poke(); // wake worker so it exits quickly
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::new(RateState::default()))
        .manage(Arc::new(RpcWorker::default()))
        .manage(Arc::new(RpcSignal::default()))
        .invoke_handler(tauri::generate_handler![
            rpc_enable,
            rpc_update,
            rpc_disable,
            rpc_status,
            rpc_last_error,
            get_user_profile,
            get_app_meta
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
