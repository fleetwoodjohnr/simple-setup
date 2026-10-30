import { create } from "zustand";
import { api, errorMessage } from "./api";
import { emptySelection } from "./selection";
import type { Device, Inventory, Selection } from "./types";

export type Page = "dashboard" | "applications" | "create" | "snapshots" | "restore";

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
  go: (page: Page) => void;
  init: () => Promise<void>;
  scan: () => Promise<void>;
  setInventory: (inv: Inventory) => void;
  setSelection: (f: (s: Selection) => Selection) => void;
  set: (patch: Partial<Pick<AppState, "step" | "name" | "destination" | "restorePath" | "selection" | "encrypt" | "password" | "password2" | "base">>) => void;
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
  go: (page) => set({ page }),
  init: async () => {
    const device = await api.deviceInfo();
    const today = new Date().toISOString().slice(0, 10);
    set({ device, selection: emptySelection([device.platform]), name: `${device.hostname} ${today}` });
    void get().scan();
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
