"use strict";

const $ = (selector) => document.querySelector(selector);
const states = [
  "backlog",
  "ready",
  "in_progress",
  "blocked",
  "in_review",
  "done",
  "canceled",
];
const stateNames = {
  backlog: "Backlog",
  ready: "Ready",
  in_progress: "In progress",
  blocked: "Blocked",
  in_review: "In review",
  done: "Done",
  canceled: "Canceled",
};
const kindSymbols = {
  epic: "ϟ",
  story: "▤",
  task: "✓",
  bug: "◆",
  subtask: "↳",
  spike: "◇",
};
const params = new URLSearchParams(location.search);
let token = sessionStorage.getItem("workboard-key") || "";
let selectedProject = params.get("project") || "";
let view = ["board", "agents", "activity"].includes(params.get("view"))
  ? params.get("view")
  : "board";
let snapshot = null;
let signature = "";
let refreshing = false;
let lastRefresh = 0;
let streamLive = false;
let streamToken = "";
let streamAbort = null;
let wakeTimer = 0;
let detailKey = null;
let detail = null;
let detailVersion = 0;
let detailSignature = "";
let activityExtra = [];
let activityCursor = null;
let activityLoaded = false;
let serverOffset = 0;
let metricsExpired = -1;
let pendingIssue = params.get("issue");

function h(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key.startsWith("on"))
      node.addEventListener(key.slice(2).toLowerCase(), value);
    else if (key === "className") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (value !== null && value !== undefined)
      node.setAttribute(key, value);
  }
  for (const child of children.flat(Infinity)) {
    if (child !== null && child !== undefined)
      node.append(
        child instanceof Node ? child : document.createTextNode(String(child)),
      );
  }
  return node;
}
function empty(title, message, icon = "▦") {
  return h(
    "div",
    { className: "empty-state" },
    h("div", { className: "empty-icon", "aria-hidden": "true" }, icon),
    h("h2", {}, title),
    h("p", {}, message),
  );
}
function initials(name) {
  return name
    .split(/[\s_-]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((p) => p[0])
    .join("")
    .toUpperCase();
}
function avatar(id) {
  return h(
    "span",
    { className: "avatar", "aria-hidden": "true" },
    initials(id),
  );
}
function agentName(id) {
  return snapshot?.agents.find((a) => a.id === id)?.name || id || "Unassigned";
}
function kindBadge(kind) {
  return h(
    "span",
    { className: "issue-kind" },
    h(
      "span",
      { className: `kind-symbol ${kind}`, "aria-hidden": "true" },
      kindSymbols[kind] || "◇",
    ),
    kind,
  );
}
function stateBadge(state) {
  return h(
    "span",
    { className: "state-badge" },
    h("span", { className: `status-dot ${state}` }),
    stateNames[state] || state,
  );
}
function priority(p) {
  return h(
    "span",
    { className: `priority ${p}` },
    h(
      "span",
      { "aria-hidden": "true" },
      p === "critical" ? "⇈" : p === "high" ? "↑" : p === "low" ? "↓" : "−",
    ),
    p,
  );
}
function absoluteTime(ts) {
  return new Date(ts * 1000).toLocaleString();
}
function age(ts) {
  const elapsed = Math.max(
    0,
    Math.floor((Date.now() + serverOffset) / 1000) - ts,
  );
  if (elapsed < 60) return "just now";
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)}m ago`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)}h ago`;
  return `${Math.floor(elapsed / 86400)}d ago`;
}
function timeNode(ts) {
  return h(
    "time",
    { datetime: new Date(ts * 1000).toISOString(), title: absoluteTime(ts) },
    age(ts),
  );
}
function issueLink(key, title) {
  return h(
    "button",
    { className: "issue-link", onClick: () => openIssue(key) },
    title || key,
  );
}
function updateUrl() {
  const url = new URL(location.href);
  url.search = "";
  if (selectedProject) url.searchParams.set("project", selectedProject);
  if (view !== "board") url.searchParams.set("view", view);
  if (detailKey) url.searchParams.set("issue", detailKey);
  history.replaceState(null, "", url);
}
function connection(ok, text) {
  $("#connection").textContent = text;
  $("#connection-dot").classList.toggle("offline", !ok);
}
async function api(path, key = token) {
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${key}` },
    cache: "no-store",
    signal: AbortSignal.timeout(15000),
  });
  if (response.status === 401) {
    const error = new Error(
      "Your API key is invalid, revoked, or belongs to an inactive agent.",
    );
    error.unauthorized = true;
    throw error;
  }
  if (!response.ok) {
    let message = `Request failed (${response.status})`;
    try {
      const body = await response.json();
      message = body.error?.message || message;
    } catch {
      /* HTTP fallback */
    }
    throw new Error(message);
  }
  return response.json();
}
function authPrompt(message = "") {
  stopStream();
  token = "";
  sessionStorage.removeItem("workboard-key");
  snapshot = null;
  signature = "";
  detailVersion++;
  detailKey = null;
  detail = null;
  activityExtra = [];
  if ($("#issue-dialog").open) $("#issue-dialog").close();
  $("#detail-content").replaceChildren();
  $("#metrics").replaceChildren();
  $("#content").replaceChildren(
    empty(
      "Connect to your workboard",
      "Use an assigned API key to view projects and follow your agents.",
    ),
  );
  $("#project").replaceChildren(
    h("option", { value: "" }, "Connect to a project"),
  );
  $("#breadcrumb").textContent = "All work";
  $("#error").hidden = true;
  $("#issue-count").textContent = "0";
  $("#nav-agent-count").textContent = "0";
  connection(false, "Not connected");
  $("#auth-error").textContent = message;
  $("#auth-error").hidden = !message;
  if (!$("#auth-dialog").open) $("#auth-dialog").showModal();
}
function replaceOptions(select, values, first) {
  const previous = select.value;
  select.replaceChildren(
    h("option", { value: "" }, first),
    ...values.map(([value, text]) => h("option", { value }, text)),
  );
  if (values.some(([value]) => value === previous)) select.value = previous;
}
function updateSelectors() {
  $("#project").replaceChildren(
    ...snapshot.projects.map((p) =>
      h("option", { value: p.key }, `${p.key} · ${p.name}`),
    ),
  );
  if (!snapshot.projects.length)
    $("#project").append(h("option", { value: "" }, "No projects yet"));
  $("#project").value = selectedProject;
  replaceOptions(
    $("#assignee"),
    [
      ["__unassigned", "Unassigned"],
      ...snapshot.agents.map((a) => [
        a.id,
        a.name + (a.active ? "" : " (inactive)"),
      ]),
    ],
    "All agents",
  );
  replaceOptions(
    $("#sprint"),
    snapshot.sprints.map((s) => [s.id, `${s.name} · ${s.state}`]),
    "All sprints",
  );
  $("#breadcrumb").textContent =
    snapshot.projects.find((p) => p.key === selectedProject)?.name ||
    "All work";
  $("#nav-agent-count").textContent = snapshot.agents.filter(
    (a) => a.active,
  ).length;
}
// Live updates: the server pushes one event per change, and each burst of events
// triggers a single refresh. A fetch stream is used because EventSource cannot
// send the Authorization header. Polling continues as a slower safety net, and is
// the only path if the stream is unavailable.
function wake() {
  clearTimeout(wakeTimer);
  wakeTimer = setTimeout(() => {
    if (!document.hidden) refresh();
  }, 150);
}
function stopStream() {
  streamAbort?.abort();
  streamAbort = null;
  streamToken = "";
  streamLive = false;
  clearTimeout(wakeTimer);
}
async function followEvents() {
  if (!token || streamToken === token) return;
  stopStream();
  const mine = token;
  const controller = new AbortController();
  streamAbort = controller;
  streamToken = mine;
  let delay = 1000;
  try {
    while (!controller.signal.aborted && token === mine) {
      try {
        const response = await fetch("/api/events/stream", {
          headers: { Authorization: `Bearer ${mine}` },
          cache: "no-store",
          signal: controller.signal,
        });
        // Refresh handles invalid keys, which returns the user to sign-in.
        if (response.status === 401 || response.status === 403) return;
        if (!response.ok || !response.body) throw new Error("stream unavailable");
        streamLive = true;
        delay = 1000;
        wake(); // Cover anything that changed while disconnected.
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let end;
          while ((end = buffer.indexOf("\n\n")) >= 0) {
            const block = buffer.slice(0, end);
            buffer = buffer.slice(end + 2);
            if (/^id:/m.test(block)) wake();
          }
        }
      } catch {
        if (controller.signal.aborted) return;
      }
      streamLive = false;
      await new Promise((resolve) => setTimeout(resolve, delay));
      delay = Math.min(delay * 2, 30000);
    }
  } finally {
    if (streamAbort === controller) {
      streamAbort = null;
      streamToken = "";
      streamLive = false;
    }
  }
}
async function refresh() {
  if (!token || refreshing) return;
  refreshing = true;
  lastRefresh = Date.now();
  $("#refresh").disabled = true;
  const requestedProject = selectedProject;
  const requestedToken = token;
  try {
    const data = await api(
      `/api/overview${selectedProject ? `?project=${encodeURIComponent(selectedProject)}` : ""}`,
    );
    if (token !== requestedToken || selectedProject !== requestedProject)
      return;
    selectedProject = data.project || "";
    serverOffset = data.server_time * 1000 - Date.now();
    const { server_time, ...content } = data;
    const nextSignature = JSON.stringify(content);
    if (activityLoaded && snapshot)
      activityExtra = [
        ...new Map(
          [...activityExtra, ...snapshot.activity.items].map((e) => [e.id, e]),
        ).values(),
      ];
    snapshot = data;
    followEvents();
    $("#error").hidden = true;
    connection(true, "Live");
    $("#updated").textContent =
      `Last synced ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}`;
    if (signature !== nextSignature) {
      signature = nextSignature;
      updateSelectors();
      render();
    }
    updateLeases();
    if (pendingIssue) {
      const key = pendingIssue;
      pendingIssue = null;
      openIssue(key);
    }
    if (detailKey) await refreshDetail();
    updateUrl();
  } catch (error) {
    if (token !== requestedToken || selectedProject !== requestedProject)
      return;
    if (error.unauthorized) authPrompt(error.message);
    else {
      connection(false, "Disconnected");
      $("#error").textContent =
        `Unable to sync: ${error.message}. Showing the last received data. Retrying automatically.`;
      $("#error").hidden = false;
      if (!snapshot)
        $("#content").replaceChildren(
          empty(
            "The workboard is unavailable",
            "Check the server connection, then refresh.",
          ),
        );
    }
  } finally {
    refreshing = false;
    $("#refresh").disabled = false;
  }
}
function filteredIssues() {
  const search = $("#search").value.trim().toLowerCase();
  const assignee = $("#assignee").value;
  return snapshot.issues.filter(
    (i) =>
      (!search ||
        `${i.key} ${i.title} ${i.description} ${i.labels.join(" ")}`
          .toLowerCase()
          .includes(search)) &&
      (!assignee ||
        (assignee === "__unassigned"
          ? !i.assignee
          : i.assignee === assignee)) &&
      (!$("#kind").value || i.kind === $("#kind").value) &&
      (!$("#sprint").value || i.sprint === $("#sprint").value),
  );
}
function renderMetrics() {
  const issues = snapshot.issues;
  const working = issues.filter((i) =>
    ["in_progress", "in_review"].includes(i.state),
  );
  const done = issues.filter((i) => i.state === "done").length;
  const blocked = issues.filter((i) => i.state === "blocked").length;
  const expired = working.filter(
    (i) => i.lease && i.lease.expires_at * 1000 <= Date.now() + serverOffset,
  ).length;
  metricsExpired = expired;
  const busy = new Set(working.map((i) => i.assignee).filter(Boolean));
  const metrics = [
    [
      "Total issues",
      issues.length,
      `${issues.filter((i) => i.kind === "epic").length} epics · ${issues.filter((i) => i.kind === "bug").length} bugs`,
      "▦",
      false,
    ],
    [
      "Agents at work",
      busy.size,
      `${snapshot.agents.filter((a) => a.active).length} active agents in the workspace`,
      "◉",
      false,
    ],
    [
      "In flight",
      working.length,
      `${blocked} blocked${expired ? ` · ${expired} expired ${expired === 1 ? "lease" : "leases"}` : " · progressing through the board"}`,
      "↗",
      blocked + expired > 0,
    ],
    [
      "Completed",
      done,
      `${issues.length ? Math.round((done / issues.length) * 100) : 0}% of project issues are done`,
      "✓",
      false,
    ],
  ];
  $("#metrics").replaceChildren(
    ...metrics.map(([label, value, note, icon, attention]) =>
      h(
        "div",
        { className: "metric" },
        h(
          "div",
          { className: "metric-label" },
          label,
          h(
            "span",
            { className: "metric-symbol", "aria-hidden": "true" },
            icon,
          ),
        ),
        h("div", { className: "metric-value" }, value),
        h(
          "div",
          { className: `metric-note${attention ? " attention" : ""}` },
          note,
        ),
      ),
    ),
  );
}
function leaseNode(lease) {
  return h("div", {
    className: "lease-note",
    "data-expires": lease.expires_at,
  });
}
function updateLeases() {
  if (snapshot) {
    const expired = snapshot.issues.filter(
      (i) => i.lease && i.lease.expires_at * 1000 <= Date.now() + serverOffset,
    ).length;
    if (expired !== metricsExpired) renderMetrics();
  }
  document.querySelectorAll("[data-expires]").forEach((node) => {
    const seconds = Math.ceil(
      Number(node.dataset.expires) - (Date.now() + serverOffset) / 1000,
    );
    node.classList.toggle("expired", seconds <= 0);
    node.textContent =
      seconds <= 0
        ? "◷ Lease expired · awaiting recovery"
        : `◷ Lease · ${seconds >= 60 ? `${Math.floor(seconds / 60)}m ${seconds % 60}s` : `${seconds}s`} remaining`;
  });
}
function issueCard(issue) {
  return h(
    "button",
    {
      className: "issue-card",
      onClick: () => openIssue(issue.key),
      "data-focus": issue.key,
      "aria-label": `${issue.key}: ${issue.title}`,
    },
    h(
      "div",
      { className: "card-top" },
      kindBadge(issue.kind),
      h("span", { className: "mono" }, issue.key),
    ),
    h("div", { className: "card-title" }, issue.title),
    issue.labels.length
      ? h(
          "div",
          { className: "card-labels" },
          issue.labels.map((l) => h("span", { className: "tag" }, l)),
        )
      : null,
    h(
      "div",
      { className: "card-footer" },
      priority(issue.priority),
      h(
        "div",
        { className: "agent-mini" },
        issue.assignee ? avatar(issue.assignee) : null,
        h("span", {}, agentName(issue.assignee)),
      ),
    ),
    issue.lease ? leaseNode(issue.lease) : null,
  );
}
function renderBoard() {
  const items = filteredIssues();
  $("#issue-count").textContent =
    `${items.length}${items.length !== snapshot.issues.length ? ` / ${snapshot.issues.length}` : ""}`;
  if (!snapshot.issues.length) {
    $("#content").replaceChildren(
      empty(
        "A little quiet. Ready for what’s next.",
        "When your agents create issues in this project, their work will appear here.",
      ),
    );
  } else if (!items.length) {
    $("#content").replaceChildren(
      empty(
        "No matching issues",
        "Try another search or clear the filters to see all project work.",
        "⌕",
      ),
    );
  } else {
    const oldScroll = $(".board")?.scrollLeft || 0;
    const focused = document.activeElement?.dataset.focus;
    const board = h("div", {
      className: "board",
      tabindex: "0",
      "aria-label": "Issue board, scroll horizontally for all statuses",
    });
    for (const state of states) {
      const group = items.filter((i) => i.state === state);
      group.sort(
        (a, b) =>
          ["critical", "high", "medium", "low"].indexOf(a.priority) -
          ["critical", "high", "medium", "low"].indexOf(b.priority),
      );
      board.append(
        h(
          "section",
          {
            className: "column",
            "aria-label": `${stateNames[state]}, ${group.length} issues`,
          },
          h(
            "div",
            { className: "column-heading" },
            h("span", { className: `status-dot ${state}` }),
            stateNames[state],
            h("span", { className: "count" }, group.length),
          ),
          group.length
            ? group.map(issueCard)
            : h("div", { className: "column-empty" }, "No issues here"),
        ),
      );
    }
    $("#content").replaceChildren(board);
    board.scrollLeft = oldScroll;
    if (focused)
      Array.from(board.querySelectorAll("[data-focus]"))
        .find((n) => n.dataset.focus === focused)
        ?.focus({ preventScroll: true });
  }
  $("#clear-filters").hidden = !(
    $("#search").value ||
    $("#assignee").value ||
    $("#kind").value ||
    $("#sprint").value
  );
}
function renderAgents() {
  $("#issue-count").textContent = snapshot.agents.length;
  if (!snapshot.agents.length) {
    $("#content").replaceChildren(
      empty(
        "No agents registered",
        "Registered agents and their project workloads will appear here.",
        "◉",
      ),
    );
    return;
  }
  $("#content").replaceChildren(
    h(
      "div",
      { className: "agent-grid" },
      snapshot.agents.map((agent) => {
        const assigned = snapshot.issues.filter((i) => i.assignee === agent.id);
        const working = assigned.filter((i) =>
          ["in_progress", "in_review"].includes(i.state),
        );
        const open = assigned.filter(
          (i) => !["done", "canceled"].includes(i.state),
        );
        return h(
          "article",
          { className: "agent-card" },
          h(
            "div",
            { className: "agent-heading" },
            avatar(agent.id),
            h(
              "div",
              {},
              h("h3", {}, agent.name),
              h("div", { className: "mono" }, agent.id),
            ),
            h(
              "span",
              { className: `agent-status${agent.active ? "" : " inactive"}` },
              agent.active
                ? working.length
                  ? "Working"
                  : "Available"
                : "Inactive",
            ),
          ),
          h(
            "div",
            { className: "capabilities" },
            agent.capabilities.length
              ? agent.capabilities.map((c) =>
                  h("span", { className: "tag" }, c),
                )
              : h("span", { className: "muted" }, "No capabilities listed"),
          ),
          h(
            "div",
            { className: "workload" },
            ...[
              [working.length, "in flight"],
              [open.length, "open"],
              [assigned.filter((i) => i.state === "done").length, "done"],
            ].map(([n, label]) => h("div", {}, h("strong", {}, n), label)),
          ),
          h(
            "div",
            { className: "agent-issues" },
            open
              .slice(0, 6)
              .map((i) => issueLink(i.key, `${i.key} · ${i.title}`)),
            open.length > 6
              ? h(
                  "button",
                  {
                    className: "text-button",
                    onClick: () => {
                      $("#assignee").value = agent.id;
                      setView("board");
                    },
                  },
                  `View all ${open.length} open issues`,
                )
              : null,
          ),
        );
      }),
    ),
  );
}
const actionNames = {
  issue_created: "created an issue",
  issue_updated: "updated an issue",
  issue_assigned: "changed an assignment",
  issue_transitioned: "moved an issue",
  issue_claimed: "claimed work",
  claim_renewed: "renewed a lease",
  claim_released: "released work",
  claim_expired: "recovered an expired lease",
  comment_added: "added a comment",
  link_added: "linked issues",
  link_removed: "removed a link",
  project_created: "created a project",
  agent_registered: "registered an agent",
  agent_active_changed: "changed agent availability",
  sprint_created: "created a sprint",
  sprint_transitioned: "updated a sprint",
  api_key_created: "issued an API key",
  api_key_revoked: "revoked an API key",
};
function eventNode(event) {
  const summary = h(
    "div",
    { className: "event-summary" },
    h("strong", {}, agentName(event.actor)),
    ` ${actionNames[event.action] || event.action.replaceAll("_", " ")}`,
  );
  const meta = h("div", { className: "event-meta" });
  if (event.issue) meta.append(issueLink(event.issue));
  if (event.data?.state) meta.append(stateBadge(event.data.state));
  meta.append(h("span", {}, `Event #${event.id}`));
  const body =
    event.action === "comment_added" ? event.data?.body : event.data?.title;
  return h(
    "article",
    { className: "event" },
    avatar(event.actor),
    h(
      "div",
      { className: "event-content" },
      summary,
      meta,
      body ? h("div", { className: "event-payload" }, body) : null,
    ),
    timeNode(event.created_at),
  );
}
function mergedEvents() {
  const map = new Map(
    [...activityExtra, ...snapshot.activity.items].map((e) => [e.id, e]),
  );
  return [...map.values()].sort((a, b) => b.id - a.id);
}
function renderActivity() {
  const events = mergedEvents();
  $("#issue-count").textContent = events.length;
  if (!events.length) {
    $("#content").replaceChildren(
      empty(
        "No activity yet",
        "Claims, updates, comments and state changes will appear here as agents work.",
        "↗",
      ),
    );
    return;
  }
  const nodes = [
    h("div", { className: "activity-list" }, events.map(eventNode)),
  ];
  const before = activityLoaded
    ? activityCursor
    : snapshot.activity.next_before;
  if (before)
    nodes.push(
      h(
        "button",
        {
          className: "load-more",
          onClick: async (event) => {
            const button = event.currentTarget;
            button.disabled = true;
            const requested = selectedProject;
            try {
              const page = await api(
                `/api/activity?project=${encodeURIComponent(selectedProject)}&before=${before}`,
              );
              if (requested !== selectedProject || !token) return;
              activityExtra.push(...page.items);
              activityCursor = page.next_before;
              activityLoaded = true;
              renderActivity();
            } catch (error) {
              handleError(error);
              button.disabled = false;
            }
          },
        },
        "Load earlier activity",
      ),
    );
  $("#content").replaceChildren(...nodes);
}
function handleError(error) {
  if (error.unauthorized) authPrompt(error.message);
  else {
    $("#error").hidden = false;
    $("#error").textContent = error.message;
  }
}
function render() {
  if (!snapshot) return;
  renderMetrics();
  document.querySelectorAll("[data-view]").forEach((b) => {
    b.classList.toggle("selected", b.dataset.view === view);
    if (b.dataset.view === view) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  const names = {
    board: [
      "Issue board",
      "A clear view of what your agents are building.",
      "All issues",
      "Grouped by status",
    ],
    agents: [
      "Agents",
      "The team behind the work. Workloads shown for this project.",
      "Your agents",
      "Availability is based on project assignments",
    ],
    activity: [
      "Activity",
      "Every handoff, claim, and milestone in one place.",
      "Project activity",
      "Newest first · updates live",
    ],
  };
  const [title, description, section, note] = names[view];
  $("#page-title").textContent = title;
  document.title = `${title} · Agent Workboard`;
  $("#page-description").textContent = description;
  $("#section-title").replaceChildren(
    document.createTextNode(`${section} `),
    $("#issue-count"),
  );
  $("#section-note").textContent = note;
  $("#filters").hidden = view !== "board";
  $("#content").setAttribute("aria-busy", "false");
  if (!snapshot.projects.length) {
    $("#content").replaceChildren(
      empty(
        "Your workspace is ready",
        "Create a project through the CLI, MCP, or authenticated HTTP API. Agent work will appear here automatically.",
      ),
    );
    $("#issue-count").textContent = "0";
  } else if (view === "board") renderBoard();
  else if (view === "agents") renderAgents();
  else renderActivity();
  updateLeases();
}
function setView(next) {
  view = next;
  render();
  updateUrl();
}
function property(label, value) {
  return h(
    "div",
    { className: "property" },
    h("dt", {}, label),
    h("dd", {}, value),
  );
}
function detailSection(label, content) {
  return h(
    "section",
    { className: "detail-section" },
    h("h3", {}, label),
    content,
  );
}
function commentsContent() {
  const items = detail.comments.items;
  return h(
    "div",
    {},
    items.length
      ? items.map((c) =>
          h(
            "article",
            { className: "comment" },
            h(
              "div",
              { className: "comment-header" },
              agentName(c.actor),
              timeNode(c.created_at),
            ),
            h("div", { className: "prose" }, c.body),
          ),
        )
      : h("p", { className: "muted prose" }, "No comments yet."),
    detail.comments.next_before
      ? detailMore("comments", "Load earlier comments")
      : null,
  );
}
function detailMore(type, label) {
  return h(
    "button",
    {
      className: "load-more",
      onClick: async (event) => {
        const button = event.currentTarget;
        button.disabled = true;
        const requested = detailKey;
        const version = detailVersion;
        const page = detail[type === "events" ? "activity" : "comments"];
        try {
          const data = await api(
            `/api/issues/${encodeURIComponent(requested)}/${type}?before=${page.next_before}`,
          );
          if (detailVersion !== version || detailKey !== requested) return;
          page.items.push(...data.items);
          page.next_before = data.next_before;
          renderDetail();
        } catch (error) {
          handleError(error);
          button.disabled = false;
        }
      },
    },
    label,
  );
}
function renderDetail() {
  if (!detail) return;
  const i = detail.issue;
  $("#detail-key").textContent = `${i.project} / ${i.key}`;
  const nodes = [
    h(
      "div",
      { className: "detail-badges" },
      kindBadge(i.kind),
      stateBadge(i.state),
      priority(i.priority),
    ),
    h("h2", { id: "detail-title" }, i.title),
    h(
      "dl",
      { className: "detail-properties" },
      property("Assigned agent", agentName(i.assignee)),
      property("Parent", i.parent ? issueLink(i.parent) : "None"),
      property(
        "Sprint",
        snapshot?.sprints.find((s) => s.id === i.sprint)?.name ||
          i.sprint ||
          "None",
      ),
      property("Estimate", i.estimate ?? "Not estimated"),
      property("Revision", String(i.revision)),
      property("Updated", absoluteTime(i.updated_at)),
    ),
  ];
  if (i.lease) nodes.push(leaseNode(i.lease));
  if (i.labels.length)
    nodes.push(
      h(
        "div",
        { className: "card-labels" },
        i.labels.map((l) => h("span", { className: "tag" }, l)),
      ),
    );
  nodes.push(
    detailSection(
      "Description",
      h(
        "div",
        { className: "prose" },
        i.description || "No description provided.",
      ),
    ),
  );
  if (i.acceptance_criteria.length)
    nodes.push(
      detailSection(
        "Acceptance criteria",
        h(
          "ul",
          { className: "criteria" },
          i.acceptance_criteria.map((c) => h("li", {}, c)),
        ),
      ),
    );
  if (detail.blockers.length)
    nodes.push(
      detailSection(
        "Unresolved blockers",
        h(
          "div",
          {},
          detail.blockers.map((key) =>
            h(
              "div",
              { className: "relation" },
              h("span", {}, "Blocked by"),
              issueLink(key),
            ),
          ),
        ),
      ),
    );
  if (detail.children.length)
    nodes.push(
      detailSection(
        "Child issues",
        h(
          "div",
          {},
          detail.children.map((c) =>
            h(
              "div",
              { className: "relation" },
              stateBadge(c.state),
              issueLink(c.key, `${c.key} · ${c.title}`),
            ),
          ),
        ),
      ),
    );
  if (detail.links.length)
    nodes.push(
      detailSection(
        "Linked issues",
        h(
          "div",
          {},
          detail.links.map((link) => {
            const outgoing = link.source === i.key;
            const label =
              link.kind === "blocks"
                ? outgoing
                  ? "Blocks"
                  : "Blocked by"
                : link.kind === "relates_to"
                  ? "Related to"
                  : outgoing
                    ? "Duplicates"
                    : "Duplicated by";
            return h(
              "div",
              { className: "relation" },
              h("span", {}, label),
              issueLink(outgoing ? link.target : link.source),
            );
          }),
        ),
      ),
    );
  if (Object.keys(i.metadata).length)
    nodes.push(
      detailSection(
        "Agent context",
        h(
          "pre",
          { className: "metadata" },
          JSON.stringify(i.metadata, null, 2),
        ),
      ),
    );
  nodes.push(detailSection("Comments & evidence", commentsContent()));
  nodes.push(
    detailSection(
      "Issue history",
      h(
        "div",
        {},
        detail.activity.items.map(eventNode),
        detail.activity.next_before
          ? detailMore("events", "Load earlier history")
          : null,
      ),
    ),
  );
  const scroll = $("#issue-dialog").scrollTop;
  $("#detail-content").replaceChildren(...nodes);
  $("#issue-dialog").scrollTop = scroll;
  updateLeases();
}
async function refreshDetail() {
  const requested = detailKey;
  const version = detailVersion;
  if (!requested) return;
  try {
    const data = await api(`/api/issues/${encodeURIComponent(requested)}`);
    if (version !== detailVersion || requested !== detailKey) return;
    const next = JSON.stringify(data);
    if (detailSignature === next) return;
    detailSignature = next;
    if (detail) {
      for (const key of ["activity", "comments"]) {
        if (
          detail[key].items.length > 100 ||
          detail[key].next_before !== data[key].next_before
        ) {
          const merged = new Map(
            [...detail[key].items, ...data[key].items].map((item) => [
              item.id,
              item,
            ]),
          );
          data[key].items = [...merged.values()].sort((a, b) => b.id - a.id);
          data[key].next_before = detail[key].next_before;
        }
      }
    }
    detail = data;
    renderDetail();
  } catch (error) {
    if (requested !== detailKey || version !== detailVersion) return;
    if (error.unauthorized) authPrompt(error.message);
    else if (!detail)
      $("#detail-content").replaceChildren(
        h("h2", { id: "detail-title" }, "Unable to load this issue"),
        h("p", { className: "prose" }, error.message),
      );
    else handleError(error);
  }
}
function openIssue(key) {
  detailKey = key;
  detail = null;
  detailSignature = "";
  detailVersion++;
  $("#detail-key").textContent = key;
  $("#detail-content").replaceChildren(
    h("h2", { id: "detail-title" }, "Loading issue…"),
  );
  $("#issue-dialog").scrollTop = 0;
  if (!$("#issue-dialog").open) $("#issue-dialog").showModal();
  updateUrl();
  refreshDetail();
}
function clearFilters() {
  ["search", "assignee", "kind", "sprint"].forEach((id) => {
    $(`#${id}`).value = "";
  });
}
$("#auth-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const key = $("#api-key").value.trim();
  $("#connect").disabled = true;
  try {
    await api("/api/session", key);
    token = key;
    sessionStorage.setItem("workboard-key", key);
    $("#api-key").value = "";
    $("#auth-error").hidden = true;
    $("#auth-dialog").close();
    signature = "";
    await refresh();
  } catch (error) {
    $("#auth-error").textContent = error.message;
    $("#auth-error").hidden = false;
  } finally {
    $("#connect").disabled = false;
  }
});
$("#auth-dialog").addEventListener("cancel", (event) => event.preventDefault());
$("#sign-out").addEventListener("click", () => authPrompt());
$("#refresh").addEventListener("click", refresh);
$("#project").addEventListener("change", () => {
  selectedProject = $("#project").value;
  signature = "";
  clearFilters();
  activityExtra = [];
  activityCursor = null;
  activityLoaded = false;
  if ($("#issue-dialog").open) $("#issue-dialog").close();
  $("#content").replaceChildren(
    empty("Switching projects", "Loading the latest work…"),
  );
  updateUrl();
  // If the previous refresh is still pending it will discard its response. The
  // next refresh starts as soon as that request settles, without overlapping.
  const wait = () => (refreshing ? setTimeout(wait, 50) : refresh());
  wait();
});
["search", "assignee", "kind", "sprint"].forEach((id) =>
  $(`#${id}`).addEventListener(id === "search" ? "input" : "change", render),
);
$("#clear-filters").addEventListener("click", () => {
  clearFilters();
  render();
});
document
  .querySelectorAll("[data-view]")
  .forEach((button) =>
    button.addEventListener("click", () => setView(button.dataset.view)),
  );
$("#close-detail").addEventListener("click", () => $("#issue-dialog").close());
$("#issue-dialog").addEventListener("click", (event) => {
  if (
    event.target === $("#issue-dialog") &&
    event.clientX < $("#issue-dialog").getBoundingClientRect().left
  )
    $("#issue-dialog").close();
});
$("#issue-dialog").addEventListener("close", () => {
  detailKey = null;
  detail = null;
  detailVersion++;
  updateUrl();
});
setInterval(() => {
  const every = streamLive ? 15000 : 3000;
  if (!document.hidden && Date.now() - lastRefresh >= every) refresh();
}, 1000);
setInterval(updateLeases, 1000);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh();
});
if (token) refresh();
else authPrompt();
