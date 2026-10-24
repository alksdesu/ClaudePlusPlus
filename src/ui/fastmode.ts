import type { FastModeStatus, RendererState } from "../state";

const rendererStates: Record<RendererState, { label: string; cls: string }> = {
  patched: { label: "已 patch", cls: "pill-registered" },
  outdated: { label: "补丁需更新", cls: "pill-pending" },
  pristine: { label: "官方原样", cls: "pill-pending" },
  mismatch: { label: "锚点失配", cls: "pill-tombstone" },
  notFound: { label: "未找到", cls: "pill-foreign" },
  unreadable: { label: "不可读", cls: "pill-foreign" },
  unsupported: { label: "当前未适配", cls: "pill-foreign" },
};

export function rendererPresentation(renderer: FastModeStatus["renderer"]): { label: string; cls: string } {
  const state = rendererStates[renderer.state];
  if (!renderer.requiresSigning) return state;
  if (renderer.state === "patched") return { ...state, label: "原生开关已安装" };
  if (renderer.state === "pristine") return { ...state, label: "可安装 · 需重签" };
  return state;
}
