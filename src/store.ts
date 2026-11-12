import { create } from "zustand";
import { api, errorMessage } from "./api";
import { emptySelection } from "./selection";
import { applyTheme } from "./theme";
import { setDistroNames } from "./ui";
import type { AppInfo, Device, Inventory, Listed, Probed, Selection, Settings, UpdateInfo } from "./types";

/** The five places in the navigation, and the two flows opened from them. */
export type Page = "dashboard" | "snapshots" | "transfers" | "applications" | "settings" | "create" | "restore";
/** Snapshot: save it for later or another computer. Transfer: send it straight to a paired one. */
export type Mode = "snapshot" | "transfer";
/** Where a snapshot is going: not known yet, a computer like this one, one the user describes, or one paired on the network. */
export type DestChoice = "decide" | "same" | "choose" | "connect";

const DAY = 24 * 60 * 60 * 1000;

interface AppState {
  page: Page;
  device: Device | null;
  inventory: Inventory | null;
  scanning: boolean;
  scanError: string | null;
  /** Wizard draft; survives switching pages. */
  mode: Mode;
  destChoice: DestChoice;
  /** The paired computer (Transfer flow): its address, code and what it said it is. */
  peer: { address: string; code: string; probed: Probed } | null;
  selection: Selection;
  step: number;
  name: string;
  destination: string | null;
  /** Snapshot folder the Restore page should open, and whether to start restoring it at once
   * (chosen when receiving it). */
  restorePath: string | null;
  restoreAuto: boolean;
  /** Encrypt the snapshot being built. The password lives only in memory. */
  encrypt: boolean;
  password: string;
  password2: string;
  /** Earlier snapshot this build updates (unchanged files are taken from it). */
  base: { path: string; name: string } | null;
  settings: Settings | null;
  appInfo: AppInfo | null;
  /** Distribution profiles, for names and pickers. */
  distros: Listed[];
  /** Latest update check; `updateReady` once a newer version is downloaded and verified. */
  update: UpdateInfo | null;
  updateReady: boolean;
  go: (page: Page) => void;
  /** Opens the wizard fresh for a snapshot or a transfer, keeping nothing from a finished one. */
  startFlow: (mode: Mode) => void;
  /** Saves a change to the settings (merged into the current ones). */
  saveSettings: (patch: Partial<Settings>) => Promise<void>;
  checkUpdate: () => Promise<UpdateInfo>;
  init: () => Promise<void>;
  scan: () => Promise<void>;
  loadDistros: () => Promise<void>;
  setInventory: (inv: Inventory) => void;
  setSelection: (f: (s: Selection) => Selection) => void;
  set: (patch: Partial<Pick<AppState, "step" | "name" | "destination" | "restorePath" | "restoreAuto" | "selection" | "encrypt" | "password" | "password2" | "base" | "update" | "updateReady" | "mode" | "destChoice" | "peer">>) => void;
}

export const useApp = create<AppState>((set, get) => ({
  page: "dashboard",
  device: null,
  inventory: null,
  scanning: false,
  scanError: null,
  mode: "snapshot",
  destChoice: "same",
  peer: null,
  selection: emptySelection(),
  step: 1,
  name: "",
  destination: null,
  restorePath: null,
  restoreAuto: false,
  encrypt: false,
  password: "",
  password2: "",
  base: null,
  settings: null,
  appInfo: null,
  distros: [],
  update: null,
  updateReady: false,
  go: (page) => set({ page }),
  startFlow: (mode) => {
    const { device, settings, selection, mode: was } = get();
    const today = new Date().toISOString().slice(0, 10);
    // Choices in progress for the same flow are kept; a different flow starts at its first step.
    if (was !== mode) {
      set({
        mode, step: 1, peer: null, base: null, encrypt: false, password: "", password2: "",
        destChoice: mode === "transfer" ? "connect" : "same",
        selection: { ...emptySelection(device ? [device.platform] : []), apps: selection.apps, folders: selection.folders, include_secrets: selection.include_secrets },
        name: `${device?.hostname ?? "Snapshot"} ${today}`, destination: settings?.default_destination ?? null,
      });
    }
    set({ page: "create" });
  },
  saveSettings: async (patch) => {
    const current = get().settings ?? (await api.getSettings());
    const next = { ...current, ...patch };
    await api.saveSettings(next);
    set({ settings: next });
    if (patch.theme) applyTheme(patch.theme);
  },
  checkUpdate: async () => {
    const update = await api.checkUpdate();
    set({ update, settings: { ...(get().settings ?? (await api.getSettings())), last_update_check: new Date().toISOString() } });
    return update;
  },
  loadDistros: async () => {
    const d = await api.listDistros();
    setDistroNames(d.profiles);
    set({ distros: d.profiles });
  },
  init: async () => {
    try {
      const [device, settings, appInfo] = await Promise.all([api.deviceInfo(), api.getSettings(), api.appInfo()]);
      const today = new Date().toISOString().slice(0, 10);
      set({ device, settings, appInfo, selection: emptySelection([device.platform]), name: `${device.hostname} ${today}`, destination: settings.default_destination });
      applyTheme(settings.theme);
      void get().loadDistros().catch(() => {});
      void get().scan();
      // Development builds (and the automated tests) never reach out by themselves.
      const due = !settings.last_update_check || Date.now() - Date.parse(settings.last_update_check) > DAY;
      if (!appInfo.development && settings.update_check && due) {
        const u = await get().checkUpdate();
        if (u.newer && u.file && settings.auto_update) {
          await api.downloadUpdate(() => {});
          set({ updateReady: true });
        }
      }
    } catch (e) {
      set({ scanError: errorMessage(e) });
    }
  },
  scan: async () => {
    if (get().scanning) return;
    set({ scanning: true, scanError: null });
    try {
      set({ inventory: await api.scan() });
    } catch (e) {
      set({ scanError: errorMessage(e) });
    } finally {
      set({ scanning: false });
    }
  },
  setInventory: (inventory) => set({ inventory }),
  setSelection: (f) => set({ selection: f(get().selection) }),
  set: (patch) => set(patch),
}));
