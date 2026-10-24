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
  { id: "computer-use", key: "computerUse", name: "Computer Use", group: "interaction", description: "官方电脑操作、后台模式与授权卡片。", detail: "安装后在 Claude 的 Computer use 设置中启用。屏幕录制、辅助功能和每个应用的授权保持原样。" },
  { id: "simulators", key: "simulators", name: "iOS / Android 模拟器", group: "interaction", description: "原生实时画面、设备面板和模拟器工具。", detail: "沿用官方设备枚举、画面传输和操作实现。用于本地 Code 会话，需要 Xcode 或 Android SDK，并由你批准设备访问。" },
  { id: "chinese", key: "chinese", name: "简体与繁体中文", group: "appearance", description: "官方语言包、语言选择器和地区识别。", detail: "在 Claude 原生设置中选择简体中文或繁體中文。沿用官方翻译与地区解析；当前词典未覆盖的内容保留原文，不机器补译。" },
  { id: "prewarm", key: "prewarm", name: "CLI 原生预热", group: "workflow", description: "复用官方备用进程池，减少新会话冷启动等待。", detail: "实验性：可能增加常驻内存。仅移除部署限制并启用官方预热路径，不修改推理请求、凭据处理或权限判断；不承诺降低每轮模型等待时间。", experimental: true },
  { id: "fast-mode-status", key: "fastModeStatus", name: "Fast Mode 原生开关", group: "workflow", description: "官方模型菜单开关与 Fast 标记，跟随本地 CLI 状态。", detail: "复用官方模型菜单开关、Fast 标记和实时控制接口；关闭选择会保留到下次启动。模型支持、组织策略、额度与冷却限制仍由原生逻辑处理，实际速度以 usage.speed 为准。" },
  { id: "name", name: "姓名与昵称", group: "appearance", description: "官方个人资料和称呼设置。", detail: "3p 的原生账户接口拒绝保存姓名。当前不创建本地替代账户接口。", reason: "缺少原生账户写入链路" },
  { id: "photo", name: "照片头像", group: "appearance", description: "官方照片上传、裁剪与头像显示。", detail: "裁剪组件存在，但照片存储与回填依赖官方服务。普通图案头像不受此限制。", reason: "缺少原生照片服务" },
  { id: "artifact-pane", name: "Artifacts 原生侧栏", group: "workflow", description: "使用官方产物浏览与管理面板。", detail: "入口可见不等于产物可读取。没有完整的本地原生数据链路前，不只打开空面板。", reason: "原生数据链路待验证" },
  { id: "artifact-popout", name: "Artifacts 独立窗口", group: "workflow", description: "官方产物弹窗和独立预览。", detail: "需要原生资源路由、窗口来源验证与产物数据共同成立；不以自制预览器替代。", reason: "原生窗口链路待验证" },
  { id: "extensions", name: "本地扩展安装", group: "workflow", description: "官方 MCPB / DXT 扩展安装流程。", detail: "当前安装流程依赖组织策略后端。不会移除签名、安装授权或策略检查；已有本地 MCP 与插件仍可使用。", reason: "缺少原生安装策略服务" },
  { id: "dictation", name: "快速输入听写", group: "interaction", description: "官方录音界面、快捷键和转写链路。", detail: "需要官方支持的语音服务配置。不会接入第三方转写、自建识别服务或自动发送录音。", reason: "原生转写服务待配置" },
  { id: "contrast", name: "近黑高对比主题", group: "appearance", description: "缓存中已有的官方主题样式。", detail: "来自不同构建的官方组件，尚未证明能与当前原生设置和样式链兼容；不注入自制 Desktop 主题。", reason: "官方组件移植待验证" },
  { id: "instruction-guide", name: "个性化指令引导", group: "appearance", description: "官方示例卡片与指令设置引导。", detail: "原生指令编辑已可用。新增引导组件的依赖尚未验证，不以自行编写的卡片冒充官方内容。", reason: "官方组件移植待验证" },
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
  return `<div class="view feature-view"><div class="page-head"><div><div class="eyebrow">NATIVE FEATURES</div><h1>原生能力，按需组合</h1><p class="lead">只使用官方界面与执行实现。选择功能后统一备份、校验并应用，不自动授予权限。</p></div><button class="btn btn-secondary" data-action="native-refresh" ${v.busy || v.scanning ? "disabled" : ""}>${v.scanning ? "正在检测…" : "重新检测"}</button></div>
    ${v.error ? `<div class="banner banner-error" role="alert">${esc(v.error)}</div>` : ""}
    ${v.result ? `<div class="banner" role="status">${esc(v.result)}</div>` : ""}
    ${native?.running ? '<div class="banner">Desktop 正在运行。可以选择功能；应用前请先退出，不会自动结束会话。</div>' : ""}
    ${renderDesktopUpdate(v.status?.update ?? null, v.busy || v.scanning)}
    <div class="feature-toolbar"><div class="filter-group" aria-label="功能分类">${([["all", "全部"], ["interaction", "交互与控制"], ["appearance", "语言与个性化"], ["workflow", "工作流"]] as const).map(([id, name]) => `<button class="filter-button ${v.group === id ? "active" : ""}" data-feature-group="${id}" aria-pressed="${v.group === id}">${name}</button>`).join("")}</div><input id="feature-search" class="search-input" type="search" placeholder="搜索功能与说明" aria-label="搜索功能" value="${esc(v.search)}"/></div>
    <div class="feature-grid">${visible.map(f => featureCard(f, v)).join("") || '<div class="empty-note">没有匹配的功能，试试其他关键词。</div>'}</div>
    <div class="apply-panel"><div><strong>${dirty ? "有未应用的更改" : "功能组合"}</strong><p>${chosen.length ? esc(chosen.join(" · ")) : "未选择增强功能"}</p></div><div class="apply-controls">${native && native.requiresSigning !== false ? `<label class="signing-control">签名身份<select id="native-signing-identity" ${v.busy ? "disabled" : ""}><option value="">请选择</option>${native.identities.map(i => `<option value="${esc(i.fingerprint)}" ${v.identity === i.fingerprint ? "selected" : ""}>${esc(i.name)}</option>`).join("")}</select></label>` : ""}<button class="btn btn-secondary" data-action="native-reset" ${!dirty || v.busy ? "disabled" : ""}>撤销选择</button><button class="btn btn-primary" data-action="native-prepare" ${canApply(v) ? "" : "disabled"}>应用更改</button></div></div>
    ${v.confirm ? `<section class="card confirmation-card" role="region" aria-label="确认原生功能变更"><h2>确认${chosen.length ? "应用功能组合" : "还原官方应用"}？</h2><p>目标：<span class="mono">${esc(native?.appPath ?? "")}</span></p><p>从已验证的官方备份生成完整组合，关闭一项不会误删其他选中功能。${native?.requiresSigning ? "应用会重新签名，系统可能要求重新登录或授权。" : ""}会话与 API 配置不作修改。</p>${v.draft.prewarm ? '<p>已选择实验性原生预热，可能增加常驻内存；不会保证每轮模型响应更快。</p>' : ""}<div class="computer-use-actions"><button class="btn btn-primary" data-action="native-confirm" ${canApply(v) ? "" : "disabled"}>确认应用</button><button class="btn btn-secondary" data-action="native-cancel">取消</button></div></section>` : ""}
    <p class="feature-footnote">“已安装”表示补丁校验通过，不代表已经授予设备权限或验证所有使用场景。缺少原生服务的项目不会提供无效开关。</p></div>`;
}
