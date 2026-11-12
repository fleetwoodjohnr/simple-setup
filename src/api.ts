import { Channel, invoke } from "@tauri-apps/api/core";
import type * as T from "./types";

/** The snapshot is encrypted and needs its password or recovery key. */
export const isLocked = (e: unknown) => typeof e === "object" && e !== null && (e as T.CmdError).code === "locked";

export function errorMessage(e: unknown): string {
  if (typeof e === "object" && e !== null && "message" in e) return String((e as T.CmdError).message);
  return String(e);
}

const call = <R>(cmd: string, args?: Record<string, unknown>) => invoke<R>(cmd, args);

function channel(onProgress: (p: T.Progress) => void) {
  const ch = new Channel<T.Progress>();
  ch.onmessage = onProgress;
  return ch;
}

export const api = {
  deviceInfo: () => call<T.Device>("device_info"),
  scan: () => call<T.Inventory>("scan"),
  getSettings: () => call<T.Settings>("get_settings"),
  saveSettings: (settings: T.Settings) => call<void>("save_settings", { settings }),
  saveAppEdit: (id: string, edit: T.AppEdit) => call<T.Inventory>("save_app_edit", { id, edit }),
  saveCustomApp: (app: T.App) => call<T.Inventory>("save_custom_app", { app }),
  deleteCustomApp: (id: string) => call<T.Inventory>("delete_custom_app", { id }),
  measure: (path: string, exclude: string[] = []) => call<T.Measure>("measure", { path, exclude }),
  listDir: (path: string) => call<T.DirItem[]>("list_dir", { path }),
  knownFolders: () => call<T.KnownFolderInfo[]>("known_folders"),
  volumes: () => call<T.Volume[]>("volumes"),
  engines: () => call<T.Platform[]>("engines_available"),
  /** `remote`: the computer probed over the network, planned for instead of an assumed one. */
  preflight: (selection: T.Selection, destination: string | null, remote: T.SystemInfo | null = null) =>
    call<T.Preflight>("preflight", { selection, destination, remote }),
  buildSnapshot: (selection: T.Selection, name: string, destination: string, password: string | null, base: string | null, remote: T.SystemInfo | null, onProgress: (p: T.Progress) => void) =>
    call<T.BuildReport>("build_snapshot", { selection, name, destination, password, base, remote, onProgress: channel(onProgress) }),
  cancelBuild: () => call<void>("cancel_build"),
  discardPartial: (destination: string, name: string) => call<void>("discard_partial", { destination, name }),
  listProfiles: () => call<T.Profile[]>("list_profiles"),
  saveProfile: (name: string, selection: T.Selection) => call<T.Profile>("save_profile", { name, selection }),
  deleteProfile: (name: string) => call<void>("delete_profile", { name }),
  listSnapshots: () => call<T.SnapshotItem[]>("list_snapshots"),
  openSnapshot: (path: string) => call<T.Summary>("open_snapshot", { path }),
  verifySnapshot: (path: string, onProgress: (p: T.Progress) => void) =>
    call<T.VerifyReport>("verify_snapshot", { path, onProgress: channel(onProgress) }),
  deleteSnapshot: (path: string, confirmName: string) => call<void>("delete_snapshot", { path, confirmName }),
  restorePlan: (path: string, secret: string | null) => call<T.RestorePlan>("restore_plan", { path, secret }),
  restoreExecute: (path: string, choice: { items: string[]; conflict: T.Conflict }, secret: string | null, onProgress: (p: T.Progress) => void) =>
    call<T.RestoreReport>("restore_execute", { path, choice, secret, onProgress: channel(onProgress) }),
  cancelRestore: () => call<void>("cancel_restore"),
  unlockSummary: (path: string, secret: string) => call<T.Summary>("unlock_summary", { path, secret }),
  findReceivers: () => call<T.Peer[]>("find_receivers"),
  sendSnapshot: (path: string, address: string, code: string, onProgress: (p: T.Progress) => void) =>
    call<T.SendReport>("send_snapshot", { path, address, code, onProgress: channel(onProgress) }),
  receiveSnapshot: (destination: string, onEvent: (e: T.ReceiveEvent) => void) => {
    const ch = new Channel<T.ReceiveEvent>();
    ch.onmessage = onEvent;
    return call<string>("receive_snapshot", { destination, onEvent: ch });
  },
  answerOffer: (accept: boolean) => call<void>("answer_offer", { accept }),
  cancelTransfer: () => call<void>("cancel_transfer"),
  probeReceiver: (address: string, code: string) => call<T.Probed>("probe_receiver", { address, code }),
  outgoingFolder: () => call<string>("outgoing_folder"),
  listTransfers: () => call<T.TransferRecord[]>("list_transfers"),
  exportReport: (file: string, to: string) => call<void>("export_report", { file, to }),
  listDistros: () => call<T.DistroList>("list_distros"),
  distroDetails: (id: string) => call<[T.DistroProfile | null, T.Resolved]>("distro_details", { id }),
  readDistroFile: (path: string) => call<T.ProfileFile>("read_distro_file", { path }),
  testDistros: (file: T.ProfileFile) => call<T.TestReport>("test_distros", { file }),
  saveDistros: (file: T.ProfileFile) => call<string[]>("save_distros", { file }),
  exportDistros: (ids: string[], to: string) => call<void>("export_distros", { ids, to }),
  setDistroEnabled: (id: string, enabled: boolean) => call<void>("set_distro_enabled", { id, enabled }),
  deleteDistro: (id: string) => call<void>("delete_distro", { id }),
  trustDistro: (id: string, trust: boolean) => call<void>("trust_distro", { id, trust }),
  updateDistros: () => call<{ updated: boolean; version: string; message: string }>("update_distros"),
  appInfo: () => call<T.AppInfo>("app_info"),
  systemInfo: () => call<T.SystemInfo>("system_info"),
  recentRestores: () => call<T.ReportSummary[]>("recent_restores"),
  checkUpdate: () => call<T.UpdateInfo>("check_update"),
  downloadUpdate: (onProgress: (p: T.UpdateProgress) => void) => {
    const ch = new Channel<T.UpdateProgress>();
    ch.onmessage = onProgress;
    return call<void>("download_update", { onProgress: ch });
  },
  cancelUpdate: () => call<void>("cancel_update"),
  installUpdate: () => call<void>("install_update"),
};
