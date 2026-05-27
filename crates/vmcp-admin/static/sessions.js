// Sessions UI: client/session table + per-session dump viewer with live SSE.
// All driven by /admin/api/sessions/* (part C).

(function () {
  "use strict";

  let activeStream = null;
  let pollTimer = null;

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );
  }

  function fmtTime(ms) {
    if (!ms) return "—";
    try {
      return new Date(ms).toLocaleString();
    } catch (e) {
      return String(ms);
    }
  }

  // Tabler `.status` looks like a light-tint pill with a colored dot — readable
  // at body font-size, unlike `.badge` which collapses to ~8px when `<code>` is
  // nested inside. Active sessions get the animated pulsing dot.
  function stateBadge(state) {
    const label = state === "pre_registered" ? "pre-registered" : (state || "—");
    const color = state === "active"
      ? "green"
      : state === "pre_registered"
        ? "blue"
        : state === "idle"
          ? "yellow"
          : "secondary";
    const dot = state === "active" ? "status-dot status-dot-animated" : "status-dot";
    return `<span class="status status-${color}"><span class="${dot}"></span>${escapeHtml(label)}</span>`;
  }

  // /mcp (lime, GraphQL semantic) vs /mcp-proxy (purple, transparent passthrough)
  // — jump out side-by-side. Unknown / not-yet-tagged sessions get a dim "—" so
  // the column doesn't collapse during the very first request before meta is
  // flushed.
  function endpointBadge(upstream) {
    if (!upstream) return '<span class="text-secondary">—</span>';
    const color = upstream === "/mcp-proxy" ? "purple" : "lime";
    return `<span class="status status-${color}"><span class="status-dot"></span><span class="font-monospace">${escapeHtml(upstream)}</span></span>`;
  }

  function shortId(id) {
    if (!id) return "—";
    const head = id.length > 20 ? id.slice(0, 18) + "…" : id;
    return `<code class="text-secondary" title="${escapeHtml(id)}" style="font-size:0.8em">${escapeHtml(head)}</code>`;
  }

  async function refreshClients() {
    const root = document.getElementById("sessions-grid");
    if (!root) return;
    try {
      const res = await fetch("/admin/api/sessions");
      if (!res.ok) {
        root.innerHTML =
          '<p class="text-danger">API error: HTTP ' + res.status + "</p>";
        return;
      }
      const { clients } = await res.json();
      if (!clients || !clients.length) {
        root.innerHTML =
          '<p class="text-muted"><em>No registered clients or sessions yet.</em></p>';
        return;
      }
      root.innerHTML =
        '<div class="card"><div class="table-responsive"><table class="table table-vcenter table-hover card-table">' +
        '<thead><tr>' +
        '<th>Client</th><th>Endpoint</th><th>State</th><th class="text-end">Sessions</th><th>Registered</th><th>Last seen</th><th>ID</th>' +
        "</tr></thead><tbody>" +
        clients
          .map((c) => {
            const lastSeen = c.sessions && c.sessions.length ? c.sessions[0].last_seen_ms : null;
            const upstream = (c.sessions || []).find((s) => s.upstream)?.upstream || null;
            const nameCell = c.client_name
              ? `<strong>${escapeHtml(c.client_name)}</strong>`
              : `<span class="text-secondary fst-italic">(unnamed)</span>`;
            return `<tr data-cid="${escapeHtml(c.client_id)}" style="cursor:pointer">
              <td>${nameCell}</td>
              <td>${endpointBadge(upstream)}</td>
              <td>${stateBadge(c.state)}</td>
              <td class="text-end"><span class="badge bg-secondary-lt">${(c.sessions || []).length}</span></td>
              <td class="text-secondary">${fmtTime(c.registered_at_ms)}</td>
              <td class="text-secondary">${fmtTime(lastSeen)}</td>
              <td>${shortId(c.client_id)}</td>
            </tr>`;
          })
          .join("") +
        "</tbody></table></div></div>";
      root.querySelectorAll("tr[data-cid]").forEach((tr) => {
        tr.addEventListener("click", () => {
          const cid = tr.dataset.cid;
          const c = clients.find((x) => x.client_id === cid);
          openClient(cid, c);
        });
      });
    } catch (e) {
      root.innerHTML =
        '<p class="text-danger">Fetch failed: ' + escapeHtml(String(e)) + "</p>";
    }
  }

  async function openClient(cid, client) {
    const panel = document.getElementById("session-detail");
    if (!panel) return;
    panel.classList.remove("d-none");
    const sessions = (client && client.sessions) || [];
    const sid = sessions.length ? sessions[0].id : null;
    const upstream = sessions.find((s) => s.upstream)?.upstream || null;
    const titleEl = document.getElementById("session-detail-title");
    const namePart = (client && client.client_name) || cid;
    titleEl.innerHTML =
      escapeHtml(namePart) + " " + endpointBadge(upstream);
    if (!sid) {
      document.getElementById("dump-grid").textContent = "No sessions yet";
      document.getElementById("export-link").href = "#";
      const cmp = document.getElementById("compare-link");
      if (cmp) cmp.href = "#";
      return;
    }
    document.getElementById(
      "export-link"
    ).href = `/admin/api/sessions/${encodeURIComponent(cid)}/dump.jsonl?session_id=${encodeURIComponent(sid)}`;
    const cmp = document.getElementById("compare-link");
    if (cmp) {
      cmp.href = `/admin/compare?a=${encodeURIComponent(cid)}:${encodeURIComponent(sid)}`;
    }
    const btn = document.getElementById("live-toggle");
    btn.dataset.cid = cid;
    btn.dataset.sid = sid;
    // Kill any prior stream when switching clients.
    if (activeStream) {
      activeStream.close();
      activeStream = null;
      btn.textContent = "▶ Resume live";
    }
    try {
      const res = await fetch(
        `/admin/api/sessions/${encodeURIComponent(cid)}/dump?session_id=${encodeURIComponent(sid)}&limit=100`
      );
      const { exchanges } = await res.json();
      window.__lastExchanges = exchanges || [];
      renderDump(window.__lastExchanges);
    } catch (e) {
      document.getElementById("dump-grid").innerHTML =
        '<p class="text-danger">Failed to load dump: ' + escapeHtml(String(e)) + "</p>";
    }
  }

  function renderDump(exchanges) {
    const grid = document.getElementById("dump-grid");
    if (!grid) return;
    if (!exchanges || !exchanges.length) {
      grid.innerHTML = '<p class="text-muted"><em>No exchanges yet.</em></p>';
      return;
    }
    grid.innerHTML =
      '<table class="table table-sm table-hover"><thead><tr>' +
      "<th>Time</th><th>Dir</th><th>Method</th><th>ID</th><th>Latency</th>" +
      "</tr></thead><tbody>" +
      exchanges
        .map((e, i) => {
          const t = e.timestamp_ms
            ? new Date(e.timestamp_ms).toISOString().substr(11, 12)
            : "—";
          const arrow = e.direction === "C2S" ? "→" : "←";
          const method = e.method || "";
          const id = e.jsonrpc_id !== null && e.jsonrpc_id !== undefined
            ? JSON.stringify(e.jsonrpc_id)
            : "";
          const lat = e.latency_ms ? e.latency_ms + "ms" : "";
          return `<tr data-idx="${i}" style="cursor:pointer">
            <td><code>${escapeHtml(t)}</code></td>
            <td>${arrow}</td>
            <td><code>${escapeHtml(method)}</code></td>
            <td><code>${escapeHtml(id)}</code></td>
            <td>${escapeHtml(lat)}</td>
          </tr>`;
        })
        .join("") +
      "</tbody></table>";
    grid.querySelectorAll("tr[data-idx]").forEach((tr) => {
      tr.addEventListener("click", () => showExchange(parseInt(tr.dataset.idx, 10)));
    });
  }

  function showExchange(idx) {
    const all = window.__lastExchanges || [];
    const ex = all[idx];
    if (!ex) return;
    // Pair lookup: same jsonrpc_id, opposite direction.
    const pair = all.find(
      (e) =>
        e.jsonrpc_id !== null &&
        e.jsonrpc_id !== undefined &&
        JSON.stringify(e.jsonrpc_id) === JSON.stringify(ex.jsonrpc_id) &&
        e.direction !== ex.direction
    );
    const [req, resp] = ex.direction === "C2S" ? [ex, pair] : [pair, ex];
    document.getElementById("req-panel").textContent = req
      ? JSON.stringify(req.body, null, 2)
      : "—";
    document.getElementById("resp-panel").textContent = resp
      ? JSON.stringify(resp.body, null, 2)
      : "(no response yet)";
  }

  function setupLiveToggle() {
    const btn = document.getElementById("live-toggle");
    if (!btn) return;
    btn.addEventListener("click", () => {
      if (activeStream) {
        activeStream.close();
        activeStream = null;
        btn.textContent = "▶ Resume live";
        return;
      }
      const cid = btn.dataset.cid;
      const sid = btn.dataset.sid;
      if (!cid || !sid) return;
      activeStream = new EventSource(
        `/admin/api/sessions/${encodeURIComponent(cid)}/dump/stream?session_id=${encodeURIComponent(sid)}&prefill=0`
      );
      activeStream.onmessage = (ev) => {
        try {
          const ex = JSON.parse(ev.data);
          window.__lastExchanges = window.__lastExchanges || [];
          window.__lastExchanges.push(ex);
          renderDump(window.__lastExchanges);
        } catch (e) {
          console.warn("bad SSE payload", e);
        }
      };
      activeStream.addEventListener("lag", (ev) => {
        console.warn("SSE lag:", ev.data);
      });
      activeStream.onerror = () => {
        // Browser auto-reconnects; nothing to do here.
      };
      btn.textContent = "⏸ Pause";
    });
  }

  document.addEventListener("DOMContentLoaded", () => {
    setupLiveToggle();
    refreshClients();
    pollTimer = setInterval(refreshClients, 5000);
  });
})();
