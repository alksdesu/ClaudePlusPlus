import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import DOMPurify from "dompurify";
import { marked } from "marked";
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
  groupId: string;
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

interface DeleteReport {
  target: string;
  outcome: {
    cliSessionId: string | null;
    metadataRemoved: string | null;
    tombstoneRemoved: boolean;
    transcriptRemoved: boolean;
  } | null;
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

type CodexStatus = "migrated" | "mirror" | "orphanMirror" | "nativeOnly";

interface CodexSession {
  threadId: string;
  rolloutPath: string;
  cwd: string | null;
  originator: string | null;
  title: string | null;
  sizeBytes: number;
  lastActivityMs: number;
  status: CodexStatus;
  claudePath: string | null;
}

interface CodexMigrateReport {
  threadId: string;
  outcome: { status: string; claudePath?: string; turns?: number; reason?: string } | null;
  error: string | null;
}

type CodexFilter = "all" | "nativeOnly" | "migrated" | "mirror";

interface PreviewMessage {
  role: string;
  text: string;
  timestamp: string | null;
}

interface SessionPreview {
  title: string | null;
  cwd: string | null;
  totalMessages: number;
  truncated: boolean;
  messages: PreviewMessage[];
}

interface PurgeOutcome {
  fileName: string;
  markerRemoved: boolean;
  transcriptRemoved: boolean;
  error: string | null;
}

interface AppState {
  tab: "unify" | "migrate" | "codex";
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
  deleteConfirm: "cli" | "desktop" | null;
  expandedGroups: Set<string>;
  codexSessions: CodexSession[];
  codexRunning: boolean;
  selectedCodex: Set<string>;
  codexFilter: CodexFilter;
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
  deleteConfirm: null,
  expandedGroups: new Set(),
  codexSessions: [],
  codexRunning: false,
  selectedCodex: new Set(),
  codexFilter: "all",
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
  try {
    [state.codexSessions, state.codexRunning] = await Promise.all([
      invoke<CodexSession[]>("list_codex_sessions"),
      invoke<boolean>("codex_running"),
    ]);
  } catch {
    state.codexSessions = [];
    state.codexRunning = false;
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
      : { label: "外部链接", cls: "pill-foreign" };
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
          <p class="lead">把每个账号/组织组合合并进主池，原目录备份后以链接指回 —— 切渠道、换账号、官方版与第三方版看到同一份列表。</p>
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

/* rewind/resume 分支与 compact 续接共享 groupId（后端按转录血缘算出）。
   全局列表已按活跃时间倒序，组内首个成员即最新活跃分支 = 组代表。 */
interface CliGroup {
  groupId: string;
  rep: CliSession;
  branches: CliSession[];
  anyRegistered: boolean;
}

function cliGroupsByProject(): Map<string, CliGroup[]> {
  const byProject = new Map<string, Map<string, CliGroup>>();
  for (const s of state.report?.cliSessions ?? []) {
    const project = projectOf(s);
    if (!byProject.has(project)) byProject.set(project, new Map());
    const groups = byProject.get(project)!;
    const key = s.groupId || s.sessionId;
    const group = groups.get(key);
    if (!group) {
      groups.set(key, { groupId: key, rep: s, branches: [], anyRegistered: s.registered });
    } else {
      group.branches.push(s);
      group.anyRegistered ||= s.registered;
    }
  }
  const out = new Map<string, CliGroup[]>();
  for (const [project, groups] of byProject) out.set(project, [...groups.values()]);
  return out;
}

/* 组内任一分支已注册即视为该逻辑会话已入 Desktop，批量操作跳过整组；
   展开后单独勾选分支不受此限。 */
function repRegistrable(group: CliGroup): boolean {
  return !group.anyRegistered && !group.rep.tombstoned;
}

function findCliGroup(sessionId: string): CliGroup | undefined {
  for (const groups of cliGroupsByProject().values()) {
    const hit = groups.find(
      (g) => g.rep.sessionId === sessionId || g.branches.some((b) => b.sessionId === sessionId),
    );
    if (hit) return hit;
  }
  return undefined;
}

/* 一个逻辑会话只该进 Desktop 一次：勾选组内任一行前先清掉同组其他行。 */
function selectExclusiveInGroup(sessionId: string) {
  const group = findCliGroup(sessionId);
  if (!group) return;
  state.selectedCli.delete(group.rep.sessionId);
  for (const branch of group.branches) state.selectedCli.delete(branch.sessionId);
  state.selectedCli.add(sessionId);
}

/* 勾选里排掉已注册的：删除需要能选中它们，注册则不该重复提交 */
function registerableSelection(): string[] {
  const byId = new Map((state.report?.cliSessions ?? []).map((s) => [s.sessionId, s]));
  return [...state.selectedCli].filter((id) => !byId.get(id)?.registered);
}

/* 勾中组代表 = 删掉这个逻辑会话的所有分支文件；单勾某个分支只删它自己 */
function cliDeleteTargets(): CliSession[] {
  const targets = new Map<string, CliSession>();
  for (const id of state.selectedCli) {
    const group = findCliGroup(id);
    if (!group) continue;
    if (group.rep.sessionId === id) {
      targets.set(group.rep.sessionId, group.rep);
      for (const branch of group.branches) targets.set(branch.sessionId, branch);
    } else {
      const branch = group.branches.find((b) => b.sessionId === id);
      if (branch) targets.set(id, branch);
    }
  }
  return [...targets.values()];
}

function desktopDeleteTargets(): { session: DesktopSession; transcript: CliSession | undefined }[] {
  const byId = new Map((state.report?.cliSessions ?? []).map((s) => [s.sessionId, s]));
  return [...state.selectedDesktop]
    .map((file) => state.desktopSessions.find((s) => s.fileName === file))
    .filter((s): s is DesktopSession => s !== undefined)
    .map((session) => ({
      session,
      transcript: session.cliSessionId ? byId.get(session.cliSessionId) : undefined,
    }));
}

function groupTitle(group: CliGroup): string {
  if (group.rep.firstUserText) return group.rep.firstUserText;
  // compact 续接的代表没有可读标题时，回退到最早分支的原始首句
  for (let i = group.branches.length - 1; i >= 0; i--) {
    const text = group.branches[i].firstUserText;
    if (text) return text;
  }
  return group.rep.sessionId;
}

function cliRowHtml(s: CliSession, options: { title?: string; branch?: boolean; badge?: string } = {}): string {
  const checked = state.selectedCli.has(s.sessionId);
  const pill = s.registered
    ? '<span class="pill pill-registered">✓ 已注册</span>'
    : s.tombstoned
      ? '<span class="pill pill-tombstone">墓碑</span>'
      : "";
  return `
    <div class="session-row ${options.branch ? "branch-row" : ""} ${checked ? "selected" : ""}" data-cli="${s.sessionId}">
      <input type="checkbox" ${checked ? "checked" : ""} tabindex="-1"/>
      <div class="session-body">
        <div class="session-title">${esc(options.title ?? s.firstUserText ?? s.sessionId)}</div>
        <div class="session-sub">
          <span class="mono">${shortId(s.sessionId)}</span>
          <span>${timeOf(s.lastActivityMs)}</span>
          <span>${sizeOf(s.sizeBytes)}</span>
          ${s.entrypoint ? `<span>${esc(s.entrypoint)}</span>` : ""}
          ${s.cwd ? `<span>${esc(s.cwd)}</span>` : ""}
        </div>
      </div>
      ${options.badge ?? ""}
      ${pill}
    </div>`;
}

function renderCliColumn(): string {
  const byProject = cliGroupsByProject();
  const blocks: string[] = [];
  for (const [project, groups] of byProject) {
    const selectable = groups.filter(repRegistrable).map((g) => g.rep.sessionId);
    const allChecked = selectable.length > 0 && selectable.every((id) => state.selectedCli.has(id));
    const fileCount = groups.reduce((n, g) => n + 1 + g.branches.length, 0);
    const countNote =
      fileCount === groups.length ? `· ${groups.length}` : `· ${groups.length} 会话 / ${fileCount} 分支文件`;
    blocks.push(`
      <div class="group-label">
        <label><input type="checkbox" data-group-cli="${esc(project)}" ${allChecked ? "checked" : ""} ${
          selectable.length === 0 ? "disabled" : ""
        }/> ${esc(project)}</label>
        <span>${countNote}</span>
      </div>`);
    for (const group of groups) {
      if (group.branches.length === 0) {
        blocks.push(cliRowHtml(group.rep));
        continue;
      }
      const expanded = state.expandedGroups.has(group.groupId);
      const branchRegistered = !group.rep.registered && group.anyRegistered;
      const badge = `
        ${branchRegistered ? '<span class="pill pill-branch">分支已注册</span>' : ""}
        <button class="branch-toggle ${expanded ? "open" : ""}" data-expand-group="${esc(group.groupId)}"
          data-group-rep="${group.rep.sessionId}"
          title="${expanded ? "收起" : "展开"}同一会话的 ${group.branches.length} 个历史分支">
          ⑂ ${group.branches.length + 1}
        </button>`;
      blocks.push(cliRowHtml(group.rep, { title: groupTitle(group), badge }));
      if (expanded) {
        for (const branch of group.branches) blocks.push(cliRowHtml(branch, { branch: true }));
      }
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

function deleteBanner(): string {
  if (state.deleteConfirm === null) return "";
  const disabled = anyDesktopRunning() || state.busy;
  let note: string;
  if (state.deleteConfirm === "cli") {
    const files = cliDeleteTargets();
    const bytes = files.reduce((n, s) => n + s.sizeBytes, 0);
    const registered = files.filter((s) => s.registered).length;
    const branchNote =
      files.length > state.selectedCli.size ? `（含历史分支共 ${files.length} 个文件）` : "";
    note = `将删除 ${state.selectedCli.size} 个 CLI 会话${branchNote}，转录合计 ${sizeOf(bytes)}${
      registered ? `；其中 ${registered} 个已注册到 Desktop，条目会一并清掉` : ""
    }。`;
  } else {
    const targets = desktopDeleteTargets();
    const withTranscript = targets.filter((t) => t.transcript !== undefined);
    const bytes = withTranscript.reduce((n, t) => n + (t.transcript?.sizeBytes ?? 0), 0);
    note = `将删除 ${targets.length} 个 Desktop 条目${
      withTranscript.length ? `，连同 ${withTranscript.length} 份 CLI 转录（${sizeOf(bytes)}）` : ""
    }。`;
  }
  return `
    <div class="banner">
      <span>${note}转录送系统回收站，可从回收站找回；Desktop 元数据不留墓碑，直接消失。</span>
      <button class="btn btn-secondary btn-small btn-danger" data-action="delete-confirm" ${
        disabled ? "disabled" : ""
      }>确认删除</button>
      <button class="btn btn-tertiary btn-small" data-action="delete-cancel">取消</button>
    </div>`;
}

function renderMigrate(): string {
  const runningNote = anyDesktopRunning()
    ? '<div class="banner banner-error">Claude Desktop 正在运行 —— 迁移已禁用，请先退出 Desktop</div>'
    : "";
  const cliCount = state.selectedCli.size;
  const deskCount = state.selectedDesktop.size;
  const registerable = registerableSelection().length;
  const disabled = anyDesktopRunning() || state.busy;
  return `
    <div class="view view-wide">
      <div class="page-head">
        <div>
          <div class="eyebrow">会话迁移</div>
          <h1>CLI 与 Desktop 互认会话</h1>
          <p class="lead">注册 = 为 CLI 会话生成 Desktop 元数据（转录零拷贝，两边同源）；注销 = 只移除 Desktop 元数据（转录保留，CLI 照常 resume）；删除 = 两侧痕迹一并抹掉，转录进回收站。</p>
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
      ${deleteBanner()}
      <div class="migrate-grid">
        <div class="column">
          <div class="column-head">
            <span class="eyebrow">CLI 会话 · ~/.claude/projects</span>
            <button class="btn btn-tertiary btn-small" data-action="cli-select-all">全选可注册</button>
            <button class="btn btn-tertiary btn-small" data-action="cli-clear">清空选中</button>
            <button class="btn btn-tertiary btn-small btn-danger" data-action="delete-cli" ${
              cliCount === 0 || disabled ? "disabled" : ""
            }>删除（${cliCount}）</button>
          </div>
          <div class="column-scroll" id="cli-column">${renderCliColumn()}</div>
        </div>
        <div class="migrate-actions">
          <button class="btn btn-primary" data-action="register" ${
            registerable === 0 || disabled ? "disabled" : ""
          }>
            注册 → （${registerable}）
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
            <button class="btn btn-tertiary btn-small btn-danger" data-action="delete-desktop" ${
              deskCount === 0 || disabled ? "disabled" : ""
            }>删除（${deskCount}）</button>
          </div>
          <div class="column-scroll" id="desktop-column">${renderDesktopColumn()}</div>
        </div>
      </div>
    </div>`;
}

/* ---------- Codex 迁移页 ---------- */

const CODEX_STATUS_META: Record<CodexStatus, { label: string; cls: string }> = {
  nativeOnly: { label: "Claude 无此会话", cls: "pill-pending" },
  migrated: { label: "已迁移", cls: "pill-registered" },
  mirror: { label: "Claude 已有", cls: "pill-canonical" },
  orphanMirror: { label: "镜像 · 源已删", cls: "pill-tombstone" },
};

function codexFiltered(): CodexSession[] {
  const all = state.codexSessions;
  switch (state.codexFilter) {
    case "nativeOnly":
      return all.filter((s) => s.status === "nativeOnly");
    case "migrated":
      return all.filter((s) => s.status === "migrated");
    case "mirror":
      return all.filter((s) => s.status === "mirror" || s.status === "orphanMirror");
    default:
      return all;
  }
}

function renderCodex(): string {
  const all = state.codexSessions;
  const counts = {
    nativeOnly: all.filter((s) => s.status === "nativeOnly").length,
    migrated: all.filter((s) => s.status === "migrated").length,
    mirror: all.filter((s) => s.status === "mirror" || s.status === "orphanMirror").length,
  };
  const disabled = state.codexRunning || state.busy;
  const runningNote = state.codexRunning
    ? '<div class="banner banner-error">Codex 正在运行 —— 迁移会写入 Codex 的导入记录（防止它把迁移产物同步回去），请先完全退出 Codex</div>'
    : "";
  const filters: { key: CodexFilter; label: string }[] = [
    { key: "all", label: `全部 ${all.length}` },
    { key: "nativeOnly", label: `Claude 无 ${counts.nativeOnly}` },
    { key: "migrated", label: `已迁移 ${counts.migrated}` },
    { key: "mirror", label: `镜像 ${counts.mirror}` },
  ];
  const chips = filters
    .map(
      (f) => `<button class="filter-chip ${state.codexFilter === f.key ? "active" : ""}" data-codex-filter="${f.key}">${f.label}</button>`,
    )
    .join("");

  const rows = codexFiltered()
    .map((s) => {
      const meta = CODEX_STATUS_META[s.status];
      const selectable = s.status === "nativeOnly";
      const checked = state.selectedCodex.has(s.threadId);
      return `
        <div class="session-row ${checked ? "selected" : ""}" data-codex="${s.threadId}" ${
          selectable ? "" : 'data-disabled="1"'
        }>
          <input type="checkbox" ${checked ? "checked" : ""} ${selectable ? "" : "disabled"} tabindex="-1"/>
          <div class="session-body">
            <div class="session-title">${esc(s.title ?? s.threadId)}</div>
            <div class="session-sub">
              <span class="mono">${shortId(s.threadId)}</span>
              <span>${timeOf(s.lastActivityMs)}</span>
              <span>${sizeOf(s.sizeBytes)}</span>
              ${s.originator ? `<span>${esc(s.originator)}</span>` : ""}
              ${s.cwd ? `<span>${esc(s.cwd)}</span>` : ""}
            </div>
          </div>
          <span class="pill ${meta.cls}">${meta.label}</span>
        </div>`;
    })
    .join("");

  return `
    <div class="view view-wide">
      <div class="page-head">
        <div>
          <div class="eyebrow">Codex 会话迁移</div>
          <h1>把 Codex 原生对话带进 Claude</h1>
          <p class="lead">Codex 会把 Claude 转录同步为自己的会话；反向则由这里完成 —— 原生对话转为 Claude 转录（零破坏、幂等），迁移后可在「会话迁移」页注册进 Desktop，CLI 侧 claude -r 直接续聊。</p>
        </div>
        <div class="head-actions">
          <button class="btn btn-secondary" data-action="refresh">重新扫描</button>
          <button class="btn btn-tertiary" data-action="codex-select-native">全选可迁移</button>
          <button class="btn btn-tertiary" data-action="codex-clear">清空选中</button>
          <button class="btn btn-primary" data-action="codex-migrate" ${
            state.selectedCodex.size === 0 || disabled ? "disabled" : ""
          }>迁移到 Claude（${state.selectedCodex.size}）</button>
        </div>
      </div>
      ${runningNote}
      ${
        counts.nativeOnly === 0
          ? '<div class="banner">没有待迁移的 Codex 原生会话 —— 全部会话在 Claude 侧均有对应。</div>'
          : ""
      }
      <div class="filter-row">${chips}</div>
      <div class="column-scroll" id="codex-column">${rows || '<div class="empty-note">未发现 Codex 会话（~/.codex/sessions）</div>'}</div>
    </div>`;
}

async function doMigrateCodex() {
  state.busy = true;
  render();
  try {
    const ids = [...state.selectedCodex];
    const reports = await invoke<CodexMigrateReport[]>("migrate_codex_sessions", {
      threadIds: ids,
    });
    let ok = 0;
    let turns = 0;
    for (const r of reports) {
      if (r.error) {
        toast(`${shortId(r.threadId)}: ${r.error}`, true);
      } else if (r.outcome?.status === "migrated") {
        ok += 1;
        turns += r.outcome.turns ?? 0;
      } else if (r.outcome?.status === "alreadyMigrated") {
        toast(`${shortId(r.threadId)}: 已迁移过，跳过`);
      } else if (r.outcome?.status === "skipped") {
        toast(`${shortId(r.threadId)}: ${r.outcome.reason ?? "跳过"}`, true);
      }
    }
    if (ok) toast(`已迁移 ${ok} 个会话（共 ${turns} 轮对话）—— 去「会话迁移」页注册即可在 Desktop 打开`);
    state.selectedCodex.clear();
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.busy = false;
    await refresh();
  }
}

/* ---------- 会话预览 ---------- */

function openPreview(kind: "claude" | "codex", id: string | null, title: string) {
  if (!id) {
    toast("此会话没有关联转录，无法预览");
    return;
  }
  void invoke("open_preview", { kind, id, title }).catch((error) => toast(String(error), true));
}

function previewTimeOf(iso: string | null): string {
  if (!iso) return "";
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? "" : fmtTime.format(new Date(ms));
}

/* 转录内容不可信，渲染产物必须过 DOMPurify */
function renderMarkdown(text: string): string {
  const html = marked.parse(text, { async: false, breaks: true, gfm: true }) as string;
  return DOMPurify.sanitize(html);
}

async function initPreview(kind: string, id: string) {
  const close = () => void tauriWindow()?.close();
  app.innerHTML = `
    <header class="topbar preview-topbar" data-tauri-drag-region>
      <div class="preview-head-title">会话预览</div>
      ${tauriWindow() ? `<div class="win-controls"><button class="win-btn win-close" data-preview-close title="关闭">${SVG_CLOSE}</button></div>` : ""}
    </header>
    <main class="preview-scroll"><div class="loading">正在读取会话内容…</div></main>`;
  app.addEventListener("click", (event) => {
    if ((event.target as HTMLElement).closest("[data-preview-close]")) close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") close();
  });

  const main = app.querySelector<HTMLElement>(".preview-scroll")!;
  try {
    const preview = await invoke<SessionPreview>("load_preview", { kind, id });
    const head = app.querySelector<HTMLElement>(".preview-head-title")!;
    head.textContent = preview.title ?? "会话预览";
    const rows = preview.messages
      .map((m) => {
        if (m.role === "tool") {
          return `<div class="msg-tool">⚙ ${esc(m.text)}</div>`;
        }
        if (m.role === "user") {
          const time = previewTimeOf(m.timestamp);
          return `
            <div class="msg-user-wrap">
              <div class="msg-user-bubble">${esc(m.text)}</div>
              ${time ? `<div class="msg-time">${time}</div>` : ""}
            </div>`;
        }
        return `<div class="msg-assistant-flow md">${renderMarkdown(m.text)}</div>`;
      })
      .join("");
    main.innerHTML = `
      <div class="preview-meta">
        ${preview.cwd ? `<span class="mono">${esc(preview.cwd)}</span>` : ""}
        <span>共 ${preview.totalMessages} 条消息</span>
        ${preview.truncated ? `<span>· 仅显示最近 ${preview.messages.length} 条</span>` : ""}
      </div>
      ${rows || '<div class="empty-note">没有可显示的对话内容</div>'}`;
    main.scrollTop = 0;
  } catch (error) {
    main.innerHTML = `<div class="loading">${esc(String(error))}</div>`;
  }
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

/* 选择一变，之前那批的删除确认就失效了，避免确认的和删掉的不是同一批 */
function syncSelection() {
  if (state.deleteConfirm) {
    state.deleteConfirm = null;
    render();
    return;
  }
  updateMigrateSelection();
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
  const groupsByProject = cliGroupsByProject();
  for (const box of app.querySelectorAll<HTMLInputElement>("[data-group-cli]")) {
    const reps = (groupsByProject.get(box.dataset.groupCli!) ?? []).filter(repRegistrable);
    box.checked = reps.length > 0 && reps.every((g) => state.selectedCli.has(g.rep.sessionId));
  }
  const disabled = anyDesktopRunning() || state.busy;
  const registerable = registerableSelection().length;
  const register = app.querySelector<HTMLButtonElement>('[data-action="register"]');
  if (register) {
    register.textContent = `注册 → （${registerable}）`;
    register.disabled = registerable === 0 || disabled;
  }
  const deleteCli = app.querySelector<HTMLButtonElement>('[data-action="delete-cli"]');
  if (deleteCli) {
    deleteCli.textContent = `删除（${state.selectedCli.size}）`;
    deleteCli.disabled = state.selectedCli.size === 0 || disabled;
  }
  const deleteDesktop = app.querySelector<HTMLButtonElement>('[data-action="delete-desktop"]');
  if (deleteDesktop) {
    deleteDesktop.textContent = `删除（${state.selectedDesktop.size}）`;
    deleteDesktop.disabled = state.selectedDesktop.size === 0 || disabled;
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
        <button class="tab ${state.tab === "codex" ? "active" : ""}" data-tab="codex">Codex 会话迁移</button>
      </nav>
      ${watchChip()}
      ${windowControls()}
    </header>
    <main>${state.tab === "unify" ? renderUnify() : state.tab === "migrate" ? renderMigrate() : renderCodex()}</main>
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
    const ids = registerableSelection();
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
      } else if (r.outcome?.status === "siblingRegistered") {
        toast(`${shortId(r.sessionId)}: 同一会话的另一分支已在 Desktop，跳过`);
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

async function doDelete() {
  const side = state.deleteConfirm;
  if (side === null) return;
  state.busy = true;
  render();
  try {
    const reports = await invoke<DeleteReport[]>("delete_sessions", {
      cliSessionIds: side === "cli" ? cliDeleteTargets().map((s) => s.sessionId) : [],
      metadataFiles: side === "desktop" ? [...state.selectedDesktop] : [],
    });
    let transcripts = 0;
    let entries = 0;
    for (const r of reports) {
      if (r.error) {
        toast(`${shortId(r.target)}: ${r.error}`, true);
        continue;
      }
      if (r.outcome?.transcriptRemoved) transcripts += 1;
      if (r.outcome?.metadataRemoved) entries += 1;
    }
    const parts = [];
    if (transcripts) parts.push(`${transcripts} 份转录已送回收站`);
    if (entries) parts.push(`${entries} 个 Desktop 条目已清除`);
    toast(parts.length ? `删除完成：${parts.join("，")}` : "没有可删除的内容");
    if (side === "cli") state.selectedCli.clear();
    else state.selectedDesktop.clear();
    state.deleteConfirm = null;
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

function initMain() {
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
    if (action === "delete-cli" || action === "delete-desktop") {
      state.deleteConfirm = action === "delete-cli" ? "cli" : "desktop";
      render();
    }
    if (action === "delete-cancel") {
      state.deleteConfirm = null;
      render();
    }
    if (action === "delete-confirm") void doDelete();
    if (action === "purge-markers") void doPurge(false);
    if (action === "purge-all") void doPurge(true);
    if (action === "cli-select-all") {
      // tombstoned/已注册组会在注册时被挡下，批量勾入只会刷一屏错误 toast
      for (const groups of cliGroupsByProject().values()) {
        for (const group of groups) {
          if (repRegistrable(group)) state.selectedCli.add(group.rep.sessionId);
        }
      }
      syncSelection();
    }
    if (action === "cli-clear") {
      state.selectedCli.clear();
      syncSelection();
    }
    if (action === "desk-select-all") {
      for (const s of state.desktopSessions) {
        if (!s.sandboxed) state.selectedDesktop.add(s.fileName);
      }
      syncSelection();
    }
    if (action === "desk-clear") {
      state.selectedDesktop.clear();
      syncSelection();
    }
    if (action === "codex-select-native") {
      for (const s of state.codexSessions) {
        if (s.status === "nativeOnly") state.selectedCodex.add(s.threadId);
      }
      render();
    }
    if (action === "codex-clear") {
      state.selectedCodex.clear();
      render();
    }
    if (action === "codex-migrate") void doMigrateCodex();
    return;
  }
  const filterChip = target.closest<HTMLElement>("[data-codex-filter]");
  if (filterChip) {
    state.codexFilter = filterChip.dataset.codexFilter as CodexFilter;
    render();
    return;
  }
  const clickedCheckbox = (target as HTMLElement).closest('input[type="checkbox"]') !== null;
  const codexRow = target.closest<HTMLElement>("[data-codex]");
  if (codexRow) {
    const id = codexRow.dataset.codex!;
    if (!clickedCheckbox) {
      const title = codexRow.querySelector(".session-title")?.textContent ?? "会话预览";
      openPreview("codex", id, title);
      return;
    }
    if (codexRow.dataset.disabled) return;
    if (state.selectedCodex.has(id)) state.selectedCodex.delete(id);
    else state.selectedCodex.add(id);
    render();
    return;
  }
  const expandButton = target.closest<HTMLElement>("[data-expand-group]");
  if (expandButton) {
    const groupId = expandButton.dataset.expandGroup!;
    if (state.expandedGroups.has(groupId)) {
      state.expandedGroups.delete(groupId);
      // 收起后分支行不可见，留着选中等于静默注册看不见的会话
      const group = findCliGroup(expandButton.dataset.groupRep!);
      for (const branch of group?.branches ?? []) state.selectedCli.delete(branch.sessionId);
    } else {
      state.expandedGroups.add(groupId);
    }
    render();
    return;
  }
  const groupCli = target.closest<HTMLInputElement>("[data-group-cli]");
  if (groupCli) {
    const project = groupCli.dataset.groupCli!;
    const reps = (cliGroupsByProject().get(project) ?? []).filter(repRegistrable).map((g) => g.rep);
    const allIn = reps.every((s) => state.selectedCli.has(s.sessionId));
    for (const s of reps) {
      if (allIn) state.selectedCli.delete(s.sessionId);
      else state.selectedCli.add(s.sessionId);
    }
    syncSelection();
    return;
  }
  const cliRow = target.closest<HTMLElement>("[data-cli]");
  if (cliRow) {
    const id = cliRow.dataset.cli!;
    if (!clickedCheckbox) {
      const title = cliRow.querySelector(".session-title")?.textContent ?? "会话预览";
      openPreview("claude", id, title);
      return;
    }
    if (state.selectedCli.has(id)) state.selectedCli.delete(id);
    else selectExclusiveInGroup(id);
    syncSelection();
    return;
  }
  const deskRow = target.closest<HTMLElement>("[data-desktop]");
  if (deskRow) {
    const file = deskRow.dataset.desktop!;
    if (!clickedCheckbox) {
      const session = state.desktopSessions.find((s) => s.fileName === file);
      const title = deskRow.querySelector(".session-title")?.textContent ?? "会话预览";
      openPreview("claude", session?.cliSessionId ?? null, title);
      return;
    }
    if (state.selectedDesktop.has(file)) state.selectedDesktop.delete(file);
    else state.selectedDesktop.add(file);
    syncSelection();
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
}

const previewSpec = (window as { __PREVIEW__?: { kind: string; id: string } }).__PREVIEW__;
if (previewSpec) {
  void initPreview(previewSpec.kind, previewSpec.id);
} else {
  initMain();
}
