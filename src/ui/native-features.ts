import type { ComputerUseStatus } from "../state";
import { esc } from "./text";
import { renderDesktopUpdate, type DesktopUpdateStatus } from "./desktop-update";

export interface NativeSelection {
  computerUse: boolean;
  chinese: boolean;
  simulators: boolean;
  prewarm: boolean;
  fastModeStatus: boolean;
}
export interface NativeStatus {
  installation: ComputerUseStatus;
  selection: NativeSelection;
  supportedFeatures: string[];
  update?: DesktopUpdateStatus;
}
export const emptySelection = (): NativeSelection => ({ computerUse: false, chinese: false, simulators: false, prewarm: false, fastModeStatus: false });
export type FeatureGroup = "all" | "interaction" | "appearance" | "workflow";
interface Feature {
  id: string;
  name: string;
  group: Exclude<FeatureGroup, "all">;
  description: string;
  detail: string;
  key?: keyof NativeSelection;
  reason?: string;
  experimental?: boolean;
}
export const features: Feature[] = [
  { id: "computer-use", key: "computerUse", name: "Computer Use", group: "interaction", description: "让 Claude 看屏幕、动鼠标和键盘。", detail: "装完去 Claude 设置里打开 Computer use，按提示授权屏幕录制和辅助功能。" },
  { id: "simulators", key: "simulators", name: "iOS / Android 模拟器", group: "interaction", description: "在模拟器里实时看画面、点界面。", detail: "需要本机装好 Xcode 或 Android SDK，只能在本地 Code 会话里用。" },
  { id: "chinese", key: "chinese", name: "简体与繁体中文", group: "appearance", description: "界面语言多出简体和繁體中文。", detail: "装完在 Claude 设置的语言里选。官方词典没覆盖到的地方仍是英文。" },
  { id: "prewarm", key: "prewarm", name: "CLI 原生预热", group: "workflow", description: "提前备好进程，新会话少等一会儿。", detail: "会多占一些常驻内存，模型回复本身不会变快。", experimental: true },
  { id: "fast-mode-status", key: "fastModeStatus", name: "Fast Mode 原生开关", group: "workflow", description: "模型菜单里直接开关 Fast。", detail: "开关跟着 CLI 的实际状态走。是否真的 fast 看转录里的 usage.speed。" },
  { id: "name", name: "姓名与昵称", group: "appearance", description: "改个人资料里的名字和称呼。", detail: "账户数据的位置找到了，但输入界面还没验证通过。", reason: "编辑界面待验证" },
  { id: "photo", name: "照片头像", group: "appearance", description: "上传照片当头像。", detail: "裁剪界面是现成的，但照片存哪、怎么读回来要靠官方服务。图案头像不受影响。", reason: "缺少照片服务" },
  { id: "artifact-pane", name: "Artifacts 原生侧栏", group: "workflow", description: "在侧栏里浏览和管理产物。", detail: "入口能打开，但产物读不出来，开了也是空面板。", reason: "数据链路待验证" },
  { id: "artifact-popout", name: "Artifacts 独立窗口", group: "workflow", description: "把产物弹成单独的窗口预览。", detail: "要资源路由、窗口校验和产物数据三样都成立才能用。", reason: "窗口链路待验证" },
  { id: "extensions", name: "本地扩展安装", group: "workflow", description: "装 MCPB / DXT 扩展。", detail: "安装流程要连组织策略后端。已有的本地 MCP 和插件照常能用。", reason: "缺少策略服务" },
  { id: "dictation", name: "快速输入听写", group: "interaction", description: "说话转成文字。", detail: "要配好官方的语音服务。", reason: "语音服务待配置" },
  { id: "contrast", name: "近黑高对比主题", group: "appearance", description: "更深的高对比配色。", detail: "样式来自另一个构建，和当前版本能不能对上还没验证。", reason: "组件移植待验证" },
  { id: "instruction-guide", name: "个性化指令引导", group: "appearance", description: "写个性化指令时给示例卡片。", detail: "指令本身已经能编辑，缺的是引导卡片的依赖。", reason: "组件移植待验证" },
];

export function selectedNames(selection: NativeSelection): string[] {
  return features.filter(f => f.key && selection[f.key]).map(f => f.name);
}
export function selectionChanged(a: NativeSelection, b: NativeSelection): boolean {
  return (Object.keys(a) as (keyof NativeSelection)[]).some(key => a[key] !== b[key]);
}
export function selectionSupported(selection: NativeSelection, supported: string[]): boolean {
  return (Object.keys(selection) as (keyof NativeSelection)[]).every(key => !selection[key] || supported.includes(key));
}
export interface FeatureView {
  status: NativeStatus | null;
  draft: NativeSelection;
  group: FeatureGroup;
  search: string;
  busy: boolean;
  scanning: boolean;
  error: string | null;
  identity: string;
  confirm: boolean;
  result: string | null;
}
export function canApply(v: FeatureView): boolean {
  const s = v.status;
  if (!s || v.busy || v.scanning || s.installation.running) return false;
  if ((!selectionChanged(v.draft, s.selection) && s.installation.state !== "upgradeAvailable") || !selectionSupported(v.draft, s.supportedFeatures)) return false;
  if (selectedNames(v.draft).length === 0) return s.installation.canRestore;
  if (s.update?.enabled && ["waitingForExit", "restoring", "awaitingUpdate", "waitingForReapply", "applying"].includes(s.update.phase)) return false;
  return s.installation.canInstall && (s.installation.requiresSigning === false || s.installation.identities.some(i => i.fingerprint === v.identity));
}
function featureCard(f: Feature, v: FeatureView): string {
  const installed = f.key ? v.status?.selection[f.key] === true : false;
  const supported = f.key ? v.status?.supportedFeatures.includes(f.key) === true : false;
  const pending = !!f.key && v.draft[f.key] !== installed;
  const label = f.reason ? "待原生链路" : installed ? "已安装" : !v.status ? "待检测" : supported ? "可安装" : "当前未适配";
  const disabled = v.busy || v.scanning || !supported;
  return `<article class="feature-card ${pending ? "feature-pending" : ""}" data-feature-id="${f.id}">
    <div class="feature-heading"><span class="feature-glyph" aria-hidden="true">${f.group === "appearance" ? "Aa" : f.group === "interaction" ? "↗" : "⌘"}</span><span class="pill ${installed ? "pill-registered" : f.reason ? "pill-foreign" : "pill-pending"}">${label}</span></div>
    <h2>${esc(f.name)}${f.experimental ? '<span class="feature-experiment">实验</span>' : ""}</h2><p>${esc(f.description)}</p>
    <details class="feature-detail"><summary>使用说明${f.reason ? "与限制" : ""}</summary><p>${esc(f.detail)}</p></details>
    <div class="feature-footer">${f.key ? `<label class="toggle"><input type="checkbox" data-native-feature="${f.key}" ${v.draft[f.key] ? "checked" : ""} ${disabled ? "disabled" : ""}/><span>${pending ? "待应用" : "加入功能组合"}</span></label>` : `<span class="dependency-note">${esc(f.reason ?? "")}</span>`}</div>
  </article>`;
}
export function renderFeatures(v: FeatureView): string {
  const term = v.search.trim().toLocaleLowerCase();
  const visible = features.filter(f => (v.group === "all" || f.group === v.group) && `${f.name} ${f.description} ${f.detail}`.toLocaleLowerCase().includes(term));
  const native = v.status?.installation;
  const dirty = !!v.status && selectionChanged(v.draft, v.status.selection);
  const chosen = selectedNames(v.draft);
  return `<div class="view feature-view"><div class="page-head"><div><div class="eyebrow">NATIVE FEATURES</div><h1>原生能力，按需组合</h1><p class="lead">勾选要装的功能，一次应用。改动前会完整备份，随时能还原。</p></div><button class="btn btn-secondary" data-action="native-refresh" ${v.busy || v.scanning ? "disabled" : ""}>${v.scanning ? "正在检测…" : "重新检测"}</button></div>
    ${v.error ? `<div class="banner banner-error" role="alert">${esc(v.error)}</div>` : ""}
    ${v.result ? `<div class="banner" role="status">${esc(v.result)}</div>` : ""}
    ${native?.running ? '<div class="banner">Desktop 开着，应用前请先退出。</div>' : ""}
    ${renderDesktopUpdate(v.status?.update ?? null, v.busy || v.scanning)}
    <div class="feature-toolbar"><div class="filter-group" aria-label="功能分类">${([["all", "全部"], ["interaction", "交互与控制"], ["appearance", "语言与个性化"], ["workflow", "工作流"]] as const).map(([id, name]) => `<button class="filter-button ${v.group === id ? "active" : ""}" data-feature-group="${id}" aria-pressed="${v.group === id}">${name}</button>`).join("")}</div><input id="feature-search" class="search-input" type="search" placeholder="搜索功能与说明" aria-label="搜索功能" value="${esc(v.search)}"/></div>
    <div class="feature-grid">${visible.map(f => featureCard(f, v)).join("") || '<div class="empty-note">没有匹配的功能，试试其他关键词。</div>'}</div>
    <div class="apply-panel"><div><strong>${dirty ? "有未应用的更改" : "功能组合"}</strong><p>${chosen.length ? esc(chosen.join(" · ")) : "未选择增强功能"}</p></div><div class="apply-controls">${native && native.requiresSigning !== false ? `<label class="signing-control">签名身份<select id="native-signing-identity" ${v.busy ? "disabled" : ""}><option value="">请选择</option>${native.identities.map(i => `<option value="${esc(i.fingerprint)}" ${v.identity === i.fingerprint ? "selected" : ""}>${esc(i.name)}</option>`).join("")}</select></label>` : ""}<button class="btn btn-secondary" data-action="native-reset" ${!dirty || v.busy ? "disabled" : ""}>撤销选择</button><button class="btn btn-primary" data-action="native-prepare" ${canApply(v) ? "" : "disabled"}>应用更改</button></div></div>
    ${v.confirm ? `<section class="card confirmation-card" role="region" aria-label="确认原生功能变更"><h2>确认${chosen.length ? "应用功能组合" : "还原官方应用"}？</h2><p>目标：<span class="mono">${esc(native?.appPath ?? "")}</span></p><p>从官方备份重新生成，关掉某一项不会影响其他选中的。${native?.requiresSigning ? "应用会重新签名，之后可能要重新登录。" : "修改后的 exe 不再带官方签名，还原即恢复。"}</p>${v.draft.prewarm ? '<p>预热是实验功能，会多占常驻内存。</p>' : ""}<div class="computer-use-actions"><button class="btn btn-primary" data-action="native-confirm" ${canApply(v) ? "" : "disabled"}>确认应用</button><button class="btn btn-secondary" data-action="native-cancel">取消</button></div></section>` : ""}
    <p class="feature-footnote">“已安装”只代表文件改好了，设备权限还要你在系统里单独授权。</p></div>`;
}
