(() => {
  "use strict";

  const ui = Object.fromEntries([...document.querySelectorAll("[id]")].map((node) => [node.id, node]));
  const state = { csrf: null, session: null, socket: null, burnArmedUntil: 0, metricsTimer: null };

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
    if (!response.ok) throw new Error(payload?.error?.message || `HTTP ${response.status}`);
    return payload;
  }

  function toast(message, danger = false) {
    ui.toast.textContent = message.toUpperCase();
    ui.toast.style.color = danger ? "var(--orange)" : "var(--green)";
  }

  function setBusy(busy) {
    document.querySelector(".app-shell").setAttribute("aria-busy", String(busy));
    const active = state.session?.phase === "active" && Boolean(state.csrf);
    document.querySelectorAll("button").forEach((button) => {
      const needsSession = button.matches("[data-nav], [data-profile], [data-egress], #burn-button, #omnibox-form button");
      button.disabled = busy || (needsSession && !active);
    });
    if (busy) toast("APPLYING TRANSACTION");
  }

  function renderSession(snapshot, history = []) {
    state.session = snapshot;
    const active = snapshot?.phase === "active" && Boolean(state.csrf);
    ui["session-phase"].textContent = (snapshot?.phase || "idle").toUpperCase();
    ui["session-id"].textContent = snapshot?.id || "—";
    ui["egress-readout"].textContent = (snapshot?.egress || "direct").toUpperCase();
    ui["control-readout"].textContent = snapshot?.controller_attached ? "LEASED" : "DETACHED";
    ui.omnibox.value = snapshot?.url || "";
    ui["viewport-empty"].hidden = active;
    ui["viewport-title"].textContent = snapshot?.phase === "failed" ? "Phiên cần được burn" : "Chưa có phiên hoạt động";
    ui["viewport-copy"].textContent = snapshot?.failure || "Khởi tạo một WebView duy nhất. Cookie, cache, history và token sẽ nằm trong tmpfs.";
    ui["start-button"].hidden = snapshot?.phase === "burning" || active;
    ui["profile-readout"].textContent = (snapshot?.stream_profile || "1080p30").replace("p", "P / ");
    document.querySelectorAll("[data-profile]").forEach((button) => button.classList.toggle("active", button.dataset.profile === snapshot?.stream_profile));
    document.querySelectorAll("[data-egress]").forEach((button) => button.classList.toggle("active", button.dataset.egress === snapshot?.egress));
    document.querySelectorAll("[data-nav], [data-profile], [data-egress], #burn-button, #omnibox-form button").forEach((button) => { button.disabled = !active; });
    renderHistory(history);
    if (active) connectEvents().catch((error) => toast(error.message, true));
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

  async function pair(secret) {
    const payload = await request("/api/v1/pair/exchange", mutation("POST", { secret }));
    state.csrf = payload.csrf_token;
    ui["pairing-error"].textContent = "";
    ui["pairing-dialog"].close();
    toast("CONTROL LEASE READY");
    await refreshSession();
    startMetricPolling();
  }

  async function connectEvents() {
    if (!state.session?.id || state.socket?.readyState === WebSocket.OPEN || !state.csrf) return;
    const { ticket } = await request("/api/v1/webrtc/ticket", mutation("POST", {}));
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(`${scheme}//${location.host}/ws/v1/session/${state.session.id}/events?ticket=${encodeURIComponent(ticket)}`);
    state.socket = socket;
    socket.addEventListener("open", () => toast("EVENT CHANNEL SECURE"));
    socket.addEventListener("message", (event) => {
      const payload = JSON.parse(event.data);
      if (payload.session) renderSession(payload.session);
    });
    socket.addEventListener("close", () => { if (state.socket === socket) state.socket = null; });
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
    const snapshot = await request(`/api/v1/session/${state.session.id}/navigation`, mutation("POST", command));
    renderSession(snapshot);
  }

  async function burn() {
    if (!state.session?.id) return;
    if (Date.now() > state.burnArmedUntil) {
      state.burnArmedUntil = Date.now() + 5000;
      ui["burn-button"].classList.add("armed");
      ui["burn-button"].textContent = "CONFIRM BURN";
      toast("BURN ARMED — PRESS AGAIN", true);
      window.setTimeout(() => {
        if (Date.now() > state.burnArmedUntil) {
          ui["burn-button"].classList.remove("armed");
          ui["burn-button"].textContent = "BURN SESSION";
        }
      }, 5100);
      return;
    }
    setBusy(true);
    try {
      await request(`/api/v1/session/${state.session.id}`, mutation("DELETE"));
      state.csrf = null;
      state.socket?.close();
      renderSession({ phase: "idle", egress: "direct", stream_profile: "1080p30", controller_attached: false });
      toast("SESSION BURNED — AUTH REVOKED", true);
      ui["pairing-dialog"].showModal();
    } finally {
      setBusy(false);
      ui["burn-button"].classList.remove("armed");
      ui["burn-button"].textContent = "BURN SESSION";
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
    } finally { setBusy(false); ui["egress-state"].textContent = "READY"; }
  }

  async function setProfile(profile) {
    if (!state.session?.id) return;
    const snapshot = await request(`/api/v1/session/${state.session.id}/stream-profile`, mutation("PUT", { profile }));
    renderSession(snapshot);
    toast(`${profile} SELECTED`);
  }

  async function refreshMetrics() {
    if (!state.csrf) return;
    try {
      const data = await request("/api/v1/metrics");
      ui["metric-memory"].textContent = data.memory_available_mib || "—";
      ui["metric-temp"].textContent = data.temperature_celsius?.toFixed(1) || "—";
      ui["metric-bitrate"].textContent = data.stream.bitrate_kbps || "—";
      ui["metric-loss"].textContent = data.stream.packet_loss_percent?.toFixed(2) || "—";
      ui["blocked-count"].textContent = `${data.blocked_requests} BLOCKED`;
      ui["stream-readout"].textContent = data.stream.connected ? `${data.stream.fps.toFixed(1)} FPS` : "NO SIGNAL";
    } catch (error) {
      if (/unauthorized/i.test(error.message)) state.csrf = null;
    }
  }

  function startMetricPolling() {
    window.clearInterval(state.metricsTimer);
    refreshMetrics();
    state.metricsTimer = window.setInterval(refreshMetrics, 3000);
  }

  function wireControls() {
    ui["pairing-form"].addEventListener("submit", (event) => {
      event.preventDefault();
      pair(ui["pairing-secret"].value.trim()).catch((error) => { ui["pairing-error"].textContent = error.message; });
    });
    ui["start-button"].addEventListener("click", () => startSession().catch((error) => toast(error.message, true)));
    ui["burn-button"].addEventListener("click", () => burn().catch((error) => toast(error.message, true)));
    ui["omnibox-form"].addEventListener("submit", (event) => {
      event.preventDefault();
      const raw = ui.omnibox.value.trim();
      if (!raw) return;
      const url = /^[a-z][a-z0-9+.-]*:/i.test(raw) ? raw : raw.includes(".") ? `https://${raw}` : `https://duckduckgo.com/?q=${encodeURIComponent(raw)}`;
      navigate({ navigate: { url } }).catch((error) => toast(error.message, true));
    });
    document.querySelectorAll("[data-nav]").forEach((button) => button.addEventListener("click", () => navigate(button.dataset.nav).catch((error) => toast(error.message, true))));
    document.querySelectorAll("[data-egress]").forEach((button) => button.addEventListener("click", () => setEgress(button.dataset.egress).catch((error) => toast(error.message, true))));
    document.querySelectorAll("[data-profile]").forEach((button) => button.addEventListener("click", () => setProfile(button.dataset.profile).catch((error) => toast(error.message, true))));
  }

  async function boot() {
    wireControls();
    renderSession({ phase: "idle", egress: "direct", stream_profile: "1080p30", controller_attached: false });
    try {
      const status = await request("/api/v1/status");
      ui["service-pulse"].classList.add("online");
      ui["service-state"].textContent = `ONLINE / ${status.version}`;
      ui["session-phase"].textContent = status.phase.toUpperCase();
      const fragment = new URLSearchParams(location.hash.slice(1));
      const secret = fragment.get("pair");
      if (secret) {
        history.replaceState(null, "", `${location.pathname}${location.search}`);
        await pair(secret);
      } else {
        ui["pairing-dialog"].showModal();
      }
    } catch (error) {
      toast(error.message, true);
      ui["pairing-dialog"].showModal();
    }
  }

  boot();
})();
