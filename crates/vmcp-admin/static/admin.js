// vmcp admin — client-side nav + views.
// Sidebar #1 toggles between views (Services / Skills / Sessions / Compare):
//   • Services — sidebar #2 (server list) + the selected service in the body.
//   • Skills   — sidebar #2 (skills list) + selected skill detail in the body
//                (Figma master-detail: 452:1659 list / 452:1705 detail).
// Backed by /admin/api/* (servers, skills CRUD, sessions, schema, notifications).
(function () {
  "use strict";

  let servers = [];
  let selected = null;
  let currentView = "services";
  let skills = null;        // cached skill list once loaded
  let skillsLoading = false;

  function esc(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );
  }

  // Only "live" is produced by the API today (pool holds connected upstreams).
  // Building/stop kept for forward-compat with the design.
  function statusPill(status) {
    switch (status) {
      case "building":
        return '<span class="pill pill--blue"><span class="dot dot--blue"></span>Building</span>';
      case "stop":
        return '<span class="pill pill--red"><span class="dot dot--red"></span>Stop</span>';
      case "live":
      default:
        return '<span class="pill pill--green"><span class="dot dot--green"></span>Live</span>';
    }
  }

  const WRENCH =
    '<svg class="tool-item__wrench" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18v3h3l6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2-2z"/></svg>';
  const CARET =
    '<svg class="tool-item__caret" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="9 6 15 12 9 18"/></svg>';

  // ---------- sidebar #2 ----------
  function renderList(filter) {
    const root = document.getElementById("server-list");
    if (!root) return;
    const q = (filter || "").toLowerCase();
    const items = servers.filter(
      (s) =>
        !q ||
        s.name.toLowerCase().includes(q) ||
        (s.description || "").toLowerCase().includes(q)
    );
    if (!items.length) {
      root.innerHTML = '<p class="muted">No matching servers.</p>';
      return;
    }
    root.innerHTML = items
      .map(
        (s) => `<div class="srv-item${s.name === selected ? " active" : ""}" data-name="${esc(s.name)}">
          <div class="srv-item__title">${esc(s.name)}</div>
          ${statusPill(s.status)}
          <div class="srv-item__meta">${s.tool_count} (${s.read_only_count} read-only)</div>
        </div>`
      )
      .join("");
    root.querySelectorAll(".srv-item[data-name]").forEach((el) => {
      el.addEventListener("click", () => selectServer(el.dataset.name));
    });
  }

  // The yellow zone always shows exactly ONE service — the selected one.
  function selectServer(name) {
    selected = name;
    renderList(document.getElementById("server-search").value);
    renderCard(name);
  }

  // ---------- body: tool cards ----------
  function cardHtml(s, tools) {
    const toolItems = tools.length
      ? tools
          .map(
            (t) => `<div class="tool-item">
              <button class="tool-item__head" type="button">
                ${CARET}${WRENCH}<span class="tool-item__name">${esc(t.name)}</span>
              </button>
              <div class="tool-item__desc">${t.description ? esc(t.description) : '<span class="muted">No description.</span>'}</div>
              <div class="tool-item__schema">
                <span class="field-label">Input schema</span>
                <pre class="code-block">${esc(JSON.stringify(t.inputSchema, null, 2))}</pre>
              </div>
            </div>`
          )
          .join("")
      : '<div class="empty-box">No matching records found</div>';

    return `<div class="card" data-name="${esc(s.name)}">
      <div class="card__title">${esc(s.name)}</div>
      <div class="card__sub">Last Deployed At: —</div>
      <div class="card__desc">${s.description ? esc(s.description) : '<span class="muted">No description.</span>'}</div>
      <div class="svc-divider"></div>
      <div class="tool-head-row">
        <span class="tool-head-row__label">${WRENCH}Tool</span>
      </div>
      <div class="tool-count">${s.tool_count} available</div>
      <div class="tool-list">${toolItems}</div>
    </div>`;
  }

  function bindAccordions(root) {
    root.querySelectorAll(".tool-item__head").forEach((btn) => {
      btn.addEventListener("click", () => {
        btn.parentElement.classList.toggle("open");
      });
    });
  }

  async function toolsFor(s) {
    try {
      const r = await fetch(`/admin/api/servers/${encodeURIComponent(s.name)}/tools`);
      if (!r.ok) return [];
      const d = await r.json();
      return d.tools || [];
    } catch (e) {
      return [];
    }
  }

  /** Map API server rows onto the sidebar shape (status pill). */
  function normalizeServer(s) {
    const status =
      s.status ||
      (s.connected === false ? "stop" : "live");
    return Object.assign({}, s, { status: status });
  }

  // Render the single selected service into the yellow zone (full area).
  async function renderCard(name) {
    const root = document.getElementById("server-cards");
    const s = servers.find((x) => x.name === name);
    if (!s) {
      root.innerHTML = '<p class="muted">Select a server to view its tools.</p>';
      return;
    }
    root.innerHTML = '<p class="muted">Loading…</p>';
    const tools = await toolsFor(s);
    // Guard against a newer selection finishing first.
    if (selected !== name) return;
    root.innerHTML = cardHtml(s, tools);
    bindAccordions(root);
  }

  // ---------- Services view ----------
  async function loadServers() {
    const search = document.getElementById("server-search");
    if (search && !search.dataset.bound) {
      search.addEventListener("input", () => renderList(search.value));
      search.dataset.bound = "1";
    }

    try {
      const res = await fetch("/admin/api/servers");
      const data = await res.json();
      servers = (data.servers || []).map(normalizeServer);
    } catch (e) {
      document.getElementById("server-list").innerHTML =
        '<p class="muted">Failed to load servers: ' + esc(String(e)) + "</p>";
      document.getElementById("server-cards").innerHTML = "";
      return;
    }

    renderList("");
    if (servers.length) {
      selectServer(servers[0].name);
    } else {
      document.getElementById("server-cards").innerHTML =
        '<p class="muted">No upstream servers registered.</p>';
    }
  }

  // ---------- Skills view (master-detail: side2 list + body detail) ----------
  // Figma nodes 452:1659 (list) / 452:1705 (detail). Side2 stays visible and
  // is reconfigured as the skills list; body shows the selected skill.
  let selectedSkill = null;

  function skillArgRow(a) {
    const req = a.required ? ' <span class="badge-required">required</span>' : "";
    const desc = a.description
      ? " — " + esc(a.description)
      : "";
    return "<li><code>" + esc(a.name) + "</code>" + req + desc + "</li>";
  }

  function ensureSkillsSide2() {
    const side2 = document.getElementById("side2");
    if (!side2 || side2.dataset.mode === "skills") return;
    side2.dataset.mode = "skills";
    const header = side2.querySelector(".side2__header");
    if (header) {
      header.innerHTML = `
        <h2 class="side2__title">Skills (MCP prompts)</h2>
        <div class="side2__search side2__search--dark">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>
          <input type="text" id="skill-search" placeholder="Search skill..." autocomplete="off">
        </div>
        <button type="button" class="btn btn--neutral btn--block" id="new-skill-btn">+ New skill</button>`;
      const search = document.getElementById("skill-search");
      if (search) search.addEventListener("input", () => renderSkillList(search.value));
      const newBtn = document.getElementById("new-skill-btn");
      if (newBtn) newBtn.addEventListener("click", () => openSkillModal("new", null));
    }
    const list = document.getElementById("server-list");
    if (list) list.id = "skill-list";
  }

  function ensureServersSide2() {
    const side2 = document.getElementById("side2");
    if (!side2 || side2.dataset.mode === "services") return;
    side2.dataset.mode = "services";
    const header = side2.querySelector(".side2__header");
    if (header) {
      header.innerHTML = `
        <h2 class="side2__title">Upstream MCP servers</h2>
        <div class="side2__search">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>
          <input type="text" id="server-search" placeholder="Search servers…" autocomplete="off">
        </div>`;
      const search = document.getElementById("server-search");
      if (search) {
        search.addEventListener("input", () => renderList(search.value));
        search.dataset.bound = "1";
      }
    }
    const list = document.getElementById("skill-list") || document.getElementById("server-list");
    if (list) list.id = "server-list";
  }

  function renderSkillList(filter) {
    const root = document.getElementById("skill-list");
    if (!root) return;
    if (skillsLoading) {
      root.innerHTML = '<p class="muted">Loading…</p>';
      return;
    }
    if (skills === "error") {
      root.innerHTML = '<p class="muted">Failed to load skills.</p>';
      return;
    }
    const all = Array.isArray(skills) ? skills : [];
    const q = (filter || "").toLowerCase();
    const items = all.filter(
      (s) =>
        !q ||
        s.name.toLowerCase().includes(q) ||
        (s.description || "").toLowerCase().includes(q)
    );
    if (!items.length) {
      root.innerHTML = all.length
        ? '<p class="muted">No matching skills.</p>'
        : '<p class="muted">No skills loaded.</p>';
      return;
    }
    root.innerHTML = items
      .map(
        (s) => `<div class="srv-item${s.name === selectedSkill ? " active" : ""}" data-name="${esc(s.name)}">
          <div class="srv-item__title">${esc(s.name)}</div>
          <div class="srv-item__meta">Last Deployed At: —</div>
        </div>`
      )
      .join("");
    root.querySelectorAll(".srv-item[data-name]").forEach((el) => {
      el.addEventListener("click", () => selectSkill(el.dataset.name));
    });
  }

  async function selectSkill(name) {
    selectedSkill = name;
    const search = document.getElementById("skill-search");
    renderSkillList(search ? search.value : "");
    const body = document.getElementById("server-cards");
    if (body) body.innerHTML = '<p class="muted">Loading…</p>';
    try {
      const r = await fetch("/admin/api/skills/" + encodeURIComponent(name));
      if (!r.ok) throw new Error("HTTP " + r.status);
      const skill = await r.json();
      renderSkillDetail(skill);
    } catch (e) {
      if (body) {
        body.innerHTML =
          '<p class="muted">Failed to load skill: ' + esc(String(e)) + "</p>";
      }
    }
  }

  function renderSkillDetail(skill) {
    const root = document.getElementById("server-cards");
    if (!root) return;
    const args = skill.arguments || [];
    const n = args.length;
    const argsInner = n
      ? '<ul class="skill-args">' + args.map(skillArgRow).join("") + "</ul>"
      : '<p class="muted">none</p>';
    const desc = skill.description
      ? "<p class=\"skill-detail__desc\">" + esc(skill.description) + "</p>"
      : '<p class="muted">No description.</p>';

    root.innerHTML = `
      <div class="skill-detail">
        <div class="skill-detail__head">
          <h1 class="skill-detail__title">${esc(skill.name)}</h1>
          <div class="skill-detail__meta">Last Deployed At: —</div>
          ${desc}
        </div>
        <div class="skill-detail__actions">
          <button type="button" class="btn btn--ghost btn--sm" data-action="edit">Edit</button>
          <button type="button" class="btn btn--ghost btn--sm" data-action="duplicate">Duplicate</button>
          <button type="button" class="btn btn--danger btn--sm" data-action="delete">Delete</button>
        </div>
        <div class="skill-detail__section">
          <div class="skill-args-label">Arguments</div>
          <div class="skill-detail__block">
            <div class="muted mb-2">${n} argument${n === 1 ? "" : "s"}:</div>
            ${argsInner}
          </div>
        </div>
        <div class="skill-detail__section">
          <div class="skill-args-label">Template</div>
          <pre class="code-block" id="skill-detail-code">${esc(skill.template || "")}</pre>
        </div>
        <div class="skill-detail__section">
          <label class="field-label" for="skill-detail-args">Args (JSON)</label>
          <textarea class="textarea mb-2" id="skill-detail-args" rows="4">{}</textarea>
          <button type="button" class="btn btn--primary btn--sm" data-action="generate">Generate</button>
          <pre class="code-block mt-2 d-none" id="skill-detail-result"></pre>
        </div>
      </div>`;

    root.querySelectorAll("[data-action]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const action = btn.getAttribute("data-action");
        if (action === "edit") openSkillModalEnsured("edit", skill);
        else if (action === "duplicate") openSkillModalEnsured("duplicate", skill);
        else if (action === "delete") deleteSkill(skill.name);
        else if (action === "generate") generateSkill(skill.name);
      });
    });
  }

  async function generateSkill(name) {
    const argsEl = document.getElementById("skill-detail-args");
    const resultEl = document.getElementById("skill-detail-result");
    if (!argsEl || !resultEl) return;
    let argsObj;
    try {
      argsObj = JSON.parse(argsEl.value || "{}");
    } catch (e) {
      resultEl.classList.remove("d-none");
      resultEl.textContent = "args JSON error: " + e;
      return;
    }
    resultEl.classList.remove("d-none");
    resultEl.textContent = "…";
    try {
      const r = await fetch("/admin/api/skills/" + encodeURIComponent(name) + "/generate", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ args: argsObj }),
      });
      const data = await r.json();
      resultEl.textContent = data.rendered || JSON.stringify(data, null, 2);
    } catch (e) {
      resultEl.textContent = "request failed: " + e;
    }
  }

  function cssEscape(s) {
    return window.CSS && CSS.escape ? CSS.escape(s) : String(s).replace(/[^a-zA-Z0-9_-]/g, "\\$&");
  }

  async function loadSkills() {
    ensureSkillsSide2();
    skillsLoading = true;
    renderSkillList("");
    const body = document.getElementById("server-cards");
    if (body) body.innerHTML = '<p class="muted">Loading…</p>';
    try {
      const r = await fetch("/admin/api/skills");
      if (!r.ok) throw new Error("HTTP " + r.status);
      const data = await r.json();
      skills = data.skills || [];
    } catch (e) {
      skills = "error";
    }
    skillsLoading = false;
    if (currentView !== "skills") return;
    const search = document.getElementById("skill-search");
    renderSkillList(search ? search.value : "");
    if (Array.isArray(skills) && skills.length) {
      const want =
        selectedSkill && skills.some((s) => s.name === selectedSkill)
          ? selectedSkill
          : skills[0].name;
      await selectSkill(want);
    } else if (body) {
      body.innerHTML =
        skills === "error"
          ? '<p class="muted">Failed to load skills.</p>'
          : '<div class="empty-box">No skills loaded. Click “+ New skill” or drop a YAML file into the skills directory.</div>';
    }
  }

  // ---------- skill modal (create / edit / duplicate) ----------
  function showModal() {
    const m = document.getElementById("skill-modal");
    m.classList.add("show");
    m.setAttribute("aria-hidden", "false");
  }
  function hideModal() {
    const m = document.getElementById("skill-modal");
    m.classList.remove("show");
    m.setAttribute("aria-hidden", "true");
    setModalError("");
  }
  function setModalError(msg) {
    const el = document.getElementById("skill-modal-error");
    if (!el) return;
    if (msg) {
      el.textContent = msg;
      el.classList.remove("d-none");
    } else {
      el.textContent = "";
      el.classList.add("d-none");
    }
  }

  function argEditorRow(a) {
    const row = document.createElement("div");
    row.className = "arg-row";
    row.innerHTML =
      '<input type="text" class="input" placeholder="name" data-arg-field="name" value="' +
      esc(a.name || "") +
      '">' +
      '<input type="text" class="input" placeholder="description" data-arg-field="description" value="' +
      esc(a.description || "") +
      '">' +
      '<label class="arg-row__req"><input class="checkbox" type="checkbox" data-arg-field="required" ' +
      (a.required ? "checked" : "") +
      "> required</label>" +
      '<button type="button" class="arg-row__remove" data-arg-remove aria-label="Remove argument">&times;</button>';
    row.querySelector("[data-arg-remove]").addEventListener("click", () => row.remove());
    return row;
  }

  function renderArgsEditor(args) {
    const c = document.getElementById("skill-modal-args");
    c.innerHTML = "";
    (args || []).forEach((a) => c.appendChild(argEditorRow(a)));
  }

  function collectArgs() {
    const rows = document.querySelectorAll("#skill-modal-args .arg-row");
    const out = [];
    rows.forEach((row) => {
      const name = row.querySelector('[data-arg-field="name"]').value.trim();
      const description = row.querySelector('[data-arg-field="description"]').value.trim();
      const required = row.querySelector('[data-arg-field="required"]').checked;
      if (name) out.push({ name, description: description || null, required });
    });
    return out;
  }

  function openSkillModal(mode, skill) {
    document.getElementById("skill-modal-mode").value = mode;
    document.getElementById("skill-modal-original-name").value =
      skill && mode === "edit" ? skill.name : "";
    document.getElementById("skill-modal-title").textContent =
      mode === "edit" ? "Edit skill" : mode === "duplicate" ? "Duplicate skill" : "New skill";
    document.getElementById("skill-modal-name").value =
      mode === "duplicate" ? skill.name + "_copy" : (skill && skill.name) || "";
    document.getElementById("skill-modal-name").disabled = mode === "edit";
    document.getElementById("skill-modal-description").value = (skill && skill.description) || "";
    document.getElementById("skill-modal-template").value =
      (skill && (skill.template || skill.template_preview)) || "";
    renderArgsEditor((skill && skill.arguments) || []);
    showModal();
  }

  async function openSkillModalEnsured(mode, skill) {
    let full = skill;
    if (
      (mode === "edit" || mode === "duplicate") &&
      skill &&
      typeof skill.template !== "string"
    ) {
      try {
        const r = await fetch("/admin/api/skills/" + encodeURIComponent(skill.name));
        if (r.ok) full = await r.json();
      } catch (e) {
        /* fall through with preview data */
      }
    }
    openSkillModal(mode, full);
  }

  async function saveSkill() {
    const mode = document.getElementById("skill-modal-mode").value;
    const name = document.getElementById("skill-modal-name").value.trim();
    const description = document.getElementById("skill-modal-description").value.trim();
    const template = document.getElementById("skill-modal-template").value;
    const args = collectArgs();
    if (!name) return setModalError("name is required");
    if (!template) return setModalError("template is required");
    const payload = { name, description, template, arguments: args };
    const url =
      mode === "edit"
        ? "/admin/api/skills/" +
          encodeURIComponent(document.getElementById("skill-modal-original-name").value)
        : "/admin/api/skills";
    const method = mode === "edit" ? "PUT" : "POST";
    try {
      const r = await fetch(url, {
        method,
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (r.status === 200 || r.status === 201) {
        hideModal();
        selectedSkill = name;
        await loadSkills();
      } else {
        const txt = await r.text();
        setModalError("save failed (" + r.status + "): " + txt);
      }
    } catch (e) {
      setModalError("request failed: " + e);
    }
  }

  async function deleteSkill(name) {
    if (!window.confirm('Delete skill "' + name + '"?')) return;
    try {
      const r = await fetch("/admin/api/skills/" + encodeURIComponent(name), {
        method: "DELETE",
      });
      if (r.ok || r.status === 204) {
        if (selectedSkill === name) selectedSkill = null;
        await loadSkills();
      } else {
        window.alert("delete failed (" + r.status + "): " + (await r.text()));
      }
    } catch (e) {
      window.alert("request failed: " + e);
    }
  }

  function bindModal() {
    const closeBtn = document.getElementById("skill-modal-close");
    if (closeBtn) closeBtn.addEventListener("click", hideModal);
    const cancelBtn = document.getElementById("skill-modal-cancel");
    if (cancelBtn) cancelBtn.addEventListener("click", hideModal);
    const saveBtn = document.getElementById("skill-modal-save");
    if (saveBtn) saveBtn.addEventListener("click", saveSkill);
    const addArgBtn = document.getElementById("skill-modal-add-arg");
    if (addArgBtn)
      addArgBtn.addEventListener("click", () => {
        document.getElementById("skill-modal-args").appendChild(argEditorRow({}));
      });
    // Backdrop click closes the dialog.
    const modal = document.getElementById("skill-modal");
    if (modal)
      modal.addEventListener("click", (e) => {
        if (e.target === modal) hideModal();
      });
  }

  // ==========================================================================
  // Sessions view — clients | sessions | exchanges + describe-exchange.
  // Read-only, driven by /admin/api/sessions*. Mirrors the /admin Sessions tab
  // but rendered inside the admin body (sidebar #2 is hidden for this view).
  // ==========================================================================
  let sessClients = [];          // last /admin/api/sessions payload (stable-sorted)
  let sessSelectedClient = null; // client_id
  let sessSelectedSession = null;// session id
  let sessSelectedExchangeIdx = null;
  let sessLastExchanges = [];
  let sessPollTimer = null;

  function sessFmtTime(ms) {
    if (!ms) return "—";
    try {
      return new Date(ms).toLocaleString();
    } catch (e) {
      return String(ms);
    }
  }

  function sessShortId(id, n) {
    if (!id) return "—";
    n = n || 18;
    return id.length > n ? id.slice(0, n) + "…" : id;
  }

  function sessStateBadge(state) {
    const label = state === "pre_registered" ? "pre-registered" : (state || "—");
    const color = state === "active"
      ? "green"
      : state === "pre_registered"
        ? "blue"
        : state === "idle"
          ? "yellow"
          : "";
    const dot = state === "active"
      ? "dot dot--green dot--pulse"
      : "dot" + (color ? " dot--" + color : "");
    return `<span class="pill${color ? " pill--" + color : ""}"><span class="${dot}"></span>${esc(label)}</span>`;
  }

  function sessEndpointBadge(upstream) {
    if (!upstream) return "";
    const color = upstream === "/mcp-proxy" ? "purple" : "lime";
    return `<span class="pill pill--${color}"><span class="dot dot--${color}"></span><span class="mono">${esc(upstream)}</span></span>`;
  }

  function sessEndpointLabel(upstream) {
    return upstream ? upstream.replace(/^\//, "") : "mcp";
  }

  function sessUpstreamOf(sessions) {
    return (sessions || []).find((s) => s.upstream)?.upstream || null;
  }

  function sessFindClient(cid) {
    return sessClients.find((c) => c.client_id === cid) || null;
  }

  function sessCurrentSession() {
    const c = sessFindClient(sessSelectedClient);
    return ((c && c.sessions) || []).find((s) => s.id === sessSelectedSession) || null;
  }

  function sessCurrentUpstream() {
    const s = sessCurrentSession();
    const c = sessFindClient(sessSelectedClient);
    return (s && s.upstream) || sessUpstreamOf(c && c.sessions);
  }

  // ----- clients column -----
  function sessClientLabel(c) {
    if (c && c.name) return String(c.name);
    if (c && c.client_name) return String(c.client_name);
    return "";
  }

  function sessRenderClients() {
    const root = document.getElementById("sess-client-list");
    if (!root) return;
    const q = (document.getElementById("sess-client-search")?.value || "").toLowerCase().trim();
    const items = sessClients.filter((c) =>
      !q ||
      sessClientLabel(c).toLowerCase().includes(q) ||
      (c.client_name || "").toLowerCase().includes(q) ||
      (c.client_id || "").toLowerCase().includes(q)
    );
    if (!items.length) {
      root.innerHTML = q
        ? '<p class="muted">No matching clients.</p>'
        : '<p class="muted"><em>No registered clients or sessions yet.</em></p>';
      return;
    }
    root.innerHTML = items.map((c) => {
      const cid = c.client_id;
      const upstream = sessUpstreamOf(c.sessions);
      const label = sessClientLabel(c);
      const name = label
        ? esc(label)
        : '<span class="muted">(unnamed)</span>';
      const sub = c.name && c.client_name && c.name !== String(c.client_name).toLowerCase()
        ? `<span class="muted sess-item__sub">${esc(c.client_name)}</span>`
        : `<span class="muted sess-item__sub mono">${esc(sessShortId(cid, 14))}</span>`;
      return `<div class="sess-item${cid === sessSelectedClient ? " active" : ""}" data-cid="${esc(cid)}">
          <div class="sess-item__title">${name} ${sessEndpointBadge(upstream)}</div>
          <div class="sess-item__meta">${sessStateBadge(c.state)} <span class="pill">${(c.sessions || []).length}</span> ${sub}</div>
        </div>`;
    }).join("");
    root.querySelectorAll(".sess-item[data-cid]").forEach((el) => {
      el.addEventListener("click", () => sessSelectClient(el.dataset.cid));
    });
  }

  function sessSelectClient(cid) {
    sessSelectedClient = cid;
    sessSelectedSession = null;
    sessSelectedExchangeIdx = null;
    sessResetDetail();
    sessRenderClients();
    sessRenderNameEditor();
    sessRenderSessions();
  }

  function sessResetDetail() {
    const panel = document.getElementById("sess-detail");
    if (panel) {
      panel.innerHTML =
        '<div class="sess__empty"><p class="muted">Select a session to inspect its exchanges.</p></div>';
    }
  }

  // ----- operator name editor (DCR unique label) -----
  function sessRenderNameEditor() {
    const wrap = document.getElementById("sess-name-editor");
    if (!wrap) return;
    const client = sessFindClient(sessSelectedClient);
    if (!client || !client.name) {
      // Disk-only / orphan rows have no DCR name to edit.
      wrap.innerHTML = client
        ? '<p class="muted sess-name-hint">No editable DCR name for this client.</p>'
        : "";
      return;
    }
    wrap.innerHTML = `
      <label class="sess-name-label" for="sess-client-name">Name</label>
      <div class="sess-name-row">
        <input type="text" class="input" id="sess-client-name" value="${esc(client.name)}"
          maxlength="64" pattern="[a-z0-9_-]{1,64}" autocomplete="off"
          spellcheck="false" title="Unique label: a-z, 0-9, _, -">
        <button type="button" class="btn btn--neutral btn--sm" id="sess-name-save">Save</button>
      </div>
      <p class="form-hint" id="sess-name-status">unique · editable</p>`;
    const input = document.getElementById("sess-client-name");
    const saveBtn = document.getElementById("sess-name-save");
    const save = () => sessSaveClientName(client.client_id, input && input.value);
    if (saveBtn) saveBtn.addEventListener("click", save);
    if (input) {
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          save();
        }
      });
    }
  }

  async function sessSaveClientName(cid, raw) {
    const status = document.getElementById("sess-name-status");
    const name = (raw || "").trim();
    if (!name) {
      if (status) status.textContent = "name is required";
      return;
    }
    if (status) status.textContent = "saving…";
    try {
      const r = await fetch("/admin/api/sessions/" + encodeURIComponent(cid), {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name }),
      });
      const txt = await r.text();
      let data = null;
      try { data = JSON.parse(txt); } catch (e) { /* ignore */ }
      if (!r.ok) {
        if (status) {
          status.textContent = (data && data.error) || ("HTTP " + r.status + ": " + txt);
        }
        return;
      }
      const c = sessFindClient(cid);
      if (c) c.name = (data && data.name) || name;
      if (status) status.textContent = "saved";
      sessRenderClients();
      sessRenderNameEditor();
    } catch (e) {
      if (status) status.textContent = "save failed: " + e;
    }
  }

  // ----- sessions column -----
  function sessRenderSessions() {
    const root = document.getElementById("sess-session-list");
    if (!root) return;
    const client = sessFindClient(sessSelectedClient);
    if (!client) {
      root.innerHTML = '<p class="muted">Select a client.</p>';
      return;
    }
    const sessions = client.sessions || [];
    if (!sessions.length) {
      root.innerHTML = '<p class="muted"><em>No sessions yet.</em></p>';
      return;
    }
    root.innerHTML = sessions.map((s) => {
      const active = sessSelectedClient === client.client_id && s.id === sessSelectedSession;
      const dot = s.status === "active"
        ? '<span class="dot dot--green dot--pulse"></span>'
        : '<span class="dot"></span>';
      return `<div class="sess-item${active ? " active" : ""}" data-sid="${esc(s.id)}">
          <div class="sess-item__title">${dot} <code>${esc(sessShortId(s.id, 20))}</code></div>
          <div class="sess-item__meta">${esc(s.status || "—")} · ${s.request_count || 0} req · ${esc(sessFmtTime(s.started_at_ms))}</div>
        </div>`;
    }).join("");
    root.querySelectorAll(".sess-item[data-sid]").forEach((el) => {
      el.addEventListener("click", () => sessSelectSession(client.client_id, el.dataset.sid));
    });
  }

  // ----- exchange classification helpers -----
  function sessMethodCategory(method) {
    if (!method) return null;
    if (method === "initialize") return "cap";
    if (method === "ping") return "ping";
    if (method.startsWith("tools/")) return "tool";
    if (method.startsWith("prompts/")) return "prompt";
    if (method.startsWith("resources/")) return "resource";
    if (method.startsWith("notifications/")) return "notify";
    return "other";
  }

  const SESS_CATEGORY_COLOR = {
    cap: "lime",
    tool: "orange",
    prompt: "purple",
    resource: "blue",
    notify: "blue",
  };

  function sessCategoryPill(cat) {
    if (!cat) return "";
    const color = SESS_CATEGORY_COLOR[cat];
    const cls = color ? " pill--" + color : "";
    const dot = `<span class="dot${color ? " dot--" + color : ""}"></span>`;
    return `<span class="pill${cls}">${dot}${esc(cat)}</span>`;
  }

  function sessBodyBytes(ex) {
    if (!ex || ex.body == null) return 0;
    let str;
    try {
      str = typeof ex.body === "string" ? ex.body : JSON.stringify(ex.body);
    } catch (e) {
      return 0;
    }
    if (!str || str === "{}" || str === "null" || str === "[]") return 0;
    return str.length;
  }

  function sessIsEmptyBody(ex) {
    return sessBodyBytes(ex) === 0;
  }

  function sessSumBodyKb(exchanges) {
    const bytes = (exchanges || [])
      .filter((e) => e.direction === "S2C")
      .reduce((a, e) => a + sessBodyBytes(e), 0);
    return bytes ? (bytes / 1024).toFixed(1) : "0";
  }

  function sessJsonId(id) {
    return typeof id === "string" ? id : String(id);
  }

  function sessArrowHtml(ex) {
    const glyph = ex.direction === "C2S" ? "→" : "←";
    let color;
    if (sessIsEmptyBody(ex)) color = "gray";
    else if (sessMethodCategory(ex.method) === "cap") color = "green";
    else color = ex.direction === "C2S" ? "blue" : "magenta";
    return `<span class="xchg__arrow xchg__arrow--${color}">${glyph}</span>`;
  }

  function sessRowLabel(ex) {
    const err = ex.body && typeof ex.body === "object" && ex.body.error;
    if (ex.direction === "S2C") {
      if (sessMethodCategory(ex.method) === "notify") return err ? "error" : "result";
      if (sessIsEmptyBody(ex)) return "";
      return err ? "error" : "result";
    }
    if (sessIsEmptyBody(ex)) return "";
    return ex.method || "";
  }

  // Vendored GraphQL format (graphql print) + Prism highlighting under /admin/static/vendor/.
  // Loaded dynamically so main.html stays a single script tag (CSP script-src 'self').
  function loadScriptOnce(src) {
    return new Promise((resolve, reject) => {
      const existing = document.querySelector('script[data-vmcp-src="' + src + '"]');
      if (existing) {
        if (existing.dataset.vmcpLoaded === "1") {
          resolve();
          return;
        }
        existing.addEventListener("load", () => resolve(), { once: true });
        existing.addEventListener("error", () => reject(new Error(src)), { once: true });
        return;
      }
      const s = document.createElement("script");
      s.src = src;
      s.dataset.vmcpSrc = src;
      s.onload = () => {
        s.dataset.vmcpLoaded = "1";
        resolve();
      };
      s.onerror = () => reject(new Error(src));
      document.head.appendChild(s);
    });
  }

  let gqlLibsPromise = null;
  function ensureGqlLibs() {
    if (gqlLibsPromise) return gqlLibsPromise;
    gqlLibsPromise = (async () => {
      await Promise.all([
        loadScriptOnce("/admin/static/vendor/graphql-format.min.js"),
        (async () => {
          await loadScriptOnce("/admin/static/vendor/prism-core.min.js");
          await loadScriptOnce("/admin/static/vendor/prism-graphql.min.js");
          await loadScriptOnce("/admin/static/vendor/prism-json.min.js");
        })(),
      ]);
    })().catch(() => {
      /* keep Detail usable even if vendor assets fail */
    });
    return gqlLibsPromise;
  }
  ensureGqlLibs();

  // Extract query_graphql payload from a tools/call request body.
  // Only this gateway tool carries a GraphQL document — other tools/call stay JSON-only.
  function sessExtractGraphql(req) {
    if (!req || !req.body || typeof req.body !== "object") return null;
    const body = req.body;
    if (body.method !== "tools/call") return null;
    const params = body.params || {};
    if (params.name !== "query_graphql") return null;
    const args = params.arguments || {};
    if (typeof args.query !== "string" || !args.query.trim()) return null;
    return {
      query: args.query,
      variables: args.variables,
      operationName: args.operation_name || args.operationName || null,
    };
  }

  // Pretty-print GraphQL via vendored graphql print; indent fallback if lib missing.
  function sessPrettyGraphql(query) {
    const q = String(query || "").trim();
    if (!q) return "";
    const fmt = typeof GraphqlFormat !== "undefined" && GraphqlFormat && GraphqlFormat.formatGraphql;
    if (typeof fmt === "function") {
      try {
        const out = fmt(q);
        if (out && String(out).trim()) return String(out).replace(/\s+$/, "") + "\n";
      } catch (e) {
        /* fall through */
      }
    }
    if (/\n/.test(q)) return q.endsWith("\n") ? q : q + "\n";
    let depth = 0;
    let result = "";
    let i = 0;
    while (i < q.length) {
      const ch = q[i];
      if (ch === "{") {
        depth++;
        result += "{\n" + "  ".repeat(depth);
        i++;
        while (q[i] === " ") i++;
        continue;
      }
      if (ch === "}") {
        depth = Math.max(0, depth - 1);
        result = result.replace(/[ \t]+$/, "");
        if (!result.endsWith("\n")) result += "\n";
        result += "  ".repeat(depth) + "}";
        i++;
        while (q[i] === " ") i++;
        if (q[i] && q[i] !== "}") result += "\n" + "  ".repeat(depth);
        continue;
      }
      result += ch;
      i++;
    }
    return result.trimEnd() + "\n";
  }

  // Prism.highlight returns escaped HTML; falls back to esc() when Prism is unavailable.
  function sessHighlightCode(code, language) {
    const text = String(code == null ? "" : code);
    const PrismRef = typeof Prism !== "undefined" ? Prism : null;
    if (
      PrismRef &&
      PrismRef.highlight &&
      PrismRef.languages &&
      PrismRef.languages[language]
    ) {
      try {
        return PrismRef.highlight(text, PrismRef.languages[language], language);
      } catch (e) {
        /* fall through */
      }
    }
    return esc(text);
  }

  function sessCodeBlock(code, language, extraClass) {
    const cls = extraClass ? "code-block " + extraClass : "code-block";
    return (
      `<pre class="${cls}"><code class="language-${language}">` +
      sessHighlightCode(code, language) +
      `</code></pre>`
    );
  }

  // Parse GraphQL JSON from a query_graphql CallToolResult (content[0].text).
  function sessExtractGraphqlResult(resp) {
    if (!resp || !resp.body || typeof resp.body !== "object") return null;
    const result = resp.body.result;
    if (!result || typeof result !== "object") return null;
    const content = result.content;
    if (!Array.isArray(content) || !content.length) return null;
    const text = content[0] && content[0].text;
    if (typeof text !== "string" || !text.trim()) return null;
    try {
      return JSON.parse(text);
    } catch (e) {
      return null;
    }
  }

  // Readable GraphQL breakdown under Request/Response (query → variables → result).
  function sessGraphqlDetailHtml(gql, gqlResult) {
    if (!gql && !gqlResult) return "";
    let html = `<div class="xd__section">
          <div class="xd__label xd__label--gql">GraphQL</div>`;
    if (gql) {
      html += `<div class="xd__gql-meta"><span class="field-label">query</span></div>`;
      html += sessCodeBlock(sessPrettyGraphql(gql.query), "graphql", "code-block--gql");
      if (gql.operationName) {
        html += `<div class="xd__gql-meta"><span class="field-label">operation</span> <code>${esc(gql.operationName)}</code></div>`;
      }
      if (gql.variables != null) {
        html += `<div class="xd__gql-meta"><span class="field-label">variables</span></div>`;
        html += sessCodeBlock(JSON.stringify(gql.variables, null, 2), "json", "code-block--gql");
      }
    }
    if (gqlResult) {
      html += `<div class="xd__gql-meta"><span class="field-label">result</span></div>`;
      html += sessCodeBlock(JSON.stringify(gqlResult, null, 2), "json", "code-block--gql");
    }
    html += `</div>`;
    return html;
  }

  // ----- exchanges zone -----
  async function sessSelectSession(cid, sid) {
    sessSelectedClient = cid;
    sessSelectedSession = sid;
    sessSelectedExchangeIdx = null;
    sessRenderSessions();
    sessRenderSessionView();

    const grid = document.getElementById("sess-dump-grid");
    if (grid) grid.innerHTML = '<p class="muted">Loading…</p>';

    try {
      const res = await fetch(
        `/admin/api/sessions/${encodeURIComponent(cid)}/dump?session_id=${encodeURIComponent(sid)}&limit=100`
      );
      const data = await res.json();
      sessLastExchanges = normalizeDumpRows(data);
      sessRenderExchanges();
      if (sessLastExchanges.length) sessShowExchange(0);
    } catch (e) {
      if (grid) grid.innerHTML = '<p class="danger">Failed to load dump: ' + esc(String(e)) + "</p>";
    }
  }

  function sessRenderSessionView() {
    const panel = document.getElementById("sess-detail");
    if (!panel) return;
    const exportHref =
      `/admin/api/sessions/${encodeURIComponent(sessSelectedClient)}/dump.jsonl?session_id=${encodeURIComponent(sessSelectedSession)}`;
    panel.innerHTML = `
      <div class="xchg-col">
        <div class="xchg-col__header">
          <h2 class="xchg-col__title">EXCHANGES <span class="xchg-col__sum" id="sess-xchg-sum">· Σ 0 kB body</span></h2>
          <div class="xchg-col__toolbar">
            <a class="btn btn--ghost btn--sm" id="sess-export-link" href="${exportHref}">⬇ Export .jsonl</a>
          </div>
        </div>
        <div class="xchg-col__body" id="sess-dump-grid"><p class="muted">Loading…</p></div>
      </div>
      <div class="xchg-detail" id="sess-exchange-detail">
        <div class="sess__empty"><p class="muted">Select an exchange.</p></div>
      </div>`;
  }

  function sessRenderExchanges() {
    const grid = document.getElementById("sess-dump-grid");
    const sumEl = document.getElementById("sess-xchg-sum");
    const all = sessLastExchanges || [];
    if (sumEl) sumEl.textContent = `· Σ ${sessSumBodyKb(all)} kB body`;
    if (!grid) return;
    if (!all.length) {
      grid.innerHTML = '<p class="muted"><em>No exchanges yet</em></p>';
      return;
    }
    // Group consecutive identical notify / ping into one row with a ×N count.
    const items = [];
    all.forEach((ex, i) => {
      const cat = sessMethodCategory(ex.method);
      const groupable = cat === "notify" || cat === "ping";
      const last = items[items.length - 1];
      if (groupable && last && last.groupable &&
          last.ex.method === ex.method && last.ex.direction === ex.direction) {
        last.count++;
        return;
      }
      items.push({ ex, idx: i, count: 1, groupable, cat });
    });

    grid.innerHTML = items.map((it) => {
      const ex = it.ex;
      const t = ex.timestamp_ms
        ? new Date(ex.timestamp_ms).toISOString().substr(11, 12)
        : "—";
      const right = [];
      if (it.cat) right.push(sessCategoryPill(it.cat));
      if (it.count > 1) right.push(`<span class="pill pill--faint">×${it.count}</span>`);
      if (ex.direction === "C2S" && ex.jsonrpc_id != null) {
        right.push(`<span class="pill pill--faint">id:${esc(sessJsonId(ex.jsonrpc_id))}</span>`);
      } else if (ex.direction === "S2C" && ex.latency_ms != null) {
        right.push(`<span class="pill pill--faint">${ex.latency_ms} ms</span>`);
      }
      return `<div class="xchg${it.idx === sessSelectedExchangeIdx ? " active" : ""}" data-idx="${it.idx}">
          <span class="xchg__time">${esc(t)}</span>
          ${sessArrowHtml(ex)}
          <span class="xchg__method">${esc(sessRowLabel(ex))}</span>
          <span class="xchg__right">${right.join("")}</span>
        </div>`;
    }).join("");

    grid.querySelectorAll(".xchg[data-idx]").forEach((el) => {
      el.addEventListener("click", () => sessShowExchange(parseInt(el.dataset.idx, 10)));
    });
  }

  async function sessShowExchange(idx) {
    sessSelectedExchangeIdx = idx;
    document.querySelectorAll("#sess-dump-grid .xchg").forEach((el) => {
      el.classList.toggle("active", String(el.dataset.idx) === String(idx));
    });
    const detail = document.getElementById("sess-exchange-detail");
    const all = sessLastExchanges || [];
    const ex = all[idx];
    if (!detail || !ex) return;

    // Pair lookup: same jsonrpc_id, opposite direction.
    const pair = all.find(
      (e) =>
        e.jsonrpc_id != null &&
        ex.jsonrpc_id != null &&
        JSON.stringify(e.jsonrpc_id) === JSON.stringify(ex.jsonrpc_id) &&
        e.direction !== ex.direction
    );
    const [req, resp] = ex.direction === "C2S" ? [ex, pair] : [pair, ex];
    const base = req || resp || ex;
    const method = (req && req.method) || (resp && resp.method) || ex.method || "—";

    const idPart = base && base.jsonrpc_id != null
      ? `<span class="pill pill--faint">id:${esc(sessJsonId(base.jsonrpc_id))}</span>`
      : "";
    const msPart = resp && resp.latency_ms != null
      ? `<span class="pill pill--faint">${resp.latency_ms} ms</span>`
      : "";

    // For tools/call → query_graphql: JSON first, then one GraphQL block at the bottom.
    const gql = sessExtractGraphql(req);
    const gqlResult = gql ? sessExtractGraphqlResult(resp) : null;
    if (gql || gqlResult) await ensureGqlLibs();
    const toolPill = gql
      ? `<span class="pill pill--lime"><span class="dot dot--lime"></span>query_graphql</span>`
      : "";

    // Drop stale paint if the user clicked another exchange while libs loaded.
    if (sessSelectedExchangeIdx !== idx) return;

    detail.innerHTML = `
      <div class="xd">
        <div class="xd__head">
          <span class="xd__method">${esc(method)}</span>
          <span class="xd__meta">
            <span class="xd__endpoint">${esc(sessEndpointLabel(sessCurrentUpstream()))}</span>
            ${toolPill}${idPart}${msPart}
          </span>
        </div>
        <div class="xd__section">
          <div class="xd__label xd__label--req">→ Request</div>
          <pre class="code-block">${req ? esc(JSON.stringify(req.body, null, 2)) : "—"}</pre>
        </div>
        <div class="xd__section">
          <div class="xd__label xd__label--resp">← Response</div>
          <pre class="code-block">${resp ? esc(JSON.stringify(resp.body, null, 2)) : "(no response)"}</pre>
        </div>
        ${sessGraphqlDetailHtml(gql, gqlResult)}
      </div>`;
  }

  // ----- data load + poll -----
  async function sessRefreshClients() {
    const root = document.getElementById("sess-client-list");
    if (!root) return;
    try {
      const res = await fetch("/admin/api/sessions");
      if (!res.ok) {
        root.innerHTML = '<p class="danger">API error: HTTP ' + res.status + "</p>";
        return;
      }
      const data = await res.json();
      // Stable order so rows don't jump between polls. Active first, then id.
      sessClients = (data.clients || []).slice().sort((a, b) => {
        const rank = (s) => (s === "active" ? 0 : s === "pre_registered" ? 1 : 2);
        const d = rank(a.state) - rank(b.state);
        return d !== 0 ? d : String(a.client_id).localeCompare(String(b.client_id));
      });
      sessRenderClients();
      if (sessSelectedClient) {
        sessRenderNameEditor();
        sessRenderSessions();
      }
    } catch (e) {
      root.innerHTML = '<p class="danger">Fetch failed: ' + esc(String(e)) + "</p>";
    }
  }

  function stopSessionsPoll() {
    if (sessPollTimer) {
      clearInterval(sessPollTimer);
      sessPollTimer = null;
    }
  }

  // Build the Sessions layout into the body, then load + poll the clients API.
  function openSessions() {
    const root = document.getElementById("server-cards");
    if (!root) return;
    root.innerHTML = `
      <div class="sess">
        <div class="sess__clients">
          <div class="sess__col-header">
            <h2 class="sess__col-title">Clients</h2>
            <div class="sess__search">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>
              <input type="text" id="sess-client-search" placeholder="Search clients…" autocomplete="off">
            </div>
          </div>
          <div class="sess__col-body" id="sess-client-list"><p class="muted">Loading…</p></div>
        </div>
        <div class="sess__sessions">
          <div class="sess__col-header">
            <h2 class="sess__col-title">Sessions</h2>
            <div id="sess-name-editor" class="sess-name-editor"></div>
          </div>
          <div class="sess__col-body" id="sess-session-list"><p class="muted">Select a client.</p></div>
        </div>
        <div class="sess__view" id="sess-detail">
          <div class="sess__empty"><p class="muted">Select a session to inspect its exchanges.</p></div>
        </div>
      </div>`;

    const search = document.getElementById("sess-client-search");
    if (search) search.addEventListener("input", sessRenderClients);

    // Re-render any prior selection immediately (keeps state across nav trips).
    sessRenderClients();
    if (sessSelectedClient) {
      sessRenderNameEditor();
      sessRenderSessions();
    }

    stopSessionsPoll();
    sessRefreshClients();
    sessPollTimer = setInterval(() => {
      if (currentView === "sessions") sessRefreshClients();
    }, 5000);
  }

  // ==========================================================================
  // Compare view — two MCP sessions side-by-side. Reads ?a=cid:sid&b=cid:sid
  // from the URL; if missing, auto-picks the most recent /mcp and /mcp-proxy
  // sessions. Renders two exchange tables + a summary; clicking a row opens the
  // request/response pair. Read-only, driven by /admin/api/sessions*.
  // ==========================================================================
  let cmpAllSessions = []; // flat [{cid, client_name, sid, started, upstream, ...}]
  let cmpA = [];
  let cmpB = [];

  function cmpEndpointBadge(upstream) {
    if (!upstream) return '<span class="muted">—</span>';
    const color = upstream === "/mcp-proxy" ? "purple" : "lime";
    return `<span class="pill pill--${color}"><span class="dot dot--${color}"></span><span class="mono">${esc(upstream)}</span></span>`;
  }

  function cmpParsePair(s) {
    if (!s) return null;
    const i = s.indexOf(":");
    if (i < 0) return null;
    return { cid: s.slice(0, i), sid: s.slice(i + 1) };
  }

  function cmpGetQuery() {
    const u = new URL(window.location.href);
    return {
      a: cmpParsePair(u.searchParams.get("a")),
      b: cmpParsePair(u.searchParams.get("b")),
    };
  }

  async function cmpFetchSessionsIndex() {
    const r = await fetch("/admin/api/sessions");
    if (!r.ok) throw new Error("sessions index: HTTP " + r.status);
    const { clients } = await r.json();
    const flat = [];
    for (const c of clients || []) {
      for (const s of c.sessions || []) {
        flat.push({
          cid: c.client_id,
          client_name: c.name || c.client_name || "—",
          sid: s.id,
          started: s.started_at_ms,
          last_seen: s.last_seen_ms,
          request_count: s.request_count || 0,
          upstream: s.upstream || null,
        });
      }
    }
    flat.sort((x, y) => (y.last_seen || 0) - (x.last_seen || 0));
    return flat;
  }

  function cmpPopulatePicker(selectEl, sessions, defaultKey) {
    selectEl.innerHTML = sessions
      .map((s) => {
        const key = `${s.cid}:${s.sid}`;
        const sel = defaultKey === key ? " selected" : "";
        const label = `${s.upstream || "?"} · ${s.client_name} · ${s.sid.slice(0, 10)} · ${s.request_count} req`;
        return `<option value="${esc(key)}"${sel}>${esc(label)}</option>`;
      })
      .join("");
  }

  function cmpAutoPickDefaults(sessions, query) {
    let a = query.a ? `${query.a.cid}:${query.a.sid}` : null;
    let b = query.b ? `${query.b.cid}:${query.b.sid}` : null;
    if (!a || !b) {
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

  async function cmpFetchDump(cid, sid) {
    const r = await fetch(
      `/admin/api/sessions/${encodeURIComponent(cid)}/dump?session_id=${encodeURIComponent(sid)}&limit=500`
    );
    if (!r.ok) throw new Error("dump fetch: HTTP " + r.status);
    const data = await r.json();
    return normalizeDumpRows(data);
  }

  function normalizeDumpRows(data) {
    if (Array.isArray(data.spans)) {
      return data.spans.map((s) => {
        const a = s.attributes || {};
        return {
          seq: s.span_id,
          direction: a["mcp.direction"] || "C2S",
          kind: a["mcp.kind"] || "Request",
          method: a["mcp.method"] || s.name || "",
          jsonrpc_id: a["mcp.jsonrpc_id"] ?? null,
          body: a["mcp.body"] ?? {},
          latency_ms: a["mcp.latency_ms"] || null,
          upstream: a["mcp.endpoint"] || null,
          timestamp_ms: s.end_time_unix_ms || s.start_time_unix_ms,
        };
      });
    }
    return data.exchanges || [];
  }

  function cmpBodyBytes(ex) {
    try {
      return JSON.stringify(ex.body).length;
    } catch (e) {
      return 0;
    }
  }

  function cmpSummarize(exs) {
    let c2s = 0;
    let s2c = 0;
    let bytes = 0;
    let requests = 0;
    const methods = new Set();
    for (const e of exs) {
      bytes += cmpBodyBytes(e);
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

  function cmpFmtBytes(n) {
    if (n < 1024) return n + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KiB";
    return (n / 1024 / 1024).toFixed(2) + " MiB";
  }

  function cmpRenderSummary(a, b) {
    const root = document.getElementById("cmp-summary");
    if (!root) return;
    const sa = cmpSummarize(a);
    const sb = cmpSummarize(b);
    function cell(label, va, vb) {
      const cls = va === vb ? "" : "row--warn";
      return `<tr class="${cls}"><th>${esc(label)}</th><td>${esc(String(va))}</td><td>${esc(String(vb))}</td></tr>`;
    }
    root.innerHTML = `
      <table class="cmp-table">
        <thead><tr><th></th><th>Session A ${cmpEndpointBadge(sa.upstream)}</th><th>Session B ${cmpEndpointBadge(sb.upstream)}</th></tr></thead>
        <tbody>
          ${cell("Total exchanges", sa.total, sb.total)}
          ${cell("JSON-RPC requests", sa.requests, sb.requests)}
          ${cell("C2S frames", sa.c2s, sb.c2s)}
          ${cell("S2C frames", sa.s2c, sb.s2c)}
          ${cell("Body bytes (sum)", cmpFmtBytes(sa.bytes), cmpFmtBytes(sb.bytes))}
          ${cell("Distinct methods", sa.methods.length, sb.methods.length)}
          ${cell("Methods", sa.methods.join(", ") || "—", sb.methods.join(", ") || "—")}
        </tbody>
      </table>`;
  }

  function cmpRenderGrid(gridId, exs, side) {
    const grid = document.getElementById(gridId);
    if (!grid) return;
    if (!exs.length) {
      grid.innerHTML = '<p class="muted"><em>No exchanges.</em></p>';
      return;
    }
    grid.innerHTML =
      '<table class="cmp-table"><thead><tr>' +
      "<th>#</th><th>Dir</th><th>Method</th><th>ID</th><th>Bytes</th><th>Latency</th>" +
      "</tr></thead><tbody>" +
      exs
        .map((e, i) => {
          const arrow = e.direction === "C2S"
            ? '<span class="xchg__arrow xchg__arrow--blue">→</span>'
            : '<span class="xchg__arrow xchg__arrow--magenta">←</span>';
          const id = e.jsonrpc_id !== null && e.jsonrpc_id !== undefined ? JSON.stringify(e.jsonrpc_id) : "";
          const lat = e.latency_ms ? e.latency_ms + "ms" : "";
          return `<tr class="cmp-row" data-side="${side}" data-idx="${i}">
            <td>${i + 1}</td>
            <td>${arrow}</td>
            <td><code>${esc(e.method || "")}</code></td>
            <td><code>${esc(id)}</code></td>
            <td>${cmpFmtBytes(cmpBodyBytes(e))}</td>
            <td>${esc(lat)}</td>
          </tr>`;
        })
        .join("") +
      "</tbody></table>";
    grid.querySelectorAll("tr[data-idx]").forEach((tr) => {
      tr.addEventListener("click", () => cmpShowPair(tr.dataset.side, parseInt(tr.dataset.idx, 10)));
    });
  }

  function cmpShowPair(side, idx) {
    const all = side === "a" ? cmpA : cmpB;
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
    const panels = document.getElementById("cmp-exchange-panels");
    if (panels) panels.style.display = "";
    const reqEl = document.getElementById("cmp-req-" + side);
    const respEl = document.getElementById("cmp-resp-" + side);
    if (reqEl) reqEl.textContent = req ? JSON.stringify(req.body, null, 2) : "—";
    if (respEl) respEl.textContent = resp ? JSON.stringify(resp.body, null, 2) : "(no response yet)";
  }

  async function cmpLoadAndRender(aKey, bKey) {
    const status = document.getElementById("cmp-status");
    if (status) status.textContent = "loading…";
    try {
      const a = cmpParsePair(aKey);
      const b = cmpParsePair(bKey);
      if (!a || !b) {
        if (status) status.textContent = "pick two sessions";
        return;
      }
      const [exA, exB] = await Promise.all([
        cmpFetchDump(a.cid, a.sid),
        cmpFetchDump(b.cid, b.sid),
      ]);
      cmpA = exA;
      cmpB = exB;
      const meta = new Map(cmpAllSessions.map((s) => [`${s.cid}:${s.sid}`, s]));
      const ma = meta.get(aKey);
      const mb = meta.get(bKey);
      const titleA = document.getElementById("cmp-title-a");
      const titleB = document.getElementById("cmp-title-b");
      if (titleA) {
        titleA.innerHTML =
          "A · " + (ma ? esc(ma.client_name) : "?") + " " + cmpEndpointBadge((ma && ma.upstream) || cmpSummarize(exA).upstream);
      }
      if (titleB) {
        titleB.innerHTML =
          "B · " + (mb ? esc(mb.client_name) : "?") + " " + cmpEndpointBadge((mb && mb.upstream) || cmpSummarize(exB).upstream);
      }
      cmpRenderSummary(exA, exB);
      cmpRenderGrid("cmp-grid-a", exA, "a");
      cmpRenderGrid("cmp-grid-b", exB, "b");
      if (status) status.textContent = `loaded ${exA.length} + ${exB.length} exchanges`;
      // Reflect picked sessions in the URL so the comparison is shareable.
      const u = new URL(window.location.href);
      u.searchParams.set("a", aKey);
      u.searchParams.set("b", bKey);
      window.history.replaceState({}, "", u.toString());
    } catch (e) {
      if (status) status.textContent = "error: " + e.message;
    }
  }

  // Build the Compare layout into the body, then load the sessions index and
  // auto-compare the two most-recent endpoints.
  async function openCompare() {
    const root = document.getElementById("server-cards");
    if (!root) return;
    root.innerHTML = `
      <div class="cmp">
        <div class="cmp__head">
          <h1 class="cmp__title">Compare sessions</h1>
          <p class="cmp__subtitle">Side-by-side dump of two MCP sessions. If no <code>?a=cid:sid&amp;b=cid:sid</code> is given, the two most recently active sessions are picked.</p>
        </div>
        <div class="cmp__pickers">
          <div class="cmp__picker">
            <label class="field-label">Session A</label>
            <select id="cmp-picker-a" class="input"></select>
          </div>
          <div class="cmp__picker">
            <label class="field-label">Session B</label>
            <select id="cmp-picker-b" class="input"></select>
          </div>
        </div>
        <div class="cmp__actions">
          <button id="cmp-load-btn" class="btn btn--primary">Load &amp; compare</button>
          <button id="cmp-swap-btn" class="btn btn--ghost">Swap A ↔ B</button>
          <span id="cmp-status" class="muted"></span>
        </div>
        <div id="cmp-summary" class="cmp__summary"></div>
        <div class="cmp__grids">
          <div class="cmp__col">
            <div class="cmp__col-title" id="cmp-title-a">Session A</div>
            <div id="cmp-grid-a"></div>
          </div>
          <div class="cmp__col">
            <div class="cmp__col-title" id="cmp-title-b">Session B</div>
            <div id="cmp-grid-b"></div>
          </div>
        </div>
        <div class="cmp__panels" id="cmp-exchange-panels" style="display:none">
          <div class="cmp__col">
            <h5 class="cmp__panel-label">A · request</h5><pre class="code-block" id="cmp-req-a">—</pre>
            <h5 class="cmp__panel-label">A · response</h5><pre class="code-block" id="cmp-resp-a">—</pre>
          </div>
          <div class="cmp__col">
            <h5 class="cmp__panel-label">B · request</h5><pre class="code-block" id="cmp-req-b">—</pre>
            <h5 class="cmp__panel-label">B · response</h5><pre class="code-block" id="cmp-resp-b">—</pre>
          </div>
        </div>
      </div>`;

    const status = document.getElementById("cmp-status");
    try {
      cmpAllSessions = await cmpFetchSessionsIndex();
    } catch (e) {
      if (status) status.textContent = "couldn't load sessions: " + e.message;
      return;
    }
    if (!cmpAllSessions.length) {
      if (status) status.textContent = "no sessions yet — connect an MCP client to /mcp (and /mcp-proxy) first";
      return;
    }
    const q = cmpGetQuery();
    const { a, b } = cmpAutoPickDefaults(cmpAllSessions, q);
    const pickerA = document.getElementById("cmp-picker-a");
    const pickerB = document.getElementById("cmp-picker-b");
    cmpPopulatePicker(pickerA, cmpAllSessions, a);
    cmpPopulatePicker(pickerB, cmpAllSessions, b);
    document.getElementById("cmp-load-btn").addEventListener("click", () => {
      cmpLoadAndRender(pickerA.value, pickerB.value);
    });
    document.getElementById("cmp-swap-btn").addEventListener("click", () => {
      const av = pickerA.value;
      pickerA.value = pickerB.value;
      pickerB.value = av;
      cmpLoadAndRender(pickerA.value, pickerB.value);
    });
    if (a && b) {
      cmpLoadAndRender(a, b);
    } else if (status) {
      status.textContent = "pick two sessions and click Load & compare";
    }
  }

  // ---------- Schema / Notifications (full-width body views) ----------
  async function openSchema() {
    const root = document.getElementById("server-cards");
    if (!root) return;
    root.innerHTML =
      '<div class="page-header"><h1 class="page-title">GraphQL schema</h1>' +
      '<p class="page-subtitle">Live SDL from the hot-swapped schema.</p></div>' +
      '<pre class="code-block" id="sdl">Loading…</pre>';
    try {
      const r = await fetch("/admin/api/schema.graphql");
      if (!r.ok) throw new Error("HTTP " + r.status);
      const sdl = await r.text();
      const el = document.getElementById("sdl");
      if (el) el.textContent = sdl;
    } catch (e) {
      const el = document.getElementById("sdl");
      if (el) el.textContent = "Failed to load schema: " + e;
    }
  }

  async function openNotifications() {
    const root = document.getElementById("server-cards");
    if (!root) return;
    root.innerHTML =
      '<div class="page-header"><h1 class="page-title">Notifications</h1>' +
      '<p class="page-subtitle">In-memory ring buffer of upstream MCP notifications.</p></div>' +
      '<div class="skill-detail__actions"><button type="button" class="btn btn--sm" id="notif-refresh">Refresh</button></div>' +
      '<div id="notif-grid"><p class="muted">Loading…</p></div>';
    const btn = document.getElementById("notif-refresh");
    if (btn) btn.addEventListener("click", loadNotifications);
    await loadNotifications();
  }

  async function loadNotifications() {
    const grid = document.getElementById("notif-grid");
    if (!grid) return;
    try {
      const res = await fetch("/admin/api/notifications?limit=200");
      if (!res.ok) throw new Error("HTTP " + res.status);
      const { notifications } = await res.json();
      if (!notifications || !notifications.length) {
        grid.innerHTML = '<p class="muted"><em>No notifications yet.</em></p>';
        return;
      }
      grid.innerHTML =
        '<table class="table"><thead><tr>' +
        "<th>ID</th><th>Source</th><th>Method</th><th>Params</th><th>Time</th>" +
        "</tr></thead><tbody>" +
        notifications
          .map((n) => {
            let params = "";
            try {
              params = JSON.stringify(n.params, null, 2);
            } catch (e) {
              params = String(n.params);
            }
            let time = "";
            try {
              time = new Date(n.ts_unix_ms).toISOString();
            } catch (e) {
              time = String(n.ts_unix_ms);
            }
            return (
              "<tr>" +
              "<td><code>" +
              esc(n.id) +
              "</code></td>" +
              "<td><code>" +
              esc(n.source) +
              "</code></td>" +
              "<td><code>" +
              esc(n.method) +
              "</code></td>" +
              '<td><pre class="code-block" style="max-height:200px;margin:0">' +
              esc(params) +
              "</pre></td>" +
              '<td class="muted">' +
              esc(time) +
              "</td>" +
              "</tr>"
            );
          })
          .join("") +
        "</tbody></table>";
    } catch (e) {
      grid.innerHTML =
        '<p class="danger">Failed to load notifications: ' + esc(String(e)) + "</p>";
    }
  }

  // ---------- nav / view switching ----------
  function setView(view) {
    const known = ["services", "skills", "sessions", "compare", "schema", "notifications"];
    if (known.indexOf(view) < 0) return;

    // Leaving the Sessions view: stop its background poll so it doesn't keep
    // hitting the API from a hidden view.
    if (currentView === "sessions" && view !== "sessions") stopSessionsPoll();
    currentView = view;

    document.querySelectorAll(".side1__nav .nav-link[data-nav]").forEach((el) => {
      el.classList.toggle("active", el.dataset.nav === view);
    });

    // Sidebar #2 is used by Services (servers) and Skills (skills list).
    // Sessions / Compare / Schema / Notifications hide it (full-width body).
    const side2 = document.getElementById("side2");
    if (side2) {
      side2.style.display = view === "services" || view === "skills" ? "" : "none";
    }

    // Sessions renders a full-height 3-column layout that needs the body's
    // default padding + vertical scroll removed.
    const body = document.getElementById("server-cards");
    if (body) body.classList.toggle("body--flush", view === "sessions");

    if (view === "skills") {
      hideModal();
      ensureSkillsSide2();
      if (skills === null && !skillsLoading) {
        loadSkills();
      } else {
        const search = document.getElementById("skill-search");
        renderSkillList(search ? search.value : "");
        if (selectedSkill) selectSkill(selectedSkill);
        else if (Array.isArray(skills) && skills.length) selectSkill(skills[0].name);
        else if (body) {
          body.innerHTML =
            '<div class="empty-box">No skills loaded. Click “+ New skill” or drop a YAML file into the skills directory.</div>';
        }
      }
    } else if (view === "sessions") {
      hideModal();
      openSessions();
    } else if (view === "compare") {
      hideModal();
      openCompare();
    } else if (view === "schema") {
      hideModal();
      openSchema();
    } else if (view === "notifications") {
      hideModal();
      openNotifications();
    } else {
      // Services — restore server list sidebar and selected card.
      ensureServersSide2();
      renderList(document.getElementById("server-search")?.value || "");
      if (selected) renderCard(selected);
      else if (servers.length) selectServer(servers[0].name);
    }
  }

  function bindNav() {
    document.querySelectorAll(".side1__nav .nav-link[data-nav]").forEach((el) => {
      el.addEventListener("click", (e) => {
        e.preventDefault();
        const nav = el.dataset.nav;
        if (
          nav === "services" ||
          nav === "skills" ||
          nav === "sessions" ||
          nav === "compare" ||
          nav === "schema" ||
          nav === "notifications"
        ) {
          setView(nav);
        }
      });
    });
  }

  async function init() {
    bindNav();
    bindModal();
    await loadServers();
    setView("services");
  }

  document.addEventListener("DOMContentLoaded", init);
})();
