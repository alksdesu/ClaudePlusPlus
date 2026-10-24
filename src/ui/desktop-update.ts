import { esc } from "./text";

export interface DesktopUpdateStatus {
  supported: boolean;
  enabled: boolean;
  phase: string;
  message: string;
  desiredFeatures: string[];
  pendingFeatures: string[];
  error: string | null;
  busy: boolean;
}

export function renderDesktopUpdate(status: DesktopUpdateStatus | null, busy: boolean): string {
  if (!status) return "";
  if (!status.supported) return `<section class="card"><h2 class="card-title">Desktop 更新维护</h2><p class="card-meta">此流程仅适用于 macOS。Windows 不需要还原签名包，继续使用官方更新流程。</p></section>`;
  return `<section class="card"><h2 class="card-title">Desktop 更新维护</h2><p class="card-meta">仅 macOS 需要：原生增强会重新签名，更新前需还原官方包；Windows 不需要此流程。</p>
    <label class="toggle"><input type="checkbox" data-action="desktop-update-toggle" ${status.enabled ? "checked" : ""} ${busy || status.busy ? "disabled" : ""}/><span>自动维护 Desktop 更新</span></label>
    <p role="status">${esc(status.message)}</p>
    ${status.enabled && status.desiredFeatures.length ? `<p class="card-meta">更新后恢复：${status.desiredFeatures.map(esc).join("、")}</p>` : ""}
    ${status.error ? `<div class="banner banner-error" role="alert">${esc(status.error)}</div>` : ""}
    ${status.pendingFeatures.length ? `<p class="card-meta">等待兼容适配：${status.pendingFeatures.map(esc).join("、")}</p>` : ""}
    <p class="card-meta">Desktop 运行时排队，不强行结束会话；只重装已验证兼容的功能。未知版本保留官方应用与原功能选择。</p>
    ${status.enabled ? `<button class="btn btn-secondary" data-action="desktop-update-retry" ${busy || status.busy ? "disabled" : ""}>重新检测更新维护</button>` : ""}
  </section>`;
}
