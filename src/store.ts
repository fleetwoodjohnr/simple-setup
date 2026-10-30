import { create } from "zustand";
import { api, errorMessage } from "./api";
import { emptySelection } from "./selection";
import { applyTheme } from "./theme";
import type { AppInfo, Device, Inventory, Selection, Settings, UpdateInfo } from "./types";

export type Page = "dashboard" | "applications" | "create" | "snapshots" | "restore" | "settings";

const DAY = 24 * 60 * 60 * 1000;

interface AppState {
  page: Page;
  device: Device | null;
  inventory: Inventory | null;
  scanning: boolean;
  scanError: string | null;
  /** Snapshot Builder draft; survives switching pages. */
  selection: Selection;
  step: number;
  name: string;
  destination: string | null;
  /** Snapshot folder the Restore page should open. */
  restorePath: string | null;
  /** Encrypt the snapshot being built. The password lives only in memory. */
  encrypt: boolean;
  password: string;
  password2: string;
  /** Earlier snapshot this build updates (unchanged files are taken from it). */
  base: { path: string; name: string } | null;
  settings: Settings | null;
  appInfo: AppInfo | null;
  /** Latest update check; `updateReady` once a newer version is downloaded and verified. */
  update: UpdateInfo | null;
  updateReady: boolean;
  go: (page: Page) => void;
  /** Saves a change to the settings (merged into the current ones). */
  saveSettings: (patch: Partial<Settings>) => Promise<void>;
  checkUpdate: () => Promise<UpdateInfo>;
  init: () => Promise<void>;
  scan: () => Promise<void>;
  setInventory: (inv: Inventory) => void;
  setSelection: (f: (s: Selection) => Selection) => void;
  set: (patch: Partial<Pick<AppState, "step" | "name" | "destination" | "restorePath" | "selection" | "encrypt" | "password" | "password2" | "base" | "update" | "updateReady">>) => void;
}

export const useApp = create<AppState>((set, get) => ({
  page: "dashboard",
  device: null,
  inventory: null,
  scanning: false,
  scanError: null,
  selection: emptySelection(),
  step: 1,
  name: "",
  destination: null,
  restorePath: null,
  encrypt: false,
  password: "",
  password2: "",
  base: null,
  settings: null,
  appInfo: null,
  update: null,
  updateReady: false,
  go: (page) => set({ page }),
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
  init: async () => {
    try {
      const [device, settings, appInfo] = await Promise.all([api.deviceInfo(), api.getSettings(), api.appInfo()]);
      const today = new Date().toISOString().slice(0, 10);
      set({ device, settings, appInfo, selection: emptySelection([device.platform]), name: `${device.hostname} ${today}`, destination: settings.default_destination });
      applyTheme(settings.theme);
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
