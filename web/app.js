(() => {
  "use strict";

  const ui = Object.fromEntries([...document.querySelectorAll("[id]")].map((node) => [node.id, node]));
  const sessionControlSelector = "[data-nav], [data-profile], [data-egress], [data-autoburn], #blocklist-toggle, #burn-button, #omnibox-form button";
  const state = {
    csrf: null,
    session: null,
    socket: null,
    socketSessionId: null,
    eventsConnectingSessionId: null,
    eventsReconnectTimer: null,
    eventsReconnectAttempts: 0,
    metricsTimer: null,
    metricsWatchdog: null,
    metricsController: null,
    metricsSequence: 0,
    lastMetricsAt: null,
    toastTimer: null,
    pairingInFlight: false,
    busy: false,
  };

  const mutation = (method, body) => ({
    method,
    credentials: "same-origin",
    headers: { "content-type": "application/json", "x-xanhtab-csrf": state.csrf || "" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  async function request(path, options = {}) {
    const response = await fetch(path, { credentials: "same-origin", ...options });
    if (response.status === 204) return null;
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      const error = new Error(payload?.error?.message || `HTTP ${response.status}`);
      error.code = payload?.error?.code || "HTTP_ERROR";
      error.status = response.status;
      throw error;
    }
    return payload;
  }

  function toast(message, danger = false) {
    ui.toast.textContent = message.toUpperCase();
    ui.toast.dataset.tone = danger ? "danger" : "success";
    window.clearTimeout(state.toastTimer);
    state.toastTimer = window.setTimeout(() => {
      ui.toast.textContent = "SYSTEM READY";
      ui.toast.dataset.tone = "neutral";
    }, danger ? 5000 : 4000);
  }

  function setMetricsStatus(label, state = "idle") {
    ui["metrics-status"].textContent = label;
    ui["metrics-status"].closest(".live-label").dataset.state = state;
  }

  function isAuthError(error) {
    return error?.code === "AUTH_REQUIRED" || (error?.status === 401 && error?.code !== "PAIRING_INVALID");
  }

  function stopMetricPolling(label, status = "idle") {
    window.clearInterval(state.metricsTimer);
    window.clearInterval(state.metricsWatchdog);
    state.metricsTimer = null;
    state.metricsWatchdog = null;
    state.metricsSequence += 1;
    state.metricsController?.abort();
    state.metricsController = null;
    state.lastMetricsAt = null;
    if (label) setMetricsStatus(label, status);
  }

  function stopEventChannel() {
    window.clearTimeout(state.eventsReconnectTimer);
    state.eventsReconnectTimer = null;
    state.eventsReconnectAttempts = 0;
    const socket = state.socket;
    state.socket = null;
    state.socketSessionId = null;
    socket?.close();
  }

  function scheduleEventReconnect(sessionId) {
    if (state.eventsReconnectTimer || !state.csrf || state.session?.id !== sessionId || state.session?.phase !== "active") return;
    const delay = Math.min(1000 * (2 ** state.eventsReconnectAttempts), 15000);
    state.eventsReconnectAttempts += 1;
    state.eventsReconnectTimer = window.setTimeout(() => {
      state.eventsReconnectTimer = null;
      if (state.csrf && state.session?.id === sessionId && state.session?.phase === "active") {
        connectEvents().catch(handleActionError);
      }
    }, delay);
  }

  function showPairingDialog() {
    if (ui["burn-dialog"].open) ui["burn-dialog"].close();
    if (!ui["pairing-dialog"].open) ui["pairing-dialog"].showModal();
    window.requestAnimationFrame(() => ui["pairing-secret"].focus());
  }

  function lockAuthentication(message = "SESSION AUTH EXPIRED") {
    state.csrf = null;
    stopEventChannel();
    stopMetricPolling("LOCKED", "locked");
    const terminalBurn = state.session?.phase === "burning" || state.session?.phase === "idle";
    const snapshot = state.session && !terminalBurn
      ? { ...state.session, controller_attached: false }
      : { phase: "idle", egress: "direct", stream_profile: "1080p30", controller_attached: false };
    renderSession(snapshot);
    showPairingDialog();
    toast(message, true);
  }

  function handleActionError(error) {
    if (isAuthError(error)) {
      lockAuthentication();
      return;
    }
    toast(error.message, true);
  }

  function setBusy(busy) {
    state.busy = busy;
    document.querySelector(".app-shell").setAttribute("aria-busy", String(busy));
    const active = state.session?.phase === "active" && Boolean(state.csrf);
    document.querySelectorAll("button").forEach((button) => {
      const needsSession = button.matches(sessionControlSelector);
      const pairingBusy = button.id === "pairing-submit" && state.pairingInFlight;
      button.disabled = busy || pairingBusy || (needsSession && !active);
    });
    if (busy) toast("APPLYING TRANSACTION");
  }

  function renderViewportTitle(failed) {
    ui["viewport-title"].replaceChildren(document.createTextNode(failed ? "Phiên cần được burn." : "Một cửa sổ."));
    if (!failed) ui["viewport-title"].append(document.createElement("br"), document.createTextNode("Không dấu vết."));
  }

  function renderSession(snapshot, history = []) {
    const previousSessionId = state.session?.id || null;
    const nextSessionId = snapshot?.id || null;
    if (previousSessionId && previousSessionId !== nextSessionId) stopEventChannel();
    state.session = snapshot;
    const active = snapshot?.phase === "active" && Boolean(state.csrf);
    document.querySelector(".app-shell").dataset.phase = snapshot?.phase || "idle";
    ui["session-phase"].textContent = (snapshot?.phase || "idle").toUpperCase();
    ui["session-id"].textContent = snapshot?.id || "—";
    ui["egress-readout"].textContent = (snapshot?.egress || "direct").toUpperCase();
    ui["control-readout"].textContent = snapshot?.controller_attached ? "LEASED" : "DETACHED";
    ui.omnibox.value = snapshot?.url || "";
    ui["viewport-empty"].hidden = active;
    renderViewportTitle(snapshot?.phase === "failed");
    ui["viewport-copy"].textContent = snapshot?.failure || "Khởi tạo một WebView duy nhất. Cookie, cache, history và token sẽ nằm trong tmpfs.";
    ui["start-button"].hidden = snapshot?.phase === "burning" || active;
    ui["profile-readout"].textContent = (snapshot?.stream_profile || "1080p30").replace("p", "P / ");
    const autoBurn = snapshot?.auto_burn_seconds ?? 1800;
    ui["autoburn-readout"].textContent = autoBurn === 0 ? "OFF" : `${Math.floor(autoBurn / 60)}:${String(autoBurn % 60).padStart(2, "0")}`;
    ui["blocklist-label"].textContent = snapshot?.blocklist_enabled === false ? "BLOCKLIST OFF" : "BLOCKLIST ON";
    ui["blocklist-toggle"].classList.toggle("active", snapshot?.blocklist_enabled !== false);
    ui["blocklist-toggle"].setAttribute("aria-pressed", String(snapshot?.blocklist_enabled !== false));
    document.querySelectorAll("[data-profile]").forEach((button) => {
      const selected = button.dataset.profile === snapshot?.stream_profile;
      button.classList.toggle("active", selected);
      button.setAttribute("aria-pressed", String(selected));
    });
    document.querySelectorAll("[data-egress]").forEach((button) => {
      const selected = button.dataset.egress === snapshot?.egress;
      button.classList.toggle("active", selected);
      button.setAttribute("aria-pressed", String(selected));
    });
    document.querySelectorAll("[data-autoburn]").forEach((button) => {
      const selected = Number(button.dataset.autoburn) === autoBurn;
      button.classList.toggle("active", selected);
      button.setAttribute("aria-pressed", String(selected));
    });
    document.querySelectorAll(sessionControlSelector).forEach((button) => { button.disabled = state.busy || !active; });
    renderHistory(history);
    if (active) connectEvents().catch(handleActionError);
  }

  function renderHistory(history) {
    ui["history-list"].replaceChildren();
    if (!history.length) {
      const item = document.createElement("li");
      item.textContent = "Không có lịch sử phiên";
      ui["history-list"].append(item);
      return;
    }
    history.slice(-8).reverse().forEach((entry) => {
      const item = document.createElement("li");
      item.textContent = entry.url;
      item.title = entry.url;
      ui["history-list"].append(item);
    });
  }

  async function refreshSession() {
    const payload = await request("/api/v1/session");
    renderSession(payload.session, payload.history);
  }

  function normalizePairingSecret(raw) {
    const trimmed = raw.trim();
    if (!/\s/.test(trimmed)) return trimmed;
    const groups = trimmed.toUpperCase().split(/\s+/);
    return groups.every((group) => /^[A-Z2-7]{4}$/.test(group)) ? groups.join("-") : trimmed;
  }

  async function syncAfterPairing(attempt = 0) {
    try {
      await refreshSession();
      startMetricPolling();
    } catch (error) {
      if (isAuthError(error)) {
        lockAuthentication("PAIRING ACCEPTED — SESSION AUTH FAILED");
        return;
      }
      if (attempt < 2 && state.csrf) {
        setMetricsStatus("SYNC RETRY", "syncing");
        toast(`PAIRING ACCEPTED — RETRYING SESSION SYNC ${attempt + 1}/2`, true);
        await new Promise((resolve) => window.setTimeout(resolve, 500 * (2 ** attempt)));
        await syncAfterPairing(attempt + 1);
        return;
      }
      renderSession({ phase: "idle", egress: "direct", stream_profile: "1080p30", controller_attached: false });
      setMetricsStatus("STALE", "stale");
      toast(`PAIRING ACCEPTED — SESSION SYNC FAILED: ${error.message}`, true);
    }
  }

  async function pair(secret) {
    const payload = await request("/api/v1/pair/exchange", mutation("POST", { secret: normalizePairingSecret(secret) }));
    state.csrf = payload.csrf_token;
    ui["pairing-error"].textContent = "";
    ui["pairing-secret"].removeAttribute("aria-invalid");
    ui["pairing-secret"].value = "";
    ui["pairing-dialog"].close();
    toast("CONTROL LEASE READY");
    await syncAfterPairing();
  }

  async function connectEvents() {
    if (!state.session?.id || !state.csrf) return;
    const sessionId = state.session.id;
    if (state.eventsConnectingSessionId === sessionId) return;
    if (state.socketSessionId === sessionId && (state.socket?.readyState === WebSocket.CONNECTING || state.socket?.readyState === WebSocket.OPEN)) return;
    if (state.socket && state.socketSessionId !== sessionId) stopEventChannel();
    state.eventsConnectingSessionId = sessionId;
    try {
      const { ticket } = await request("/api/v1/webrtc/ticket", mutation("POST", { purpose: "events" }));
      if (!state.csrf || state.session?.id !== sessionId) return;
      const scheme = location.protocol === "https:" ? "wss:" : "ws:";
      const socket = new WebSocket(`${scheme}//${location.host}/ws/v1/session/${sessionId}/events`);
      state.socket = socket;
      state.socketSessionId = sessionId;
      socket.addEventListener("open", () => socket.send(JSON.stringify({ type: "authenticate", ticket })));
      socket.addEventListener("message", (event) => {
        if (state.socket !== socket || state.socketSessionId !== sessionId || state.session?.id !== sessionId) return;
        const payload = JSON.parse(event.data);
        if (payload.type === "authenticated") {
          state.eventsReconnectAttempts = 0;
          toast("EVENT CHANNEL SECURE");
          return;
        }
        if (payload.error) {
          if (payload.error.code === "AUTH_REQUIRED") {
            lockAuthentication();
            return;
          }
          toast(payload.error.code || "EVENT CHANNEL REJECTED", true);
          return;
        }
        if (payload.event === "session.idle" || payload.event === "session.burn_failed") {
          lockAuthentication("SESSION ENDED — PAIR AGAIN");
          return;
        }
        if (payload.session?.id === sessionId) renderSession(payload.session);
      });
      socket.addEventListener("close", () => {
        if (state.socket !== socket) return;
        state.socket = null;
        state.socketSessionId = null;
        scheduleEventReconnect(sessionId);
      });
    } catch (error) {
      if (!state.csrf || state.session?.id !== sessionId) return;
      scheduleEventReconnect(sessionId);
      throw error;
    } finally {
      if (state.eventsConnectingSessionId === sessionId) state.eventsConnectingSessionId = null;
      if (state.csrf && state.session?.id && state.session.id !== sessionId && !state.socket) {
        connectEvents().catch(handleActionError);
      }
    }
  }

  async function startSession() {
    setBusy(true);
    try {
      const snapshot = await request("/api/v1/session", mutation("POST", {}));
      renderSession(snapshot);
      toast("SESSION ACTIVE");
    } finally { setBusy(false); }
  }

  async function navigate(command) {
    if (!state.session?.id) return;
    setBusy(true);
    try {
      const snapshot = await request(`/api/v1/session/${state.session.id}/navigation`, mutation("POST", command));
      renderSession(snapshot);
    } finally { setBusy(false); }
  }

  function openBurnDialog() {
    if (!state.session?.id) return;
    ui["burn-session-id"].textContent = state.session.id;
    ui["burn-error"].textContent = "";
    ui["burn-dialog"].showModal();
    window.requestAnimationFrame(() => ui["burn-cancel"].focus());
  }

  async function burn() {
    if (!state.session?.id) return;
    ui["burn-form"].setAttribute("aria-busy", "true");
    ui["burn-error"].textContent = "";
    setBusy(true);
    try {
      await request(`/api/v1/session/${state.session.id}`, mutation("DELETE"));
      state.csrf = null;
      stopEventChannel();
      stopMetricPolling("LOCKED", "locked");
      renderSession({ phase: "idle", egress: "direct", stream_profile: "1080p30", controller_attached: false });
      toast("SESSION BURNED — AUTH REVOKED", true);
      ui["burn-dialog"].close();
      ui["pairing-secret"].value = "";
      showPairingDialog();
    } finally {
      ui["burn-form"].setAttribute("aria-busy", "false");
      setBusy(false);
    }
  }

  async function setEgress(mode) {
    if (!state.session?.id) return;
    setBusy(true);
    ui["egress-state"].textContent = "SWITCHING";
    try {
      const snapshot = await request(`/api/v1/session/${state.session.id}/egress`, mutation("PUT", { mode }));
      renderSession(snapshot);
      toast(`${mode} EGRESS ACTIVE`);
    } catch (error) {
      try {
        await refreshSession();
      } catch (syncError) {
        if (isAuthError(syncError)) lockAuthentication();
        else toast(`EGRESS STATE SYNC FAILED: ${syncError.message}`, true);
      }
      throw error;
    } finally { setBusy(false); ui["egress-state"].textContent = "READY"; }
  }

  async function setProfile(profile) {
    if (!state.session?.id) return;
    setBusy(true);
    try {
      const snapshot = await request(`/api/v1/session/${state.session.id}/stream-profile`, mutation("PUT", { profile }));
      renderSession(snapshot);
      toast(`${profile} SELECTED`);
    } finally { setBusy(false); }
  }

  async function setAutoBurn(seconds) {
    if (!state.session?.id) return;
    setBusy(true);
    try {
      const snapshot = await request(`/api/v1/session/${state.session.id}/auto-burn`, mutation("PUT", { seconds }));
      renderSession(snapshot);
      toast(seconds === 0 ? "AUTO-BURN DISABLED FOR SESSION" : `AUTO-BURN ${seconds / 60}M`);
    } finally { setBusy(false); }
  }

  async function toggleBlocklist() {
    if (!state.session?.id) return;
    const enabled = state.session?.blocklist_enabled === false;
    setBusy(true);
    try {
      const snapshot = await request(`/api/v1/session/${state.session.id}/blocklist`, mutation("PUT", { enabled }));
      renderSession(snapshot);
      toast(`BLOCKLIST ${enabled ? "ENABLED" : "DISABLED"}`);
    } finally { setBusy(false); }
  }

  async function refreshMetrics() {
    if (!state.csrf) return;
    const sequence = state.metricsSequence + 1;
    state.metricsSequence = sequence;
    state.metricsController?.abort();
    const controller = new AbortController();
    state.metricsController = controller;
    const timeout = window.setTimeout(() => controller.abort(), 2500);
    try {
      const data = await request("/api/v1/metrics", { signal: controller.signal });
      if (sequence !== state.metricsSequence || !state.csrf) return;
      ui["metric-memory"].textContent = data.memory_available_mib ?? "—";
      ui["metric-temp"].textContent = data.temperature_celsius?.toFixed(1) ?? "—";
      ui["metric-bitrate"].textContent = data.stream.bitrate_kbps ?? "—";
      ui["metric-loss"].textContent = data.stream.packet_loss_percent?.toFixed(2) ?? "—";
      ui["blocked-count"].textContent = `${data.blocked_requests} BLOCKED`;
      ui["stream-readout"].textContent = data.stream.connected ? `${data.stream.fps.toFixed(1)} FPS` : "NO SIGNAL";
      ui["service-state"].title = `WPE ${data.versions.webkit_engine} / GST ${data.versions.gstreamer} / RSWEBRTC ${data.versions.rswebrtc}`;
      const sampledAt = new Date(data.sampled_at);
      const sampleIsFresh = !Number.isNaN(sampledAt.getTime()) && Math.abs(Date.now() - sampledAt.getTime()) <= 15000;
      state.lastMetricsAt = sampleIsFresh ? Date.now() : null;
      setMetricsStatus(
        sampleIsFresh ? `LIVE · ${sampledAt.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}` : "STALE",
        sampleIsFresh ? "live" : "stale",
      );
    } catch (error) {
      if (sequence !== state.metricsSequence) return;
      if (isAuthError(error)) {
        lockAuthentication();
        return;
      }
      state.lastMetricsAt = null;
      setMetricsStatus("STALE", "stale");
    } finally {
      window.clearTimeout(timeout);
      if (state.metricsController === controller) state.metricsController = null;
    }
  }

  function startMetricPolling() {
    stopMetricPolling();
    setMetricsStatus("SYNCING", "syncing");
    refreshMetrics();
    state.metricsTimer = window.setInterval(refreshMetrics, 3000);
    state.metricsWatchdog = window.setInterval(() => {
      if (state.csrf && state.lastMetricsAt && Date.now() - state.lastMetricsAt > 7000) {
        setMetricsStatus("STALE", "stale");
      }
    }, 1000);
  }

  function wireControls() {
    ui["pairing-dialog"].addEventListener("cancel", (event) => event.preventDefault());
    ui["pairing-form"].addEventListener("submit", async (event) => {
      event.preventDefault();
      if (state.pairingInFlight) return;
      state.pairingInFlight = true;
      ui["pairing-form"].setAttribute("aria-busy", "true");
      ui["pairing-submit"].disabled = true;
      try {
        await pair(ui["pairing-secret"].value);
      } catch (error) {
        ui["pairing-secret"].setAttribute("aria-invalid", "true");
        ui["pairing-error"].textContent = error.message;
        ui["pairing-secret"].focus();
      } finally {
        state.pairingInFlight = false;
        ui["pairing-form"].setAttribute("aria-busy", "false");
        ui["pairing-submit"].disabled = false;
      }
    });
    ui["start-button"].addEventListener("click", () => startSession().catch(handleActionError));
    ui["burn-button"].addEventListener("click", openBurnDialog);
    ui["burn-confirm"].addEventListener("click", () => burn().catch((error) => {
      ui["burn-error"].textContent = error.message;
      handleActionError(error);
      window.requestAnimationFrame(() => ui["burn-cancel"].focus());
    }));
    ui["omnibox-form"].addEventListener("submit", (event) => {
      event.preventDefault();
      const raw = ui.omnibox.value.trim();
      if (!raw) return;
      const url = /^[a-z][a-z0-9+.-]*:/i.test(raw) ? raw : raw.includes(".") ? `https://${raw}` : `https://duckduckgo.com/?q=${encodeURIComponent(raw)}`;
      navigate({ navigate: { url } }).catch(handleActionError);
    });
    document.querySelectorAll("[data-nav]").forEach((button) => button.addEventListener("click", () => navigate(button.dataset.nav).catch(handleActionError)));
    document.querySelectorAll("[data-egress]").forEach((button) => button.addEventListener("click", () => setEgress(button.dataset.egress).catch(handleActionError)));
    document.querySelectorAll("[data-profile]").forEach((button) => button.addEventListener("click", () => setProfile(button.dataset.profile).catch(handleActionError)));
    document.querySelectorAll("[data-autoburn]").forEach((button) => button.addEventListener("click", () => setAutoBurn(Number(button.dataset.autoburn)).catch(handleActionError)));
    ui["blocklist-toggle"].addEventListener("click", () => toggleBlocklist().catch(handleActionError));
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState !== "visible" || !state.csrf) return;
      if (!state.lastMetricsAt || Date.now() - state.lastMetricsAt > 7000) setMetricsStatus("STALE", "stale");
      refreshMetrics();
    });
  }

  async function boot() {
    wireControls();
    renderSession({ phase: "idle", egress: "direct", stream_profile: "1080p30", controller_attached: false });
    try {
      const status = await request("/api/v1/status");
      ui["service-pulse"].classList.add("online");
      ui["service-state"].textContent = `ONLINE / ${status.version}`;
      ui["viewport-kicker"].textContent = "CONTROL PLANE ONLINE";
      ui["session-phase"].textContent = status.phase.toUpperCase();
    } catch (error) {
      ui["viewport-kicker"].textContent = "CONTROL PLANE OFFLINE";
      toast(error.message, true);
      showPairingDialog();
      return;
    }

    const fragment = new URLSearchParams(location.hash.slice(1));
    const secret = fragment.get("pair");
    if (!secret) {
      showPairingDialog();
      return;
    }
    history.replaceState(null, "", `${location.pathname}${location.search}`);
    try {
      await pair(secret);
    } catch (error) {
      ui["pairing-secret"].setAttribute("aria-invalid", "true");
      ui["pairing-error"].textContent = error.message;
      toast(error.message, true);
      showPairingDialog();
    }
  }

  boot();
})();
