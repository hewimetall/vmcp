// Skills CRUD + preview UI. Backed by /admin/api/skills (sibling agent A).
// No inline scripts anywhere — all logic in here so the CSP can keep
// `script-src` tight.

(function () {
  "use strict";

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );
  }

  function previewTemplate(t) {
    if (!t) return "";
    return t.length <= 200 ? t : t.slice(0, 200) + "…";
  }

  // ------- list / render -------

  async function loadSkills() {
    const container = document.getElementById("skills");
    if (!container) return;
    container.innerHTML = '<p class="text-muted"><em>Loading…</em></p>';
    try {
      const r = await fetch("/admin/api/skills");
      if (!r.ok) throw new Error("HTTP " + r.status);
      const data = await r.json();
      if (!data.skills || !data.skills.length) {
        container.innerHTML =
          '<p class="text-muted"><em>No skills loaded. Click "+ New skill" or drop a YAML file into the skills directory.</em></p>';
        return;
      }
      container.innerHTML = data.skills.map(renderSkillCard).join("");
      data.skills.forEach((s) => bindCardHandlers(s));
    } catch (e) {
      container.innerHTML =
        '<p class="text-danger">Failed to load skills: ' + escapeHtml(String(e)) + "</p>";
    }
  }

  function renderArgRow(a) {
    const req = a.required ? '<span class="badge bg-red ms-1">required</span>' : "";
    const desc = a.description
      ? ' — <span class="text-muted">' + escapeHtml(a.description) + "</span>"
      : "";
    return "<li><code>" + escapeHtml(a.name) + "</code>" + req + desc + "</li>";
  }

  function renderSkillCard(s) {
    const argsHtml = (s.arguments && s.arguments.length)
      ? "<ul>" + s.arguments.map(renderArgRow).join("") + "</ul>"
      : '<em class="text-muted">no arguments</em>';
    const id = "skill-" + encodeURIComponent(s.name);
    return `<div class="card mb-2" data-skill="${escapeHtml(s.name)}">
      <div class="card-header">
        <h3 class="card-title">${escapeHtml(s.name)}</h3>
        <div class="card-actions">
          <button class="btn btn-sm" data-action="edit">Edit</button>
          <button class="btn btn-sm" data-action="duplicate">Duplicate</button>
          <button class="btn btn-sm btn-danger" data-action="delete">Delete</button>
          <button class="btn btn-sm btn-outline-primary" data-action="preview">Preview ▾</button>
        </div>
      </div>
      <div class="card-body">
        <p>${escapeHtml(s.description || "")}</p>
        <h4>Arguments</h4>${argsHtml}
        <h4>Template (preview)</h4>
        <pre>${escapeHtml(s.template_preview || previewTemplate(s.template))}</pre>
        <div class="d-none mt-3" id="${id}-preview">
          <label class="form-label">Args (JSON)</label>
          <textarea class="form-control mb-2" rows="3" id="${id}-args">{}</textarea>
          <button class="btn btn-sm btn-primary" data-action="generate">Generate</button>
          <pre class="mt-2 d-none" id="${id}-result"></pre>
        </div>
      </div>
    </div>`;
  }

  function bindCardHandlers(skill) {
    const card = document.querySelector(`[data-skill="${cssEscape(skill.name)}"]`);
    if (!card) return;
    card.querySelectorAll("[data-action]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const action = btn.getAttribute("data-action");
        if (action === "edit") openSkillModal("edit", skill);
        else if (action === "duplicate") openSkillModal("duplicate", skill);
        else if (action === "delete") deleteSkill(skill.name);
        else if (action === "preview") togglePreview(skill.name);
        else if (action === "generate") generateSkill(skill.name);
      });
    });
  }

  function cssEscape(s) {
    // Modern browsers expose CSS.escape; fall back to a basic version.
    return window.CSS && CSS.escape ? CSS.escape(s) : String(s).replace(/[^a-zA-Z0-9_-]/g, "\\$&");
  }

  function togglePreview(name) {
    const id = "skill-" + encodeURIComponent(name);
    const panel = document.getElementById(id + "-preview");
    if (panel) panel.classList.toggle("d-none");
  }

  async function generateSkill(name) {
    const id = "skill-" + encodeURIComponent(name);
    const argsEl = document.getElementById(id + "-args");
    const resultEl = document.getElementById(id + "-result");
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
      const r = await fetch(`/admin/api/skills/${encodeURIComponent(name)}/generate`, {
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

  // ------- modal: new / edit / duplicate -------

  function showModal() {
    const m = document.getElementById("skill-modal");
    m.classList.add("show");
    m.style.display = "block";
    m.setAttribute("aria-hidden", "false");
  }
  function hideModal() {
    const m = document.getElementById("skill-modal");
    m.classList.remove("show");
    m.style.display = "none";
    m.setAttribute("aria-hidden", "true");
    setError("");
  }

  function setError(msg) {
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

  function renderArgsEditor(args) {
    const container = document.getElementById("skill-modal-args");
    container.innerHTML = "";
    (args || []).forEach((a, i) => container.appendChild(argRow(a, i)));
  }

  function argRow(a, i) {
    const row = document.createElement("div");
    row.className = "row g-2 align-items-center mb-2";
    row.innerHTML = `
      <div class="col-3"><input type="text" class="form-control" placeholder="name" data-arg-field="name" value="${escapeHtml(a.name || "")}"></div>
      <div class="col"><input type="text" class="form-control" placeholder="description" data-arg-field="description" value="${escapeHtml(a.description || "")}"></div>
      <div class="col-auto"><label class="form-check form-check-inline"><input class="form-check-input" type="checkbox" data-arg-field="required" ${a.required ? "checked" : ""}> required</label></div>
      <div class="col-auto"><button type="button" class="btn btn-sm btn-outline-danger" data-arg-remove>×</button></div>`;
    row.querySelector("[data-arg-remove]").addEventListener("click", () => row.remove());
    return row;
  }

  function collectArgs() {
    const rows = document.querySelectorAll("#skill-modal-args .row");
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
    document.getElementById("skill-modal-original-name").value = (skill && mode === "edit") ? skill.name : "";
    document.getElementById("skill-modal-title").textContent =
      mode === "edit" ? "Edit skill" : mode === "duplicate" ? "Duplicate skill" : "New skill";
    document.getElementById("skill-modal-name").value =
      mode === "duplicate" ? (skill.name + "_copy") : (skill && skill.name) || "";
    document.getElementById("skill-modal-name").disabled = mode === "edit";
    document.getElementById("skill-modal-description").value = (skill && skill.description) || "";
    document.getElementById("skill-modal-template").value =
      (skill && (skill.template || skill.template_preview)) || "";
    renderArgsEditor((skill && skill.arguments) || []);
    showModal();
  }

  async function saveSkill() {
    const mode = document.getElementById("skill-modal-mode").value;
    const name = document.getElementById("skill-modal-name").value.trim();
    const description = document.getElementById("skill-modal-description").value.trim();
    const template = document.getElementById("skill-modal-template").value;
    const args = collectArgs();
    if (!name) return setError("name is required");
    if (!template) return setError("template is required");
    const payload = { name, description, template, arguments: args };
    const url = mode === "edit"
      ? `/admin/api/skills/${encodeURIComponent(document.getElementById("skill-modal-original-name").value)}`
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
        await loadSkills();
      } else {
        const txt = await r.text();
        setError(`save failed (${r.status}): ${txt}`);
      }
    } catch (e) {
      setError("request failed: " + e);
    }
  }

  async function deleteSkill(name) {
    if (!confirm(`Delete skill "${name}"?`)) return;
    try {
      const r = await fetch(`/admin/api/skills/${encodeURIComponent(name)}`, { method: "DELETE" });
      if (r.ok || r.status === 204) {
        await loadSkills();
      } else {
        const txt = await r.text();
        alert(`delete failed (${r.status}): ${txt}`);
      }
    } catch (e) {
      alert("request failed: " + e);
    }
  }

  // ------- init -------

  document.addEventListener("DOMContentLoaded", () => {
    const newBtn = document.getElementById("new-skill-btn");
    if (newBtn) newBtn.addEventListener("click", () => openSkillModal("new", null));
    const closeBtn = document.getElementById("skill-modal-close");
    if (closeBtn) closeBtn.addEventListener("click", hideModal);
    const cancelBtn = document.getElementById("skill-modal-cancel");
    if (cancelBtn) cancelBtn.addEventListener("click", hideModal);
    const saveBtn = document.getElementById("skill-modal-save");
    if (saveBtn) saveBtn.addEventListener("click", saveSkill);
    const addArgBtn = document.getElementById("skill-modal-add-arg");
    if (addArgBtn) addArgBtn.addEventListener("click", () => {
      const c = document.getElementById("skill-modal-args");
      c.appendChild(argRow({}, c.children.length));
    });
    loadSkills();
  });
})();
