import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

let rpcEnabled = false;
let busy = false;

type ButtonCfg = { label: string; url: string };

type PresenceCfg = {
  client_id: string;
  details: string;
  state: string;
  large_image?: string | null;
  large_text?: string | null;
  small_image?: string | null;
  small_text?: string | null;
  buttons: ButtonCfg[];
  with_timestamp: boolean;
};

type UserProfile = {
  id: string;
  username: string;
  global_name?: string | null;
  avatar_hash?: string | null;
  avatar_url?: string | null;
};

type AppMeta = {
  name: string;
  icon_hash?: string | null;
  icon_url?: string | null;
};

type RpcStatus = "inactive" | "connecting" | "active" | "error";

const COOLDOWN_MS_UI = 1200;
let lastActionAt = 0;
let startAt = Date.now();

let cachedAppIconUrl: string | null = null;
let cachedUserAvatarUrl: string | null = null;

// ===== Persistência (localStorage) =====
const STORAGE_KEY = "customrp.config.v1";
let saveTimer: number | null = null;

type StoredConfig = {
  clientId: string;
  details: string;
  state: string;

  largeImage: string;
  largeText: string;
  smallImage: string;
  smallText: string;

  b1label: string;
  b1url: string;
  b2label: string;
  b2url: string;

  ts: boolean;

  pvAvatarSrc: string;
  pvBannerSrc: string;
  pvCardImgSrc: string;
  pvDisplayName: string;
  pvHandle: string;
  pvPresenceLine: string;

  cachedAppIconUrl?: string | null;
  cachedUserAvatarUrl?: string | null;
  pvAppName?: string;
  pvNameText?: string;
  pvHandleText?: string;
  pvStatusText?: string;
};

function now() { return Date.now(); }

function canActUI(): boolean {
  const t = now();
  if (t - lastActionAt < COOLDOWN_MS_UI) return false;
  lastActionAt = t;
  return true;
}

function $(id: string) {
  return document.getElementById(id) as HTMLInputElement;
}
function el(id: string) {
  return document.getElementById(id) as HTMLElement;
}

function setStatus(kind: "ready" | "busy" | "warn" | "ok", text: string, hint?: string) {
  const badge = el("uiStatus");
  const h = el("uiHint");
  badge.textContent = text;
  h.textContent = hint ?? "";

  if (kind === "busy") {
    badge.style.borderColor = "rgba(88,101,242,.35)";
    badge.style.background = "rgba(88,101,242,.18)";
  } else if (kind === "warn") {
    badge.style.borderColor = "rgba(237,66,69,.30)";
    badge.style.background = "rgba(237,66,69,.16)";
  } else if (kind === "ok") {
    badge.style.borderColor = "rgba(59,165,92,.30)";
    badge.style.background = "rgba(59,165,92,.16)";
  } else {
    badge.style.borderColor = "rgba(88,101,242,.25)";
    badge.style.background = "rgba(88,101,242,.15)";
  }
}

function fmtElapsed(ms: number) {
  const s = Math.floor(ms / 1000);
  const mm = String(Math.floor(s / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  return `${mm}:${ss} elapsed`;
}

function normalizeImgSrc(v: string): string | null {
  const s = (v ?? "").trim();
  if (!s) return null;
  if (/^https?:\/\//i.test(s)) return s;
  try {
    return convertFileSrc(s);
  } catch {
    return null;
  }
}

function getCfg(): PresenceCfg {
  const buttons = [
    { label: $("b1label").value.trim(), url: $("b1url").value.trim() },
    { label: $("b2label").value.trim(), url: $("b2url").value.trim() },
  ].filter(b => b.label && b.url);

  const detailsRaw = $("details").value.trim();
  const stateRaw = $("state").value.trim();

  return {
    client_id: $("clientId").value.trim(),
    details: detailsRaw.length >= 2 ? detailsRaw : "",
    state: stateRaw.length >= 2 ? stateRaw : "",
    large_image: $("largeImage").value.trim() || null,
    large_text: $("largeText").value.trim() || null,
    small_image: $("smallImage").value.trim() || null,
    small_text: $("smallText").value.trim() || null,
    buttons,
    with_timestamp: (document.getElementById("ts") as HTMLInputElement).checked === true,
  };
}

function setBusy(disabled: boolean) {
  busy = disabled;

  const toggle = el("toggleBtn") as HTMLButtonElement | null;
  if (toggle) toggle.disabled = disabled;

  (el("updateBtn") as HTMLButtonElement).disabled = disabled;
  (el("syncUserBtn") as HTMLButtonElement).disabled = disabled;
  (el("syncAppBtn") as HTMLButtonElement).disabled = disabled;
  (el("pickAvatarBtn") as HTMLButtonElement).disabled = disabled;
  (el("pickBannerBtn") as HTMLButtonElement).disabled = disabled;
  (el("pickCardBtn") as HTMLButtonElement).disabled = disabled;
}

function renderToggle() {
  const btn = el("toggleBtn") as HTMLButtonElement;

  if (rpcEnabled) {
    btn.textContent = "Desativar";
    btn.classList.remove("primary");
    btn.classList.add("danger");
  } else {
    btn.textContent = "Ativar";
    btn.classList.remove("danger");
    btn.classList.add("primary");
  }
}

async function updateNow() {
  const cfg = getCfg();
  if (!cfg.client_id) {
    setStatus("warn", "Client ID", "Client ID é obrigatório.");
    return;
  }

  setBusy(true);
  setStatus("busy", "Atualizando", "Aplicando alterações no Rich Presence ativo...");

  try {
    await invoke("rpc_update", { cfg });
    setStatus("ok", "Atualizado", "Alterações aplicadas.");
    saveNow();
  } catch (e: any) {
    setStatus("warn", "Erro", String(e));
  } finally {
    setBusy(false);
  }
}


function updatePreview() {
  const cfg = getCfg();

  el("pvDetails").textContent = cfg.details || "—";
  el("pvState").textContent = cfg.state || "—";

  el("pvTime").textContent = cfg.with_timestamp ? fmtElapsed(now() - startAt) : "timestamp off";

  const li = cfg.large_image ? cfg.large_image : "—";
  const si = cfg.small_image ? cfg.small_image : "—";
  el("pvAssets").textContent = `large: ${li} · small: ${si}`;

  const b1 = el("pvBtn1") as HTMLAnchorElement;
  const b2 = el("pvBtn2") as HTMLAnchorElement;

  if (cfg.buttons[0]) {
    b1.style.display = "inline-flex";
    b1.textContent = cfg.buttons[0].label;
    b1.href = cfg.buttons[0].url;
  } else {
    b1.style.display = "none";
  }

  if (cfg.buttons[1]) {
    b2.style.display = "inline-flex";
    b2.textContent = cfg.buttons[1].label;
    b2.href = cfg.buttons[1].url;
  } else {
    b2.style.display = "none";
  }

  const manualName = $("pvDisplayName").value.trim();
  const manualHandle = $("pvHandle").value.trim();
  const manualPresence = $("pvPresenceLine").value.trim();

  el("pvNameText").textContent = manualName || el("pvNameText").textContent || "Você";
  el("pvHandleText").textContent = manualHandle || el("pvHandleText").textContent || "@handle";
  el("pvStatusText").textContent = manualPresence || el("pvStatusText").textContent || "Online";

  const bannerSrc = normalizeImgSrc($("pvBannerSrc").value);
  const avatarSrc = normalizeImgSrc($("pvAvatarSrc").value);
  const cardSrc = normalizeImgSrc($("pvCardImgSrc").value);

  const banner = el("pvBanner");
  if (bannerSrc) {
    banner.style.backgroundImage = `url("${bannerSrc}")`;
    banner.style.backgroundSize = "cover";
    banner.style.backgroundPosition = "center";
  } else {
    banner.style.backgroundImage = "";
  }

  const avatar = el("pvAvatar");
  const avatarFinal = avatarSrc || cachedUserAvatarUrl || cachedAppIconUrl;
  avatar.style.backgroundImage = avatarFinal ? `url("${avatarFinal}")` : "";

  const art = el("pvArt");
  const artFinal = cardSrc || cachedAppIconUrl || avatarFinal;
  art.style.backgroundImage = artFinal ? `url("${artFinal}")` : "";
}

// ===== Persistência =====

function snapshotToStore(): StoredConfig {
  return {
    clientId: $("clientId").value,
    details: $("details").value,
    state: $("state").value,

    largeImage: $("largeImage").value,
    largeText: $("largeText").value,
    smallImage: $("smallImage").value,
    smallText: $("smallText").value,

    b1label: $("b1label").value,
    b1url: $("b1url").value,
    b2label: $("b2label").value,
    b2url: $("b2url").value,

    ts: (document.getElementById("ts") as HTMLInputElement).checked,

    pvAvatarSrc: $("pvAvatarSrc").value,
    pvBannerSrc: $("pvBannerSrc").value,
    pvCardImgSrc: $("pvCardImgSrc").value,
    pvDisplayName: $("pvDisplayName").value,
    pvHandle: $("pvHandle").value,
    pvPresenceLine: $("pvPresenceLine").value,

    cachedAppIconUrl,
    cachedUserAvatarUrl,
    pvAppName: el("pvAppName")?.textContent ?? "App",
    pvNameText: el("pvNameText")?.textContent ?? "Você",
    pvHandleText: el("pvHandleText")?.textContent ?? "@handle",
    pvStatusText: el("pvStatusText")?.textContent ?? "Online",
  };
}

function applyFromStore(s: StoredConfig) {
  $("clientId").value = s.clientId ?? "";
  $("details").value = s.details ?? "";
  $("state").value = s.state ?? "";

  $("largeImage").value = s.largeImage ?? "";
  $("largeText").value = s.largeText ?? "";
  $("smallImage").value = s.smallImage ?? "";
  $("smallText").value = s.smallText ?? "";

  $("b1label").value = s.b1label ?? "";
  $("b1url").value = s.b1url ?? "";
  $("b2label").value = s.b2label ?? "";
  $("b2url").value = s.b2url ?? "";

  (document.getElementById("ts") as HTMLInputElement).checked = !!s.ts;

  $("pvAvatarSrc").value = s.pvAvatarSrc ?? "";
  $("pvBannerSrc").value = s.pvBannerSrc ?? "";
  $("pvCardImgSrc").value = s.pvCardImgSrc ?? "";
  $("pvDisplayName").value = s.pvDisplayName ?? "";
  $("pvHandle").value = s.pvHandle ?? "";
  $("pvPresenceLine").value = s.pvPresenceLine ?? "";

  cachedAppIconUrl = s.cachedAppIconUrl ?? null;
  cachedUserAvatarUrl = s.cachedUserAvatarUrl ?? null;

  if (s.pvAppName) el("pvAppName").textContent = s.pvAppName;
  if (s.pvNameText) el("pvNameText").textContent = s.pvNameText;
  if (s.pvHandleText) el("pvHandleText").textContent = s.pvHandleText;
  if (s.pvStatusText) el("pvStatusText").textContent = s.pvStatusText;

  startAt = now();
}

function saveNow() {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(snapshotToStore()));
  } catch {
    // ignore
  }
}

function scheduleSave() {
  if (saveTimer) window.clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => {
    saveNow();
    saveTimer = null;
  }, 450);
}

function loadIfAny(): boolean {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return false;
    applyFromStore(JSON.parse(raw) as StoredConfig);
    return true;
  } catch {
    return false;
  }
}

// ===== Actions =====

async function pickImage(targetInputId: string) {
  const file = await open({
    multiple: false,
    filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "webp", "gif"] }],
  });

  if (typeof file === "string") {
    $(targetInputId).value = file;
    updatePreview();
    scheduleSave();
  }
}

async function syncUserProfile() {
  const clientId = $("clientId").value.trim();
  if (!clientId) {
    setStatus("warn", "Client ID", "Preencha o Client ID para sincronizar usuário.");
    return;
  }

  setBusy(true);
  setStatus("busy", "Sincronizando", "Buscando username/avatar via IPC...");
  try {
    const prof = await invoke<UserProfile>("get_user_profile", { clientId });

    const display = (prof.global_name && prof.global_name.trim()) ? prof.global_name : prof.username;
    if (!$("pvDisplayName").value.trim()) $("pvDisplayName").value = display || "Você";
    if (!$("pvHandle").value.trim()) $("pvHandle").value = `@${prof.username ?? "handle"}`;

    cachedUserAvatarUrl = prof.avatar_url ?? null;

    setStatus("ok", "Usuário OK", "Preview atualizado com seu username/avatar.");
    updatePreview();
    saveNow();
  } catch (e: any) {
    setStatus("warn", "Falhou", String(e));
  } finally {
    setBusy(false);
  }
}

async function syncAppMeta() {
  const clientId = $("clientId").value.trim();
  if (!clientId) {
    setStatus("warn", "Client ID", "Preencha o Client ID para sincronizar app icon.");
    return;
  }

  setBusy(true);
  setStatus("busy", "Sincronizando", "Buscando nome/ícone do app...");
  try {
    const meta = await invoke<AppMeta>("get_app_meta", { clientId });
    el("pvAppName").textContent = meta?.name || "App";

    cachedAppIconUrl = meta?.icon_url ?? null;

    if (!$("pvDisplayName").value.trim() && meta?.name) {
      $("pvDisplayName").value = meta.name;
    }

    setStatus("ok", "App OK", "Ícone/nome do app aplicados no preview.");
    updatePreview();
    saveNow();
  } catch (e: any) {
    setStatus("warn", "Falhou", String(e));
  } finally {
    setBusy(false);
  }
}

async function enableRpc() {
  const cfg = getCfg();

  if (!cfg.client_id) {
    setStatus("warn", "Client ID", "Client ID é obrigatório.");
    return;
  }

  const d = $("details").value.trim();
  const s = $("state").value.trim();
  if ((d.length > 0 && d.length < 2) || (s.length > 0 && s.length < 2)) {
    setStatus("warn", "Texto inválido", "Details/State precisam ter >= 2 caracteres (ou ficar vazio).");
    return;
  }

  setBusy(true);
  setStatus("busy", "Ativando", "Iniciando worker do RPC...");

  try {
    if (cfg.with_timestamp) startAt = now();

    await invoke("rpc_enable", { cfg });

    // NÃO seta rpcEnabled aqui — quem manda é o rpc_status()
    setStatus("busy", "Conectando", "Aguardando confirmação do Discord...");
    saveNow();
  } catch (e: any) {
    setStatus("warn", "Erro", String(e));
  } finally {
    setBusy(false);
  }
}

async function disableRpc() {
  const clientId = $("clientId").value.trim();
  if (!clientId) {
    setStatus("warn", "Client ID", "Client ID é obrigatório para desativar.");
    return;
  }

  setBusy(true);
  setStatus("busy", "Desativando", "Parando worker e limpando atividade...");
  try {
    await invoke("rpc_disable", { clientId });
    // NÃO seta rpcEnabled aqui — polling vai refletir
    saveNow();
  } catch (e: any) {
    setStatus("warn", "Erro", String(e));
  } finally {
    setBusy(false);
  }
}

async function refreshRpcStatus() {
  try {
    const st = (await invoke<string>("rpc_status")) as RpcStatus;

    if (st === "active") {
      rpcEnabled = true;
      renderToggle();
      if (!busy) {
        setStatus("ok", "Ativo", "Rich Presence exibido no Discord.");
      }

    } else if (st === "connecting") {
      rpcEnabled = true;
      renderToggle();
      if (!busy) {
        setStatus("busy", "Conectando", "Tentando aplicar presença...");
      }

    } else if (st === "error") {
      rpcEnabled = false;
      renderToggle();

      if (!busy) {
        // 🔴 AQUI É O PONTO-CHAVE
        const err = await invoke<string | null>("rpc_last_error");
        setStatus(
          "warn",
          "Erro",
          err ?? "Falha ao aplicar presença no Discord."
        );
      }

    } else {
      rpcEnabled = false;
      renderToggle();
      if (!busy) {
        setStatus("ready", "Inativo", "Rich Presence desativado.");
      }
    }
  } catch (e) {
    if (!busy) {
      setStatus("warn", "Erro", String(e));
    }
  }
}


function bindLivePreviewAndSave() {
  const ids = [
    "clientId",
    "details", "state",
    "largeImage", "largeText", "smallImage", "smallText",
    "b1label", "b1url", "b2label", "b2url",
    "ts",
    "pvAvatarSrc", "pvBannerSrc", "pvCardImgSrc",
    "pvDisplayName", "pvHandle", "pvPresenceLine",
  ];

  ids.forEach(id => {
    const input = document.getElementById(id) as HTMLInputElement | null;
    if (!input) return;

    const handler = () => {
      if (id === "ts") startAt = now();
      updatePreview();
      scheduleSave();
    };

    input.addEventListener("input", handler);
    input.addEventListener("change", handler);
  });

  setInterval(() => {
    const ts = (document.getElementById("ts") as HTMLInputElement).checked;
    if (ts) updatePreview();
  }, 500);
}

function bindButtons() {
  el("toggleBtn")?.addEventListener("click", async () => {
    if (busy) return;
    if (!canActUI()) return;

    // decide pelo status real no momento
    await refreshRpcStatus();

    if (rpcEnabled) {
      await disableRpc();
    } else {
      await enableRpc();
    }
  });
  el("updateBtn")?.addEventListener("click", updateNow);
  el("syncUserBtn")?.addEventListener("click", syncUserProfile);
  el("syncAppBtn")?.addEventListener("click", syncAppMeta);

  el("pickAvatarBtn")?.addEventListener("click", () => pickImage("pvAvatarSrc"));
  el("pickBannerBtn")?.addEventListener("click", () => pickImage("pvBannerSrc"));
  el("pickCardBtn")?.addEventListener("click", () => pickImage("pvCardImgSrc"));
}

// init
bindButtons();
bindLivePreviewAndSave();

const loaded = loadIfAny();
updatePreview();

// estado inicial vem do backend
rpcEnabled = false;
renderToggle();

if (loaded) {
  setStatus("ok", "Carregado", "Config carregada automaticamente.");
} else {
  setStatus("ready", "Pronto", "Preencha Client ID e clique em Sync/Ativar.");
}

// polling leve do status real do worker/RPC
setInterval(refreshRpcStatus, 1500);
refreshRpcStatus();
