use anyhow::Context;
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process,
    time::{SystemTime, UNIX_EPOCH},
};

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn nonce() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

fn send_frame(stream: &mut UnixStream, opcode: i32, payload: &serde_json::Value) -> std::io::Result<()> {
    let bytes = payload.to_string().into_bytes();
    let mut header = Vec::with_capacity(8);
    header.extend_from_slice(&opcode.to_le_bytes());
    header.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> std::io::Result<(i32, serde_json::Value)> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;

    let opcode = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = i32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;

    let v: serde_json::Value =
        serde_json::from_slice(&buf).unwrap_or_else(|_| json!({"_raw": String::from_utf8_lossy(&buf)}));
    Ok((opcode, v))
}

fn find_discord_ipc() -> Option<String> {
    let uid = unsafe { libc::geteuid() };
    let xdg = env::var("XDG_RUNTIME_DIR").ok();

    let mut bases = vec![];
    if let Some(x) = xdg {
        bases.push(x);
    }
    bases.push(format!("/run/user/{}", uid));
    bases.push("/tmp".to_string());

    for base in bases {
        for i in 0..10 {
            let p = format!("{}/discord-ipc-{}", base, i);
            if Path::new(&p).exists() {
                return Some(p);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonCfg {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceCfg {
    pub client_id: String,
    pub details: String,
    pub state: String,

    pub large_image: Option<String>,
    pub large_text: Option<String>,
    pub small_image: Option<String>,
    pub small_text: Option<String>,

    pub buttons: Vec<ButtonCfg>,
    pub with_timestamp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar_hash: Option<String>,
    pub avatar_url: Option<String>,
}

pub struct DiscordRpcClient {
    stream: UnixStream,
    pid: i64,
}

impl DiscordRpcClient {
    pub fn connect_and_handshake(client_id: &str) -> anyhow::Result<(Self, serde_json::Value)> {
        let ipc_path = find_discord_ipc()
            .ok_or_else(|| anyhow::anyhow!("Não achei o socket IPC do Discord. Discord Desktop está rodando?"))?;

        let mut stream = UnixStream::connect(ipc_path).context("Falha ao conectar no discord-ipc")?;

        let hs = json!({ "v": 1, "client_id": client_id });
        send_frame(&mut stream, 0, &hs).context("Falha ao enviar handshake")?;

        let (_op, hs_resp) = read_frame(&mut stream).context("Falha ao ler resposta do handshake")?;
        if hs_resp.get("evt").and_then(|v| v.as_str()) == Some("ERROR") {
            return Err(anyhow::anyhow!("Handshake error: {}", hs_resp));
        }

        Ok((
            Self {
                stream,
                pid: process::id() as i64,
            },
            hs_resp,
        ))
    }

    pub fn set_activity(&mut self, cfg: &PresenceCfg, start_ts: i64) -> anyhow::Result<()> {
        let details_ok = cfg.details.trim().len() >= 2;
        let state_ok = cfg.state.trim().len() >= 2;
        if !details_ok && !state_ok {
            return Err(anyhow::anyhow!(
                "Presence inválida: preencha Details ou State com pelo menos 2 caracteres."
            ));
        }

        let mut activity_map = serde_json::Map::new();
        if details_ok {
            activity_map.insert("details".into(), json!(cfg.details));
        }
        if state_ok {
            activity_map.insert("state".into(), json!(cfg.state));
        }

        let mut activity = json!(activity_map);

        if cfg.with_timestamp {
            activity["timestamps"] = json!({ "start": start_ts });
        }

        let has_assets =
            cfg.large_image.is_some() || cfg.small_image.is_some() || cfg.large_text.is_some() || cfg.small_text.is_some();

        if has_assets {
            let mut assets = serde_json::Map::new();
            if let Some(v) = &cfg.large_image {
                assets.insert("large_image".into(), json!(v));
            }
            if let Some(v) = &cfg.large_text {
                assets.insert("large_text".into(), json!(v));
            }
            if let Some(v) = &cfg.small_image {
                assets.insert("small_image".into(), json!(v));
            }
            if let Some(v) = &cfg.small_text {
                assets.insert("small_text".into(), json!(v));
            }
            activity["assets"] = json!(assets);
        }

            let mut buttons = Vec::new();
            for b in cfg.buttons.iter().take(2) {
                let label = b.label.trim();
                let mut url = b.url.trim().to_string();

                if label.is_empty() || url.is_empty() {
                    continue;
                }

                // remove espaços
                url.retain(|c| !c.is_whitespace());

                // força https
                if url.starts_with("http://") {
                    url = url.replacen("http://", "https://", 1);
                }

                if !url.starts_with("https://") {
                    continue;
                }

                let safe_label = if label.chars().count() > 32 {
                    label.chars().take(32).collect::<String>()
                } else {
                    label.to_string()
                };

                buttons.push(json!({ "label": safe_label, "url": url }));
            }

            if !buttons.is_empty() {
                activity["buttons"] = json!(buttons);
            }


        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": self.pid, "activity": activity },
            "nonce": nonce()
        });

        send_frame(&mut self.stream, 1, &payload).context("Falha ao enviar SET_ACTIVITY")?;

        let (_op2, resp) = read_frame(&mut self.stream).context("Falha ao ler ACK do SET_ACTIVITY")?;
        if resp.get("evt").and_then(|v| v.as_str()) == Some("ERROR") {
            return Err(anyhow::anyhow!("SET_ACTIVITY error: {}", resp));
        }

        Ok(())
    }

    pub fn clear_activity(&mut self) -> anyhow::Result<()> {
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": self.pid, "activity": serde_json::Value::Null },
            "nonce": nonce()
        });

        send_frame(&mut self.stream, 1, &payload).context("Falha ao enviar CLEAR SET_ACTIVITY")?;
        let _ = read_frame(&mut self.stream);
        Ok(())
    }
}

pub fn get_user_profile_via_handshake(client_id: &str) -> anyhow::Result<UserProfile> {
    let (_client, hs_resp) = DiscordRpcClient::connect_and_handshake(client_id)?;

    let user = hs_resp
        .get("data")
        .and_then(|d| d.get("user"))
        .ok_or_else(|| anyhow::anyhow!("Handshake não retornou data.user: {}", hs_resp))?;

    let id = user.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let username = user.get("username").and_then(|v| v.as_str()).unwrap_or("user").to_string();
    let global_name = user.get("global_name").and_then(|v| v.as_str()).map(|s| s.to_string());
    let avatar_hash = user.get("avatar").and_then(|v| v.as_str()).map(|s| s.to_string());

    let avatar_url = avatar_hash.as_ref().map(|hash| {
        let ext = if hash.starts_with("a_") { "gif" } else { "png" };
        format!("https://cdn.discordapp.com/avatars/{}/{}.{}?size=128", id, hash, ext)
    });

    Ok(UserProfile { id, username, global_name, avatar_hash, avatar_url })
}

/// útil se quiser setar start_ts no backend
pub fn now_unix_ts() -> i64 {
    now_unix()
}
