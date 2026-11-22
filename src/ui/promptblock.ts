import type { AppState, PromptBlockInstall, PromptBlockState } from "../state";
import { esc } from "./text";

export interface PromptSegment {
  id: string;
  label: string;
  detail: string;
}

// 与 promptblock.rs 的 needle 表同序；行为引导类、管线合成消息、模型身份行刻意不在内
export const promptSegments: PromptSegment[] = [
  { id: "llm_rnd", label: "AI 研发能力限制声明", detail: "要求 Fable 5.2 / Mythos 5.2 在内核优化等方向降低自信、多查文档。" },
  { id: "fable_identity", label: "模型身份介绍", detail: "Fable 5 / 5.1 / 5.2 三代的“安全措施”自述段。" },
  { id: "original_games", label: "游戏复刻限制", detail: "禁止复刻具体现有游戏，改名也不行。" },
  { id: "security_policy", label: "安全测试政策", detail: "安全协助边界三句，整段抹掉。" },
  { id: "url_restriction", label: "URL 生成限制", detail: "非编程场景不得生成或猜测 URL。" },
  { id: "injection_warning", label: "防注入提醒", detail: "怀疑工具结果有 prompt injection 要先报告用户。" },
  { id: "zct_denial", label: "权限拒绝回执说教", detail: "“别恶意绕过拒绝”一段；“停下来问用户”的三个后缀保留。" },
  { id: "refusal_nudge", label: "截停恢复禁令句", detail: "只抹“不得改写重产”三处禁令，被截停与工具未执行的事实说明保留。" },
  { id: "heron_brook", label: "服务端动态提示词通道", detail: "屏蔽 heron_brook / brook_heron 的远端文本入口。" },
];

const stateMeta: Record<PromptBlockState, { label: string; cls: string }> = {
  patched: { label: "已屏蔽", cls: "pill-registered" },
  pristine: { label: "官方原样", cls: "pill-pending" },
  updated: { label: "已被更新覆盖", cls: "pill-pending" },
  damaged: { label: "位点对不上", cls: "pill-tombstone" },
  broken: { label: "目录异常", cls: "pill-tombstone" },
  missing: { label: "安装已消失", cls: "pill-foreign" },
  legacy: { label: "可升级屏蔽", cls: "pill-pending" },
  unsupported: { label: "结构待适配", cls: "pill-foreign" },
};

export function applyTargets(installs: PromptBlockInstall[]): PromptBlockInstall[] {
  return installs.filter((i) => i.state === "pristine" || i.state === "updated" || i.state === "legacy");
}

function installBlocked(install: PromptBlockInstall, desktopRunning: boolean): boolean {
  return install.id.startsWith("bundled") && desktopRunning;
}

function installCard(install: PromptBlockInstall, busy: boolean, desktopRunning: boolean): string {
  const meta = stateMeta[install.state];
  const hitTotal = Object.values(install.segmentHits).reduce((n, c) => n + c, 0);
  const blocked = installBlocked(install, desktopRunning);
  const disabled = busy || blocked ? "disabled" : "";
  const canApply = applyTargets([install]).length > 0;
  const canRestore = install.appliedAt !== null;
  return `<div class="card" data-install="${esc(install.id)}">
    <div class="card-title">
      <span>${esc(install.kind)}</span>
      <span class="pill ${meta.cls}">${meta.label}</span>
    </div>
    <div class="card-meta">
      <span>版本 <span class="mono">${esc(install.cliVersion ?? "—")}</span></span>
      <span class="mono">${esc(install.exePath)}</span>
      ${hitTotal ? `<span>已识别 ${Object.keys(install.segmentHits).length} 类提示词，共 ${hitTotal} 处文本</span>` : ""}
      ${install.state === "legacy" ? "<span>当前已安装 heron 屏蔽，其他已识别项目可通过升级应用。</span>" : install.appliedAt ? `<span>屏蔽于 ${esc(install.appliedAt)}${hitTotal ? `，覆写 ${hitTotal} 处` : ""}</span>` : ""}
      ${install.note ? `<span>${esc(install.note)}</span>` : ""}
      ${blocked ? "<span>Desktop 开着，先退出才能改这份 CLI</span>" : ""}
    </div>
    <div class="computer-use-actions">
      ${canApply ? `<button class="btn btn-primary" data-action="promptblock-apply" ${disabled}>屏蔽注入段</button>` : ""}
      ${canRestore ? `<button class="btn btn-secondary" data-action="promptblock-restore" ${disabled}>${install.state === "patched" || install.state === "legacy" ? "还原官方内容" : "清除记录"}</button>` : ""}
    </div>
  </div>`;
}

export function renderPromptBlock(state: AppState): string {
  const status = state.promptblock;
  const desktopRunning = state.running.some((r) => r.running);
  const targets = status ? applyTargets(status.installs) : [];
  const allDisabled =
    state.busy || targets.length === 0 || targets.some((i) => installBlocked(i, desktopRunning));
  const head = `<div class="page-head">
      <div>
        <div class="eyebrow">PROMPT BLOCK</div>
        <h1>把注入的限制提示词关掉</h1>
        <p class="lead">管理本机 Claude Code 的提示词屏蔽，修改前备份，支持还原。</p>
      </div>
      <div class="head-actions">
        <button class="btn btn-secondary" data-action="refresh" ${state.busy ? "disabled" : ""}>重新扫描</button>
        ${status?.supported ? `<button class="btn btn-primary" data-action="promptblock-apply-all" ${allDisabled ? "disabled" : ""}>一键屏蔽全部</button>` : ""}
      </div>
    </div>`;
  if (!status) {
    return `<div class="view">${head}<div class="empty-note">屏蔽状态读取失败，请重新扫描</div></div>`;
  }
  if (!status.supported) {
    return `<div class="view">${head}<div class="banner">当前系统尚未适配提示词屏蔽。</div></div>`;
  }
  const cards = status.installs.map((i) => installCard(i, state.busy, desktopRunning)).join("");
  return `<div class="view">
    ${head}
    ${desktopRunning && status.installs.some((i) => i.id.startsWith("bundled")) ? '<div class="banner">Desktop 开着，自带 CLI 要等它退出后才能应用或还原。</div>' : ""}
    <section class="column-section">
      <div class="eyebrow">CLI 安装</div>
      ${cards || '<div class="empty-note">没找到 Claude Code CLI（Desktop 自带、原生安装或 npm 全局）。</div>'}
    </section>
    <section class="column-section">
      <div class="eyebrow">屏蔽的注入段</div>
      <div class="card">
        <dl class="diagnostic-list">
          ${promptSegments.map((s) => `<dt>${esc(s.label)}</dt><dd>${esc(s.detail)}</dd>`).join("")}
        </dl>
      </div>
    </section>
    <section class="column-section">
      <div class="eyebrow">边界</div>
      <div class="card"><div class="card-meta">
        <span>各版本包含的提示词不同，以安装卡片识别出的项目为准。</span>
        ${status.platform === "macos" ? "<span>macOS 会备份完整 CLI 应用并重新签名。重新启动会话后生效，旧提示词快照会重新生成。</span>" : ""}
        <span>finishing_work、willow_tern 等行为引导段和重试、续写等管线消息不动。</span>
        <span>CLI 或 Desktop 更新后补丁随旧文件消失，回这页重新应用即可。</span>
      </div></div>
    </section>
  </div>`;
}
