import type { FeatureGroup, NativeSelection, NativeStatus } from "./ui/native-features";
import { emptySelection } from "./ui/native-features";

export type PoolKind = "code" | "agent";

export interface PoolCombo {
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

export interface CliSession {
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

export interface DiscoveryReport {
  roots: { label: string; path: string; antDid: string | null }[];
  combos: PoolCombo[];
  cliSessions: CliSession[];
}

export interface UnifyPlan {
  pool: PoolKind;
  canonical: PoolCombo;
  toMerge: PoolCombo[];
  alreadyUnified: PoolCombo[];
  foreignJunctions: PoolCombo[];
}

export interface DesktopSession {
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

export interface RunningDesktop {
  rootLabel: string;
  running: boolean;
}

export interface RegisterReport {
  sessionId: string;
  outcome: { status: string; metadataFile?: string; reason?: string } | null;
  error: string | null;
}

export interface UnregisterReport {
  metadataFile: string;
  outcome: { tombstone: string | null; removed: string } | null;
  error: string | null;
}

export interface DeleteReport {
  target: string;
  outcome: {
    cliSessionId: string | null;
    metadataRemoved: string | null;
    tombstoneRemoved: boolean;
    transcriptRemoved: boolean;
  } | null;
  error: string | null;
}

export type WrapperState = "deployed" | "absent" | "broken" | "noCli";
export type RendererState = "patched" | "outdated" | "pristine" | "mismatch" | "notFound" | "unreadable" | "unsupported";

export interface FastModeSettings {
  auto: boolean;
  installed: boolean;
  lastRepair: string | null;
  patchedDesktopVersion: string | null;
  patchedCliVersion: string | null;
  lastFailure: string | null;
  failedFor: string | null;
}

export interface FastModeStatus {
  supported: boolean;
  desktopVersion: string | null;
  msixPath: string | null;
  cliVersion: string | null;
  cliDir: string | null;
  wrapper: WrapperState;
  renderer: { requiresSigning: boolean; file: string | null; state: RendererState; missingAnchors: string[] };
  settings: FastModeSettings;
  speed: { sessions: number; fast: number; standard: number };
}

export interface FastModeReport {
  wrapper: string;
  renderer: string;
}

export interface ComputerUseStatus {
  supported: boolean;
  requiresSigning?: boolean;
  appPath: string;
  desktopVersion: string | null;
  state: string;
  message: string;
  identities: { fingerprint: string; name: string }[];
  backupPath: string | null;
  running: boolean;
  canInstall: boolean;
  canRestore: boolean;
}

export interface WatchStatus {
  running: boolean;
  paused: boolean;
  pendingUnify: boolean;
  pendingFastmode: boolean;
  lastEvent: string | null;
}

export interface TombstoneInfo {
  fileName: string;
  cliSessionId: string;
  deletedAt: number | null;
  jsonlPath: string | null;
  jsonlSize: number;
}

export type CodexStatus = "migrated" | "mirror" | "orphanMirror" | "nativeOnly";

export interface CodexSession {
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

export interface CodexMigrateReport {
  threadId: string;
  outcome: { status: string; claudePath?: string; turns?: number; reason?: string } | null;
  error: string | null;
}

export type CodexFilter = "all" | "nativeOnly" | "migrated" | "mirror";

export interface PreviewMessage {
  role: string;
  text: string;
  timestamp: string | null;
}

export interface SessionPreview {
  title: string | null;
  cwd: string | null;
  totalMessages: number;
  truncated: boolean;
  messages: PreviewMessage[];
}

export interface PurgeOutcome {
  fileName: string;
  markerRemoved: boolean;
  transcriptRemoved: boolean;
  error: string | null;
}

export interface AppState {
  tab: "overview" | "features" | "unify" | "migrate" | "codex" | "fastmode" | "computer-use" | "appearance" | "backups" | "diagnostics";
  native: NativeStatus | null;
  nativeDraft: NativeSelection;
  nativeDirty: boolean;
  nativeScanning: boolean;
  nativeError: string | null;
  nativeActionError: string | null;
  nativeResult: string | null;
  nativeConfirm: boolean;
  featureGroup: FeatureGroup;
  featureSearch: string;
  scanError: string | null;
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
  fastmode: FastModeStatus | null;
  computerUse: ComputerUseStatus | null;
  computerUseError: string | null;
  computerUseScanning: boolean;
  signingIdentity: string;
  computerUseConfirm: "install" | "restore" | null;
  computerUseResult: string | null;
}

export const state: AppState = {
  tab: "overview",
  native: null,
  nativeDraft: emptySelection(),
  nativeDirty: false,
  nativeScanning: false,
  nativeError: null,
  nativeActionError: null,
  nativeResult: null,
  nativeConfirm: false,
  featureGroup: "all",
  featureSearch: "",
  scanError: null,
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
  fastmode: null,
  computerUse: null,
  computerUseError: null,
  computerUseScanning: false,
  signingIdentity: "",
  computerUseConfirm: null,
  computerUseResult: null,
};
