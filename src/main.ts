import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

// absent outside the Tauri shell (browser preview), where controls are hidden
function tauriWindow() {
  try {
    return getCurrentWindow();
  } catch {
    return null;
  }
}

type PoolKind = "code" | "agent";

interface PoolCombo {
  rootLabel: string;
  pool: PoolKind;
  accountId: string;
  orgId: string;
  path: string;
  isJunction: boolean;
  junctionTarget: string | null;
  sessionCount: number;
  tombstoneCount: number;
}

interface CliSession {
  sessionId: string;
  jsonlPath: string;
  projectDir: string;
  cwd: string | null;
  entrypoint: string | null;
  firstUserText: string | null;
  firstTimestamp: string | null;
  lastActivityMs: number;
  sizeBytes: number;
  registered: boolean;
  tombstoned: boolean;
}

interface DiscoveryReport {
  roots: { label: string; path: string; antDid: string | null }[];
  combos: PoolCombo[];
  cliSessions: CliSession[];
}

interface UnifyPlan {
  pool: PoolKind;
  canonical: PoolCombo;
  toMerge: PoolCombo[];
  alreadyUnified: PoolCombo[];
  foreignJunctions: PoolCombo[];
}

interface DesktopSession {
  fileName: string;
  sessionId: string;
  cliSessionId: string | null;
  cwd: string | null;
  title: string | null;
  model: string | null;
  isArchived: boolean;
  createdAt: number | null;
  lastActivityAt: number | null;
  completedTurns: number | null;
  sandboxed: boolean;
}

interface RunningDesktop {
  rootLabel: string;
  running: boolean;
}

interface RegisterReport {
  sessionId: string;
  outcome: { status: string; metadataFile?: string; reason?: string } | null;
  error: string | null;
}

interface UnregisterReport {
  metadataFile: string;
  outcome: { tombstone: string | null; removed: string } | null;
  error: string | null;
}

interface WatchStatus {
  running: boolean;
  paused: boolean;
  pendingUnify: boolean;
  lastEvent: string | null;
}

interface TombstoneInfo {
  fileName: string;
  cliSessionId: string;
  deletedAt: number | null;
  jsonlPath: string | null;
  jsonlSize: number;
}

interface PurgeOutcome {
  fileName: string;
  markerRemoved: boolean;
  transcriptRemoved: boolean;
  error: string | null;
}

interface AppState {
  tab: "unify" | "migrate";
  report: DiscoveryReport | null;
  plans: UnifyPlan[];
  desktopSessions: DesktopSession[];
  running: RunningDesktop[];
  watch: WatchStatus | null;
  selectedCli: Set<string>;
  selectedDesktop: Set<string>;
  busy: boolean;
  maximized: boolean;
  tombstones: TombstoneInfo[];
  purgeConfirmOpen: boolean;
}

const state: AppState = {
  tab: "unify",
  report: null,
  plans: [],
  desktopSessions: [],
  running: [],
  watch: null,
  selectedCli: new Set(),
  selectedDesktop: new Set(),
  busy: false,
  maximized: false,
  tombstones: [],
  purgeConfirmOpen: false,
};

const app = document.querySelector<HTMLDivElement>("#app")!;

const fmtTime = new Intl.DateTimeFormat("zh-CN", {
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
});

function timeOf(ms: number | null): string {
  return ms ? fmtTime.format(new Date(ms)) : "—";
}

function sizeOf(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${bytes} B`;
}

function shortId(id: string): string {
  return id.slice(0, 8);
}

function esc(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function toast(message: string, isError = false) {
  const host = document.querySelector("#toast-host")!;
  const node = document.createElement("div");
  node.className = isError ? "toast toast-error" : "toast";
  node.textContent = message;
  host.appendChild(node);
  setTimeout(() => node.remove(), 6000);
}

async function refresh() {
  try {
    const [report, plans, running, watch] = await Promise.all([
      invoke<DiscoveryReport>("scan"),
      invoke<UnifyPlan[]>("plan_unify_all"),
      invoke<RunningDesktop[]>("desktop_running"),
      invoke<WatchStatus>("watch_status"),
    ]);
    state.report = report;
    state.plans = plans;
    state.running = running;
    state.watch = watch;
  } catch (error) {
    app.innerHTML = `
      <div class="loading">
        <p>扫描失败：${esc(String(error))}</p>
        <p style="margin-top: var(--sp-16)"><button class="btn btn-secondary" data-action="refresh">重试</button></p>
      </div>`;
    return;
  }
  try {
    state.desktopSessions = await invoke<DesktopSession[]>("list_desktop_sessions");
  } catch {
    state.desktopSessions = [];
  }
  try {
    state.tombstones = await invoke<TombstoneInfo[]>("list_tombstones");
  } catch {
    state.tombstones = [];
  }
  render();
}

function anyDesktopRunning(): boolean {
  return state.running.some((r) => r.running);
}

/* ---------- 归一页 ---------- */

function comboStatus(combo: PoolCombo, plan: UnifyPlan | undefined): { label: string; cls: string } {
  if (plan && combo.path === plan.canonical.path) return { label: "canonical", cls: "pill-canonical" };
  if (combo.isJunction) {
    const unified = plan?.alreadyUnified.some((c) => c.path === combo.path) ?? false;
    return unified
      ? { label: "已归一", cls: "pill-unified" }
      : { label: "外部 junction", cls: "pill-foreign" };
  }
  return { label: "待归一", cls: "pill-pending" };
}

function renderUnify(): string {
  const report = state.report!;
  const pending = state.plans.reduce((n, p) => n + p.toMerge.length, 0);
  const runningNote = anyDesktopRunning()
    ? `<div class="banner banner-error">Claude Desktop 正在运行 —— 写操作已禁用，请先从托盘退出 Desktop（${state.running
        .filter((r) => r.running)
        .map((r) => esc(r.rootLabel))
        .join("、")}）</div>`
    : "";

  const poolCards = state.plans
    .map((plan) => {
      const combos = report.combos.filter((c) => c.pool === plan.pool);
      const rows = combos
        .map((combo) => {
          const status = comboStatus(combo, plan);
          const target = combo.junctionTarget
            ? `<span>→ <span class="mono">${esc(combo.junctionTarget)}</span></span>`
            : "";
          return `
            <div class="card">
              <div class="card-title">
                <span>${esc(combo.rootLabel)}</span>
                <span class="pill ${status.cls}">${status.label}</span>
              </div>
              <div class="card-meta">
                <span>账号 <span class="mono">${shortId(combo.accountId)}</span></span>
                <span>组织 <span class="mono">${shortId(combo.orgId)}</span></span>
                <span>会话 ${combo.sessionCount}</span>
                <span>墓碑 ${combo.tombstoneCount}</span>
                ${target}
              </div>
            </div>`;
        })
        .join("");
      const poolName = plan.pool === "code" ? "Code 会话池" : "Cowork 沙箱池";
      const poolDir = plan.pool === "code" ? "claude-code-sessions" : "local-agent-mode-sessions";
      return `
        <section class="column-section">
          <div class="eyebrow">${poolName} · ${poolDir}</div>
          ${rows || '<div class="empty-note">未发现组合</div>'}
        </section>`;
    })
    .join("");

  return `
    <div class="view">
      <div class="page-head">
        <div>
          <div class="eyebrow">存储归一</div>
          <h1>渠道与账号共享同一会话池</h1>
          <p class="lead">把每个账号/组织组合合并进主池，原目录备份后以 junction 指回 —— 切渠道、换账号、官方版与第三方版看到同一份列表。</p>
        </div>
        <div class="head-actions">
          <button class="btn btn-secondary" data-action="refresh">重新扫描</button>
          <button class="btn btn-primary" data-action="unify" ${
            pending === 0 || anyDesktopRunning() || state.busy ? "disabled" : ""
          }>一键归一（${pending}）</button>
        </div>
      </div>
      ${runningNote}
      ${
        pending === 0
          ? '<div class="banner">所有组合均已归一，新渠道/新账号首次出现时托盘守护会自动处理。</div>'
          : ""
      }
      ${poolCards}
    </div>`;
}

/* ---------- 迁移页 ---------- */

function projectOf(session: CliSession): string {
  return session.cwd ?? session.projectDir;
}

function renderCliColumn(): string {
  const sessions = state.report!.cliSessions;
  const groups = new Map<string, CliSession[]>();
  for (const s of sessions) {
    const key = projectOf(s);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key)!.push(s);
  }
  const blocks: string[] = [];
  for (const [project, list] of groups) {
    const ids = list.filter((s) => !s.registered && !s.tombstoned).map((s) => s.sessionId);
    const allChecked = ids.length > 0 && ids.every((id) => state.selectedCli.has(id));
    blocks.push(`
      <div class="group-label">
        <label><input type="checkbox" data-group-cli="${esc(project)}" ${allChecked ? "checked" : ""} ${
          ids.length === 0 ? "disabled" : ""
        }/> ${esc(project)}</label>
        <span>· ${list.length}</span>
      </div>`);
    for (const s of list) {
      const checked = state.selectedCli.has(s.sessionId);
      const pill = s.registered
        ? '<span class="pill pill-registered">✓ 已注册</span>'
        : s.tombstoned
          ? '<span class="pill pill-tombstone">墓碑</span>'
          : "";
      blocks.push(`
        <div class="session-row ${checked ? "selected" : ""}" data-cli="${s.sessionId}" ${
          s.registered ? 'data-disabled="1"' : ""
        }>
          <input type="checkbox" ${checked ? "checked" : ""} ${s.registered ? "disabled" : ""} tabindex="-1"/>
          <div class="session-body">
            <div class="session-title">${esc(s.firstUserText ?? s.sessionId)}</div>
            <div class="session-sub">
              <span class="mono">${shortId(s.sessionId)}</span>
              <span>${timeOf(s.lastActivityMs)}</span>
              <span>${sizeOf(s.sizeBytes)}</span>
              ${s.entrypoint ? `<span>${esc(s.entrypoint)}</span>` : ""}
            </div>
          </div>
          ${pill}
        </div>`);
    }
  }
  return blocks.join("") || '<div class="empty-note">未发现 CLI 会话</div>';
}

function renderDesktopColumn(): string {
  const real = state.desktopSessions.filter((s) => !s.sandboxed);
  const sandboxed = state.desktopSessions.filter((s) => s.sandboxed);
  const cliPool = new Set((state.report?.cliSessions ?? []).map((s) => s.sessionId));
  const rows = real
    .map((s) => {
      const checked = state.selectedDesktop.has(s.fileName);
      const resumable = s.cliSessionId !== null && cliPool.has(s.cliSessionId);
      return `
        <div class="session-row ${checked ? "selected" : ""}" data-desktop="${esc(s.fileName)}">
          <input type="checkbox" ${checked ? "checked" : ""} tabindex="-1"/>
          <div class="session-body">
            <div class="session-title">${esc(s.title ?? s.sessionId)}</div>
            <div class="session-sub">
              <span class="mono">${s.cliSessionId ? shortId(s.cliSessionId) : "—"}</span>
              <span>${timeOf(s.lastActivityAt)}</span>
              ${s.cwd ? `<span>${esc(s.cwd)}</span>` : ""}
              ${s.isArchived ? "<span>已归档</span>" : ""}
            </div>
          </div>
          ${resumable ? '<span class="pill pill-registered" title="转录在 ~/.claude/projects，对应目录里 claude -r 即可继续">CLI 可续</span>' : ""}
        </div>`;
    })
    .join("");
  const sandboxBlock = sandboxed.length
    ? `
      <div class="column-section dimmed">
        <div class="eyebrow">沙箱会话（无项目文件夹）不支持迁移 · ${sandboxed.length}</div>
        ${sandboxed
          .map(
            (s) => `
          <div class="session-row">
            <div class="session-body">
              <div class="session-title">${esc(s.title ?? s.sessionId)}</div>
              <div class="session-sub"><span>${timeOf(s.lastActivityAt)}</span></div>
            </div>
          </div>`,
          )
          .join("")}
      </div>`
    : "";
  return (rows || '<div class="empty-note">主池暂无 Desktop code 会话</div>') + sandboxBlock;
}

function purgeBanner(): string {
  if (!state.purgeConfirmOpen) return "";
  const total = state.tombstones.length;
  const withTranscript = state.tombstones.filter((t) => t.jsonlPath !== null);
  const bytes = withTranscript.reduce((n, t) => n + t.jsonlSize, 0);
  const disabled = anyDesktopRunning() || state.busy;
  return `
    <div class="banner">
      <span>共 ${total} 个墓碑标记，其中 ${withTranscript.length} 个在 CLI 侧仍有转录（${sizeOf(bytes)}）。删除会送入系统回收站，可从回收站找回。</span>
      <button class="btn btn-secondary btn-small" data-action="purge-markers" ${disabled ? "disabled" : ""}>仅清标记（解封 ${total} 个）</button>
      <button class="btn btn-secondary btn-small btn-danger" data-action="purge-all" ${disabled ? "disabled" : ""}>标记 + 转录一起删</button>
      <button class="btn btn-tertiary btn-small" data-action="purge-cancel">取消</button>
    </div>`;
}

function renderMigrate(): string {
  const runningNote = anyDesktopRunning()
    ? '<div class="banner banner-error">Claude Desktop 正在运行 —— 迁移已禁用，请先退出 Desktop</div>'
    : "";
  const cliCount = state.selectedCli.size;
  const deskCount = state.selectedDesktop.size;
  const disabled = anyDesktopRunning() || state.busy;
  return `
    <div class="view view-wide">
      <div class="page-head">
        <div>
          <div class="eyebrow">会话迁移</div>
          <h1>CLI 与 Desktop 互认会话</h1>
          <p class="lead">注册 = 为 CLI 会话生成 Desktop 元数据（转录零拷贝，两边同源）；注销 = 移除 Desktop 元数据（转录保留，CLI 照常 resume）。</p>
        </div>
        <div class="head-actions">
          ${
            state.tombstones.length > 0
              ? `<button class="btn btn-secondary" data-action="purge-open" ${state.busy ? "disabled" : ""}>清理墓碑（${state.tombstones.length}）</button>`
              : ""
          }
          <button class="btn btn-secondary" data-action="refresh">重新扫描</button>
        </div>
      </div>
      ${runningNote}
      ${purgeBanner()}
      <div class="migrate-grid">
        <div class="column">
          <div class="column-head">
            <span class="eyebrow">CLI 会话 · ~/.claude/projects</span>
            <button class="btn btn-tertiary btn-small" data-action="cli-select-all">全选可注册</button>
            <button class="btn btn-tertiary btn-small" data-action="cli-clear">清空选中</button>
          </div>
          <div class="column-scroll" id="cli-column">${renderCliColumn()}</div>
        </div>
        <div class="migrate-actions">
          <button class="btn btn-primary" data-action="register" ${cliCount === 0 || disabled ? "disabled" : ""}>
            注册 → （${cliCount}）
          </button>
          <button class="btn btn-secondary" data-action="unregister" ${
            deskCount === 0 || disabled ? "disabled" : ""
          }>
            ← 注销（${deskCount}）
          </button>
        </div>
        <div class="column">
          <div class="column-head">
            <span class="eyebrow">Desktop Code 会话 · 主池</span>
            <button class="btn btn-tertiary btn-small" data-action="desk-select-all">全选</button>
            <button class="btn btn-tertiary btn-small" data-action="desk-clear">清空选中</button>
          </div>
          <div class="column-scroll" id="desktop-column">${renderDesktopColumn()}</div>
        </div>
      </div>
    </div>`;
}

/* ---------- 渲染与事件 ---------- */

function watchChip(): string {
  const w = state.watch;
  if (!w) return "";
  const stateName = w.paused ? "paused" : w.pendingUnify ? "pending" : "running";
  const label = w.paused ? "守护已暂停" : w.pendingUnify ? "等待 Desktop 退出" : "守护运行中";
  return `<div class="watch-chip" data-state="${stateName}" title="${esc(w.lastEvent ?? "")}">
    <span class="watch-dot"></span>${label}
  </div>`;
}

const FLOWER_MARK = `
  <svg class="brand-mark" viewBox="0 0 24 24" aria-hidden="true">
    <g fill="var(--theme-accent-clay-primary)">
      <ellipse cx="12" cy="6.9" rx="2.2" ry="3.6"/>
      <ellipse cx="12" cy="6.9" rx="2.2" ry="3.6" transform="rotate(60 12 12)"/>
      <ellipse cx="12" cy="6.9" rx="2.2" ry="3.6" transform="rotate(120 12 12)"/>
      <ellipse cx="12" cy="6.9" rx="2.2" ry="3.6" transform="rotate(180 12 12)"/>
      <ellipse cx="12" cy="6.9" rx="2.2" ry="3.6" transform="rotate(240 12 12)"/>
      <ellipse cx="12" cy="6.9" rx="2.2" ry="3.6" transform="rotate(300 12 12)"/>
    </g>
    <circle cx="12" cy="12" r="2.8" fill="var(--theme-accent-clay-interactive)"/>
    <circle cx="12" cy="12" r="1.1" fill="var(--theme-background-primary)"/>
  </svg>`;

const SVG_MIN = '<svg width="10" height="10" viewBox="0 0 10 10"><path d="M0 5h10" stroke="currentColor" stroke-width="1"/></svg>';
const SVG_MAX = '<svg width="10" height="10" viewBox="0 0 10 10"><rect x="0.5" y="0.5" width="9" height="9" rx="1.5" fill="none" stroke="currentColor" stroke-width="1"/></svg>';
const SVG_RESTORE = '<svg width="10" height="10" viewBox="0 0 10 10"><rect x="0.5" y="2.5" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1"/><path d="M2.5 2.5v-1a1 1 0 0 1 1-1h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-1" fill="none" stroke="currentColor" stroke-width="1"/></svg>';
const SVG_CLOSE = '<svg width="10" height="10" viewBox="0 0 10 10"><path d="M0.5 0.5l9 9m0-9l-9 9" stroke="currentColor" stroke-width="1"/></svg>';

function windowControls(): string {
  if (!tauriWindow()) return "";
  return `
    <div class="win-controls">
      <button class="win-btn" data-action="win-min" title="最小化" tabindex="-1">${SVG_MIN}</button>
      <button class="win-btn" data-action="win-max" title="${state.maximized ? "还原" : "最大化"}" tabindex="-1">${
        state.maximized ? SVG_RESTORE : SVG_MAX
      }</button>
      <button class="win-btn win-close" data-action="win-close" title="关闭（保留托盘守护）" tabindex="-1">${SVG_CLOSE}</button>
    </div>`;
}

/* Selection only touches row state; rebuilding innerHTML would reset the
   column scroll position, so sync the affected nodes in place. */
function updateMigrateSelection() {
  for (const row of app.querySelectorAll<HTMLElement>("[data-cli]")) {
    const selected = state.selectedCli.has(row.dataset.cli!);
    row.classList.toggle("selected", selected);
    const box = row.querySelector<HTMLInputElement>('input[type="checkbox"]');
    if (box) box.checked = selected;
  }
  for (const row of app.querySelectorAll<HTMLElement>("[data-desktop]")) {
    const selected = state.selectedDesktop.has(row.dataset.desktop!);
    row.classList.toggle("selected", selected);
    const box = row.querySelector<HTMLInputElement>('input[type="checkbox"]');
    if (box) box.checked = selected;
  }
  for (const box of app.querySelectorAll<HTMLInputElement>("[data-group-cli]")) {
    const members = (state.report?.cliSessions ?? []).filter(
      (s) => projectOf(s) === box.dataset.groupCli && !s.registered && !s.tombstoned,
    );
    box.checked = members.length > 0 && members.every((s) => state.selectedCli.has(s.sessionId));
  }
  const disabled = anyDesktopRunning() || state.busy;
  const register = app.querySelector<HTMLButtonElement>('[data-action="register"]');
  if (register) {
    register.textContent = `注册 → （${state.selectedCli.size}）`;
    register.disabled = state.selectedCli.size === 0 || disabled;
  }
  const unregister = app.querySelector<HTMLButtonElement>('[data-action="unregister"]');
  if (unregister) {
    unregister.textContent = `← 注销（${state.selectedDesktop.size}）`;
    unregister.disabled = state.selectedDesktop.size === 0 || disabled;
  }
}

function render() {
  if (!state.report) {
    app.innerHTML = '<div class="loading">正在扫描会话存储…</div>';
    return;
  }
  const scrollTops = new Map<string, number>();
  for (const column of app.querySelectorAll<HTMLElement>(".column-scroll")) {
    scrollTops.set(column.id, column.scrollTop);
  }
  app.innerHTML = `
    <header class="topbar" data-tauri-drag-region>
      <div class="brand">${FLOWER_MARK}Claude++<span class="brand-sub">会话守护</span></div>
      <nav class="tabs">
        <button class="tab ${state.tab === "unify" ? "active" : ""}" data-tab="unify">存储归一</button>
        <button class="tab ${state.tab === "migrate" ? "active" : ""}" data-tab="migrate">会话迁移</button>
      </nav>
      ${watchChip()}
      ${windowControls()}
    </header>
    <main>${state.tab === "unify" ? renderUnify() : renderMigrate()}</main>
  `;
  for (const [id, top] of scrollTops) {
    const column = document.getElementById(id);
    if (column) column.scrollTop = top;
  }
}

async function doUnify() {
  state.busy = true;
  render();
  try {
    const reports = await invoke<{ pool: PoolKind; movedEntries: number; merged: string[]; conflicts: unknown[] }[]>(
      "apply_unify_all",
    );
    const merged = reports.reduce((n, r) => n + r.merged.length, 0);
    const conflicts = reports.reduce((n, r) => n + r.conflicts.length, 0);
    toast(`归一完成：${merged} 个组合并入主池${conflicts ? `，${conflicts} 处重名冲突（原文件保留在 .bak）` : ""}`);
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.busy = false;
    await refresh();
  }
}

async function doRegister() {
  state.busy = true;
  render();
  try {
    const ids = [...state.selectedCli];
    const reports = await invoke<RegisterReport[]>("register_sessions", {
      sessionIds: ids,
      policy: "skip",
    });
    let ok = 0;
    for (const r of reports) {
      if (r.error) {
        toast(`${shortId(r.sessionId)}: ${r.error}`, true);
      } else if (r.outcome?.status === "registered") {
        ok += 1;
      } else if (r.outcome?.status === "tombstoneBlocked") {
        toast(`${shortId(r.sessionId)}: 曾在 Desktop 删除过（墓碑），再次注册将复活它`, true);
      } else if (r.outcome?.status === "alreadyRegistered") {
        toast(`${shortId(r.sessionId)}: 已注册，跳过`);
      }
    }
    if (ok) toast(`已注册 ${ok} 个会话，打开 Desktop 对应项目即可见`);
    state.selectedCli.clear();
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.busy = false;
    await refresh();
  }
}

async function doPurge(deleteTranscripts: boolean) {
  state.busy = true;
  render();
  try {
    const outcomes = await invoke<PurgeOutcome[]>("purge_tombstones", {
      fileNames: state.tombstones.map((t) => t.fileName),
      deleteTranscripts,
    });
    const markers = outcomes.filter((o) => o.markerRemoved).length;
    const transcripts = outcomes.filter((o) => o.transcriptRemoved).length;
    for (const o of outcomes) {
      if (o.error) toast(`${o.fileName}: ${o.error}`, true);
    }
    toast(
      deleteTranscripts
        ? `已清理 ${markers} 个墓碑，${transcripts} 份转录已送回收站`
        : `已清理 ${markers} 个墓碑标记，对应会话恢复为可注册`,
    );
    state.purgeConfirmOpen = false;
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.busy = false;
    await refresh();
  }
}

async function doUnregister() {
  state.busy = true;
  render();
  try {
    const files = [...state.selectedDesktop];
    const reports = await invoke<UnregisterReport[]>("unregister_sessions", {
      metadataFiles: files,
      hardDelete: false,
    });
    let ok = 0;
    for (const r of reports) {
      if (r.error) toast(`${r.metadataFile}: ${r.error}`, true);
      else ok += 1;
    }
    if (ok) toast(`已注销 ${ok} 个会话（转录保留，CLI 侧 claude -r 可继续）`);
    state.selectedDesktop.clear();
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.busy = false;
    await refresh();
  }
}

app.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  const tab = target.closest<HTMLElement>("[data-tab]");
  if (tab) {
    state.tab = tab.dataset.tab as AppState["tab"];
    render();
    return;
  }
  const action = target.closest<HTMLElement>("[data-action]")?.dataset.action;
  if (action) {
    if (action === "win-min") void tauriWindow()?.minimize();
    if (action === "win-max") void tauriWindow()?.toggleMaximize();
    if (action === "win-close") void tauriWindow()?.close();
    if (action === "refresh") void refresh();
    if (action === "unify") void doUnify();
    if (action === "register") void doRegister();
    if (action === "unregister") void doUnregister();
    if (action === "purge-open") {
      state.purgeConfirmOpen = true;
      render();
    }
    if (action === "purge-cancel") {
      state.purgeConfirmOpen = false;
      render();
    }
    if (action === "purge-markers") void doPurge(false);
    if (action === "purge-all") void doPurge(true);
    if (action === "cli-select-all") {
      for (const s of state.report?.cliSessions ?? []) {
        // tombstoned sessions get blocked at register time; sweeping them in
        // would only produce a wall of error toasts
        if (!s.registered && !s.tombstoned) state.selectedCli.add(s.sessionId);
      }
      updateMigrateSelection();
    }
    if (action === "cli-clear") {
      state.selectedCli.clear();
      updateMigrateSelection();
    }
    if (action === "desk-select-all") {
      for (const s of state.desktopSessions) {
        if (!s.sandboxed) state.selectedDesktop.add(s.fileName);
      }
      updateMigrateSelection();
    }
    if (action === "desk-clear") {
      state.selectedDesktop.clear();
      updateMigrateSelection();
    }
    return;
  }
  const groupCli = target.closest<HTMLInputElement>("[data-group-cli]");
  if (groupCli) {
    const project = groupCli.dataset.groupCli!;
    const members = (state.report?.cliSessions ?? []).filter(
      (s) => projectOf(s) === project && !s.registered && !s.tombstoned,
    );
    const allIn = members.every((s) => state.selectedCli.has(s.sessionId));
    for (const s of members) {
      if (allIn) state.selectedCli.delete(s.sessionId);
      else state.selectedCli.add(s.sessionId);
    }
    updateMigrateSelection();
    return;
  }
  const cliRow = target.closest<HTMLElement>("[data-cli]");
  if (cliRow && !cliRow.dataset.disabled) {
    const id = cliRow.dataset.cli!;
    if (state.selectedCli.has(id)) state.selectedCli.delete(id);
    else state.selectedCli.add(id);
    updateMigrateSelection();
    return;
  }
  const deskRow = target.closest<HTMLElement>("[data-desktop]");
  if (deskRow) {
    const file = deskRow.dataset.desktop!;
    if (state.selectedDesktop.has(file)) state.selectedDesktop.delete(file);
    else state.selectedDesktop.add(file);
    updateMigrateSelection();
  }
});

void listen<string>("unify-auto", (event) => {
  toast(`自动归一：${event.payload}`);
  void refresh();
});
void listen<string>("unify-blocked", (event) => toast(`归一挂起：${event.payload}`, true));
void listen<string>("unify-noop", (event) => toast(event.payload));
void listen<string>("unify-error", (event) => toast(`归一失败：${event.payload}`, true));

/* Maximize toggles can come from the drag region double-click or Win+arrow,
   so track real window state instead of assuming the button was the trigger. */
function syncMaximized() {
  const win = tauriWindow();
  if (!win) return;
  void win.isMaximized().then((maximized) => {
    if (maximized === state.maximized) return;
    state.maximized = maximized;
    const button = app.querySelector<HTMLButtonElement>('[data-action="win-max"]');
    if (button) {
      button.innerHTML = maximized ? SVG_RESTORE : SVG_MAX;
      button.title = maximized ? "还原" : "最大化";
    }
  });
}
void tauriWindow()?.onResized(() => syncMaximized());
syncMaximized();

void refresh();
setInterval(() => {
  void invoke<WatchStatus>("watch_status").then((w) => {
    const changed = JSON.stringify(w) !== JSON.stringify(state.watch);
    state.watch = w;
    if (changed) render();
  });
}, 15000);
