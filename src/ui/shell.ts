import type { AppState } from "../state";
import { features, selectedNames } from "./native-features";
import { esc } from "./text";

const navigation: { title: string; items: { id: AppState["tab"]; label: string; icon: string }[] }[] = [
  { title: "工作台", items: [{ id: "overview", label: "总览", icon: "◈" }, { id: "features", label: "功能增强", icon: "✦" }, { id: "appearance", label: "语言与个性化", icon: "Aa" }, { id: "fastmode", label: "Fast Mode", icon: "↯" }] },
  { title: "会话管理", items: [{ id: "unify", label: "存储归一", icon: "⊞" }, { id: "migrate", label: "Claude 会话", icon: "↔" }, { id: "codex", label: "Codex 导入", icon: "⇥" }] },
  { title: "维护", items: [{ id: "backups", label: "备份与恢复", icon: "↶" }, { id: "diagnostics", label: "诊断与设置", icon: "≡" }] },
];
export function renderNavigation(state: AppState): string {
  return `<aside class="sidebar"><nav aria-label="主导航">${navigation.map(group => `<div class="nav-group"><div class="nav-label">${group.title}</div>${group.items.map(item => `<button class="nav-item ${state.tab === item.id ? "active" : ""}" data-tab="${item.id}" ${state.tab === item.id ? 'aria-current="page"' : ""} ${state.busy ? "disabled" : ""}><span class="nav-icon" aria-hidden="true">${item.icon}</span><span>${item.label}</span>${item.id === "features" && state.nativeDirty ? '<span class="nav-dot" aria-label="有未应用的更改"></span>' : ""}</button>`).join("")}</div>`).join("")}</nav><div class="sidebar-footer"><span class="native-label">官方实现 · 独立管理</span><p>保留原生权限与授权流程</p></div></aside>`;
}
function packageState(state: AppState): string {
  const values: Record<string, string> = { installed: "已安装增强", available: "可以安装", notFound: "未找到 Desktop", unsupported: "需要适配", changed: "应用已变化", recovery: "需要恢复", unmanaged: "未托管的补丁", upgradeAvailable: "可升级" };
  return state.nativeScanning ? "正在检测" : state.nativeError ? "检测失败" : values[state.native?.installation.state ?? ""] ?? "等待检测";
}
export function renderOverview(state: AppState): string {
  const count = state.native ? selectedNames(state.native.selection).length : "—";
  const available = state.native?.supportedFeatures.length ?? "—";
  const s = state.native?.installation;
  return `<div class="view overview-view"><div class="page-head"><div><div class="eyebrow">YOUR DESKTOP, ORGANIZED</div><h1>把原生能力，放回你的工作流</h1><p class="lead">管理功能增强与会话存储。每次变更都有明确状态、验证和恢复路径。</p></div><button class="btn btn-secondary" data-action="refresh" ${state.busy ? "disabled" : ""}>重新扫描</button></div>
    <section class="overview-hero"><div><span class="hero-kicker">Claude Desktop</span><h2>${esc(packageState(state))}</h2><p>${esc(state.nativeError ?? s?.message ?? "正在读取安装记录与应用状态…")}</p><div class="hero-actions"><button class="btn btn-primary" data-tab="features">管理原生功能 <span aria-hidden="true">↗</span></button><button class="btn btn-secondary" data-tab="backups">查看恢复方案</button></div></div><div class="hero-mark" aria-hidden="true">✳</div></section>
    <div class="metric-grid"><section class="metric"><span>已安装原生增强</span><strong>${count}<small> / ${available}</small></strong><p>安装状态来自实际文件校验</p></section><section class="metric"><span>CLI 会话</span><strong>${state.report?.cliSessions.length ?? "—"}</strong><p>${state.scanError ? "会话扫描需要重试" : "保留原始会话与历史"}</p></section><section class="metric"><span>Desktop 状态</span><strong class="metric-word">${s ? s.running ? "运行中" : "已退出" : "待检测"}</strong><p>变更前检查进程，不自动退出</p></section></div>
    <div class="overview-columns"><section class="card"><div class="section-heading"><h2>常用操作</h2><span>按需使用</span></div><div class="quick-links"><button data-tab="features"><span>✦</span><div><strong>原生功能组合</strong><small>中文、模拟器、电脑操作与预热</small></div><span>→</span></button><button data-tab="migrate"><span>↔</span><div><strong>管理会话</strong><small>注册、迁移与查看历史</small></div><span>→</span></button><button data-tab="fastmode"><span>↯</span><div><strong>Fast Mode</strong><small>检查部署、使用记录与自动守护</small></div><span>→</span></button></div></section>
    <section class="card"><div class="section-heading"><h2>需要留意</h2></div><ul class="notice-list">${state.scanError ? `<li>${esc(state.scanError)}</li>` : ""}${state.nativeDirty ? '<li>功能选择尚未应用，离开页面不会自动安装。</li>' : ""}${s?.running ? '<li>安装或恢复前，请先退出 Claude Desktop。</li>' : ""}<li>原生授权仍由你确认，不会自动批准设备控制。</li><li>${features.filter(f => !f.key).length} 项候选仍缺原生链路；可查看具体依赖，不提供无效开关。</li><li>Desktop 更新后重新检测；未知内容不会自动改写。</li></ul></section></div></div>`;
}
export function renderBackups(state: AppState): string {
  const s = state.native?.installation;
  const enabled = state.native ? selectedNames(state.native.selection) : [];
  return `<div class="view"><div class="page-head"><div><div class="eyebrow">RECOVERY</div><h1>每次更改，都有回来的路</h1><p class="lead">组合更新始终从官方基线生成。还原应用不删除会话、API 配置或用户授权记录。</p></div></div>
    <section class="card"><div class="section-heading"><h2>当前原生增强</h2><span class="pill pill-pending">${esc(packageState(state))}</span></div><dl class="diagnostic-list"><dt>应用</dt><dd class="mono">${esc(s?.appPath ?? "尚未检测")}</dd><dt>原包备份</dt><dd class="mono">${esc(s?.backupPath ?? "首次安装时创建")}</dd><dt>功能组合</dt><dd>${esc(enabled.join("、") || "无已安装增强")}</dd></dl><div class="computer-use-actions"><button class="btn btn-secondary" data-tab="features">调整功能组合</button><button class="btn btn-danger" data-action="native-restore-prepare" ${!s?.canRestore || state.busy ? "disabled" : ""}>还原官方应用…</button></div></section>
    <section class="card"><h2>恢复前会检查什么</h2><ul class="notice-list"><li>备份哈希与官方应用身份是否一致。</li><li>当前应用是否仍属于这次安装记录，避免覆盖外部更新。</li><li>目标是否已经退出，签名与完整性是否有效。</li></ul></section><section class="card"><h2>其他恢复入口</h2><p>Fast Mode 的 CLI wrapper 与会话存储分别管理，恢复原生增强不会移除它们。</p><div class="computer-use-actions"><button class="btn btn-secondary" data-tab="fastmode">Fast Mode</button><button class="btn btn-secondary" data-tab="unify">会话存储</button></div></section></div>`;
}
export function renderDiagnostics(state: AppState): string {
  const s = state.native?.installation;
  return `<div class="view"><div class="page-head"><div><div class="eyebrow">DIAGNOSTICS</div><h1>状态清楚，问题有迹可循</h1><p class="lead">这里展示实际检测结果，不把无法读取的状态当作“已关闭”。</p></div><button class="btn btn-secondary" data-action="refresh" ${state.busy ? "disabled" : ""}>重新检测</button></div>
    <section class="card"><h2>Claude Desktop</h2><dl class="diagnostic-list"><dt>安装状态</dt><dd>${esc(packageState(state))}</dd><dt>检测版本</dt><dd>${esc(s?.desktopVersion ?? "未知")}</dd><dt>应用路径</dt><dd class="mono">${esc(s?.appPath ?? "未知")}</dd><dt>运行状态</dt><dd>${s ? s.running ? "运行中" : "已退出" : "未知"}</dd>${s?.requiresSigning ? `<dt>签名身份</dt><dd>${s.identities.length ? s.identities.map(i => esc(i.name)).join("<br>") : "未找到有效身份"}</dd>` : ""}<dt>最近结果</dt><dd>${esc(state.nativeError ?? state.nativeActionError ?? state.nativeResult ?? s?.message ?? "等待检测")}</dd></dl></section>
    <section class="card"><h2>会话与守护</h2><dl class="diagnostic-list"><dt>会话扫描</dt><dd>${esc(state.scanError ?? (state.report ? "正常" : "等待扫描"))}</dd><dt>自动守护</dt><dd>${state.watch ? state.watch.paused ? "已暂停" : state.watch.running ? "运行中" : "未运行" : "未知"}</dd><dt>最近事件</dt><dd>${esc(state.watch?.lastEvent ?? "暂无")}</dd><dt>Fast Mode</dt><dd>${esc(state.fastmode?.settings.lastFailure ?? (state.fastmode?.settings.installed ? "已安装" : "未安装或尚未检测"))}</dd></dl></section></div>`;
}
