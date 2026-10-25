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
  if (!status.supported) return `<section class="card"><h2 class="card-title">Desktop 更新维护</h2><p class="card-meta">Windows 不需要：官方更新会整包替换，更新后回到这里重新应用即可。</p></section>`;
  return `<section class="card"><h2 class="card-title">Desktop 更新维护</h2><p class="card-meta">重签过的应用装不了官方更新。开启后，Claude++ 会在 Desktop 退出时先还原官方包，等更新装完再把功能装回去。</p>
    <label class="toggle"><input type="checkbox" data-action="desktop-update-toggle" ${status.enabled ? "checked" : ""} ${busy || status.busy ? "disabled" : ""}/><span>自动维护 Desktop 更新</span></label>
    <p role="status">${esc(status.message)}</p>
    ${status.enabled && status.desiredFeatures.length ? `<p class="card-meta">更新后装回：${status.desiredFeatures.map(esc).join("、")}</p>` : ""}
    ${status.error ? `<div class="banner banner-error" role="alert">${esc(status.error)}</div>` : ""}
    ${status.pendingFeatures.length ? `<p class="card-meta">新版还没适配，暂时留着：${status.pendingFeatures.map(esc).join("、")}</p>` : ""}
    <p class="card-meta">Desktop 开着就排队等，不会强行结束会话。</p>
    ${status.enabled ? `<button class="btn btn-secondary" data-action="desktop-update-retry" ${busy || status.busy ? "disabled" : ""}>重新检测</button>` : ""}
  </section>`;
}
