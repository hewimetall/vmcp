// Compare two MCP sessions side-by-side. Reads ?a=cid:sid&b=cid:sid from the
// URL; if missing, picks the two most recently active sessions for each
// vmcp endpoint (/mcp vs /mcp-proxy) so the demo "click and it just works".
//
// Renders two exchange tables and a summary strip (round-trip count, total
// bytes, distinct methods). Clicking a row in either table opens the
// request/response pair below.

(function () {
  "use strict";

  let allSessions = []; // flat [{cid, client_name, sid, started, upstream, ...}]
  let aExchanges = [];
  let bExchanges = [];

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );
  }

  function endpointBadge(upstream) {
    if (!upstream) return '<span class="text-secondary">—</span>';
    const color = upstream === "/mcp-proxy" ? "purple" : "lime";
    return `<span class="status status-${color}"><span class="status-dot"></span><span class="font-monospace">${escapeHtml(upstream)}</span></span>`;
  }

  function parsePair(s) {
    if (!s) return null;
    const i = s.indexOf(":");
    if (i < 0) return null;
    return { cid: s.slice(0, i), sid: s.slice(i + 1) };
  }

  function getQuery() {
    const u = new URL(window.location.href);
    return {
      a: parsePair(u.searchParams.get("a")),
      b: parsePair(u.searchParams.get("b")),
    };
  }

  async function fetchSessionsIndex() {
    const r = await fetch("/admin/api/sessions");
    if (!r.ok) throw new Error("sessions index: HTTP " + r.status);
    const { clients } = await r.json();
    const flat = [];
    for (const c of clients || []) {
      for (const s of c.sessions || []) {
        flat.push({
          cid: c.client_id,
          client_name: c.client_name || "—",
          sid: s.id,
          started: s.started_at_ms,
          last_seen: s.last_seen_ms,
          request_count: s.request_count || 0,
          upstream: s.upstream || null,
        });
      }
    }
    // Most recent first.
    flat.sort((x, y) => (y.last_seen || 0) - (x.last_seen || 0));
    return flat;
  }

  function populatePicker(selectEl, sessions, defaultKey) {
    selectEl.innerHTML = sessions
      .map((s) => {
        const key = `${s.cid}:${s.sid}`;
        const sel = defaultKey === key ? " selected" : "";
        const label =
          `${s.upstream || "?"} · ${s.client_name} · ${s.sid.slice(0, 10)} · ${s.request_count} req`;
        return `<option value="${escapeHtml(key)}"${sel}>${escapeHtml(label)}</option>`;
      })
      .join("");
  }

  function autoPickDefaults(sessions, query) {
    // Honor explicit ?a / ?b first.
    let a = query.a ? `${query.a.cid}:${query.a.sid}` : null;
    let b = query.b ? `${query.b.cid}:${query.b.sid}` : null;

    if (!a || !b) {
      // Demo magic: grab the most recent /mcp and /mcp-proxy session
      // respectively. Falls back to the two most recent overall if either
      // upstream tag is missing.
      const byMcp = sessions.find((s) => s.upstream === "/mcp");
      const byProxy = sessions.find((s) => s.upstream === "/mcp-proxy");
      if (!a) {
        if (byMcp) a = `${byMcp.cid}:${byMcp.sid}`;
        else if (sessions[0]) a = `${sessions[0].cid}:${sessions[0].sid}`;
      }
      if (!b) {
        if (byProxy) b = `${byProxy.cid}:${byProxy.sid}`;
        else if (sessions[1]) b = `${sessions[1].cid}:${sessions[1].sid}`;
        else if (sessions[0] && a !== `${sessions[0].cid}:${sessions[0].sid}`) {
          b = `${sessions[0].cid}:${sessions[0].sid}`;
        }
      }
    }
    return { a, b };
  }

  async function fetchDump(cid, sid) {
    const r = await fetch(
      `/admin/api/sessions/${encodeURIComponent(cid)}/dump?session_id=${encodeURIComponent(sid)}&limit=500`
    );
    if (!r.ok) throw new Error("dump fetch: HTTP " + r.status);
    const { exchanges } = await r.json();
    return exchanges || [];
  }

  function bodyBytes(ex) {
    try {
      return JSON.stringify(ex.body).length;
    } catch (e) {
      return 0;
    }
  }

  function summarize(exs) {
    let c2s = 0;
    let s2c = 0;
    let bytes = 0;
    let requests = 0;
    const methods = new Set();
    for (const e of exs) {
      bytes += bodyBytes(e);
      if (e.direction === "C2S") c2s++;
      else s2c++;
      if (e.kind === "Request") requests++;
      if (e.method) methods.add(e.method);
    }
    return {
      total: exs.length,
      requests,
      c2s,
      s2c,
      bytes,
      methods: [...methods],
      upstream: exs.find((e) => e.upstream)?.upstream || null,
    };
  }

  function fmtBytes(n) {
    if (n < 1024) return n + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KiB";
    return (n / 1024 / 1024).toFixed(2) + " MiB";
  }

  function renderSummary(a, b) {
    const root = document.getElementById("summary");
    const sa = summarize(a);
    const sb = summarize(b);
    function cell(label, va, vb) {
      const cls = va === vb ? "" : "table-warning";
      return `<tr class="${cls}"><th>${escapeHtml(label)}</th><td>${escapeHtml(String(va))}</td><td>${escapeHtml(String(vb))}</td></tr>`;
    }
    root.innerHTML = `
      <div class="col-12">
        <table class="table table-sm table-bordered mb-0">
          <thead><tr><th></th><th>Session A ${endpointBadge(sa.upstream)}</th><th>Session B ${endpointBadge(sb.upstream)}</th></tr></thead>
          <tbody>
            ${cell("Total exchanges", sa.total, sb.total)}
            ${cell("JSON-RPC requests", sa.requests, sb.requests)}
            ${cell("C2S frames", sa.c2s, sb.c2s)}
            ${cell("S2C frames", sa.s2c, sb.s2c)}
            ${cell("Body bytes (sum)", fmtBytes(sa.bytes), fmtBytes(sb.bytes))}
            ${cell("Distinct methods", sa.methods.length, sb.methods.length)}
            ${cell("Methods", sa.methods.join(", ") || "—", sb.methods.join(", ") || "—")}
          </tbody>
        </table>
      </div>
    `;
  }

  function renderGrid(gridId, exs, side) {
    const grid = document.getElementById(gridId);
    if (!exs.length) {
      grid.innerHTML = '<p class="text-muted"><em>No exchanges.</em></p>';
      return;
    }
    grid.innerHTML =
      '<table class="table table-sm table-hover mb-0"><thead><tr>' +
      "<th>#</th><th>Dir</th><th>Method</th><th>ID</th><th>Bytes</th><th>Latency</th>" +
      "</tr></thead><tbody>" +
      exs
        .map((e, i) => {
          const arrow = e.direction === "C2S" ? "→" : "←";
          const id = e.jsonrpc_id !== null && e.jsonrpc_id !== undefined ? JSON.stringify(e.jsonrpc_id) : "";
          const lat = e.latency_ms ? e.latency_ms + "ms" : "";
          return `<tr data-side="${side}" data-idx="${i}" style="cursor:pointer">
            <td>${i + 1}</td>
            <td>${arrow}</td>
            <td><code>${escapeHtml(e.method || "")}</code></td>
            <td><code>${escapeHtml(id)}</code></td>
            <td>${fmtBytes(bodyBytes(e))}</td>
            <td>${escapeHtml(lat)}</td>
          </tr>`;
        })
        .join("") +
      "</tbody></table>";
    grid.querySelectorAll("tr[data-idx]").forEach((tr) => {
      tr.addEventListener("click", () => showPair(side, parseInt(tr.dataset.idx, 10)));
    });
  }

  function showPair(side, idx) {
    const all = side === "a" ? aExchanges : bExchanges;
    const ex = all[idx];
    if (!ex) return;
    const pair = all.find(
      (e) =>
        e.jsonrpc_id !== null &&
        e.jsonrpc_id !== undefined &&
        JSON.stringify(e.jsonrpc_id) === JSON.stringify(ex.jsonrpc_id) &&
        e.direction !== ex.direction
    );
    const [req, resp] = ex.direction === "C2S" ? [ex, pair] : [pair, ex];
    document.getElementById("exchange-panels").style.display = "";
    document.getElementById("req-" + side).textContent = req ? JSON.stringify(req.body, null, 2) : "—";
    document.getElementById("resp-" + side).textContent = resp ? JSON.stringify(resp.body, null, 2) : "(no response yet)";
  }

  async function loadAndRender(aKey, bKey) {
    const status = document.getElementById("status");
    status.textContent = "loading…";
    try {
      const a = parsePair(aKey);
      const b = parsePair(bKey);
      if (!a || !b) {
        status.textContent = "pick two sessions";
        return;
      }
      const [exA, exB] = await Promise.all([
        fetchDump(a.cid, a.sid),
        fetchDump(b.cid, b.sid),
      ]);
      aExchanges = exA;
      bExchanges = exB;
      const meta = new Map(allSessions.map((s) => [`${s.cid}:${s.sid}`, s]));
      const ma = meta.get(aKey);
      const mb = meta.get(bKey);
      document.getElementById("title-a").innerHTML =
        "A · " + (ma ? escapeHtml(ma.client_name) : "?") + " " + endpointBadge(ma?.upstream || summarize(exA).upstream);
      document.getElementById("title-b").innerHTML =
        "B · " + (mb ? escapeHtml(mb.client_name) : "?") + " " + endpointBadge(mb?.upstream || summarize(exB).upstream);
      renderSummary(exA, exB);
      renderGrid("grid-a", exA, "a");
      renderGrid("grid-b", exB, "b");
      status.textContent = `loaded ${exA.length} + ${exB.length} exchanges`;
      // Reflect picked sessions in the URL so the comparison is shareable.
      const u = new URL(window.location.href);
      u.searchParams.set("a", aKey);
      u.searchParams.set("b", bKey);
      window.history.replaceState({}, "", u.toString());
    } catch (e) {
      status.textContent = "error: " + e.message;
    }
  }

  document.addEventListener("DOMContentLoaded", async () => {
    const status = document.getElementById("status");
    try {
      allSessions = await fetchSessionsIndex();
    } catch (e) {
      status.textContent = "couldn't load sessions: " + e.message;
      return;
    }
    if (!allSessions.length) {
      status.textContent = "no sessions yet — connect a Claude.ai to /mcp and /mcp-proxy first";
      return;
    }
    const q = getQuery();
    const { a, b } = autoPickDefaults(allSessions, q);
    const pickerA = document.getElementById("picker-a");
    const pickerB = document.getElementById("picker-b");
    populatePicker(pickerA, allSessions, a);
    populatePicker(pickerB, allSessions, b);
    document.getElementById("load-btn").addEventListener("click", () => {
      loadAndRender(pickerA.value, pickerB.value);
    });
    document.getElementById("swap-btn").addEventListener("click", () => {
      const av = pickerA.value;
      pickerA.value = pickerB.value;
      pickerB.value = av;
      loadAndRender(pickerA.value, pickerB.value);
    });
    if (a && b) {
      loadAndRender(a, b);
    } else {
      status.textContent = "pick two sessions and click Load & compare";
    }
  });
})();
