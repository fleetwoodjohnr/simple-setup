// Checks every package ID in crates/core/registry/apps.json against the live catalogs, so stale
// entries are found before a restore relies on them. Run by hand (it makes a few hundred
// requests; GitHub allows 60 an hour without a token):
//   GITHUB_TOKEN=$(gh auth token) node scripts/check-registry.mjs [app-id…]
// Distribution packages are checked in Fedora (rawhide), Ubuntu (noble) and Arch's official
// repositories; names from vendors' own repositories (google-chrome-stable…) are reported as
// notes, not failures, because the restore checks availability on the computer itself.
import { readFileSync } from "node:fs";

const reg = JSON.parse(readFileSync(new URL("../crates/core/registry/apps.json", import.meta.url)));
const only = process.argv.slice(2);
const gh = { Accept: "application/vnd.github+json", "User-Agent": "simple-setup-registry-check" };
if (process.env.GITHUB_TOKEN) gh.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;

async function status(url, headers = {}) {
  for (let i = 0; i < 3; i++) {
    try {
      const r = await fetch(url, { headers: { "User-Agent": "simple-setup-registry-check", ...headers } });
      if (r.status === 429 || r.status >= 500) throw new Error(`HTTP ${r.status}`);
      return r;
    } catch (e) {
      if (i === 2) throw e;
      await new Promise((ok) => setTimeout(ok, 1000 * (i + 1)));
    }
  }
}
const ok = async (url, headers) => (await status(url, headers)).ok;
const json = async (url, headers) => {
  const r = await status(url, headers);
  return r.ok ? r.json() : null;
};

const checks = {
  winget: async (id) => {
    const [pub, ...rest] = id.split(".");
    return ok(`https://api.github.com/repos/microsoft/winget-pkgs/contents/manifests/${id[0].toLowerCase()}/${pub}/${rest.join("/")}`, gh);
  },
  brew_cask: (t) => ok(`https://formulae.brew.sh/api/cask/${t}.json`),
  brew: (f) => ok(`https://formulae.brew.sh/api/formula/${f}.json`),
  flatpak: (id) => ok(`https://flathub.org/api/v2/appstream/${id}`),
  snap: (n) => ok(`https://api.snapcraft.io/v2/snaps/info/${n}`, { "Snap-Device-Series": "16" }),
  rpm: (n) => ok(`https://mdapi.fedoraproject.org/rawhide/pkg/${n}`),
  pacman: async (n) => {
    const r = await json(`https://archlinux.org/packages/search/json/?name=${encodeURIComponent(n)}`);
    return (r?.results ?? []).some((p) => p.pkgname === n);
  },
  // Ubuntu (noble) or Debian (any release).
  dpkg: async (n) => {
    const r = await json(
      `https://api.launchpad.net/1.0/ubuntu/+archive/primary?ws.op=getPublishedBinaries&binary_name=${encodeURIComponent(n)}&exact_match=true&distro_arch_series=https://api.launchpad.net/1.0/ubuntu/noble/amd64&status=Published`,
    );
    if ((r?.total_size ?? 0) > 0) return true;
    const d = await json(`https://api.ftp-master.debian.org/madison?package=${encodeURIComponent(n)}&f=json`);
    return Array.isArray(d) && d.some((x) => Object.keys(x).length > 0);
  },
};
// Vendor repositories, RPM Fusion and the AUR: not in the distributions' own catalogs. The
// names still identify the app when it was installed from there.
const thirdParty = new Set(["google-chrome-stable", "brave-browser", "code", "microsoft-edge-stable", "1password", "docker-ce", "signal-desktop", "teamviewer", "spotify-client", "vivaldi-stable", "sublime-text", "opera-stable", "discord", "steam", "telegram-desktop", "rustdesk", "visual-studio-code-bin"]);

let failures = 0, notes = 0, checked = 0;
for (const e of reg.apps) {
  if (only.length && !only.includes(e.id)) continue;
  const lines = [];
  for (const [method, ids] of Object.entries(e.packages)) {
    const check = checks[method];
    if (!check) continue;
    for (const id of ids) {
      checked++;
      let found;
      try {
        found = await check(id);
      } catch (err) {
        lines.push(`  ? ${method} ${id}: ${err.message}`);
        notes++;
        continue;
      }
      if (found) continue;
      if (["rpm", "dpkg", "pacman"].includes(method) && thirdParty.has(id)) {
        lines.push(`  note: ${method} ${id} comes from the vendor's own repository`);
        notes++;
      } else {
        lines.push(`  FAIL: ${method} ${id} not found`);
        failures++;
      }
    }
  }
  if (e.github) {
    checked++;
    const r = await json(`https://api.github.com/repos/${e.github}`, gh);
    if (!r) {
      lines.push(`  FAIL: github ${e.github} not found`);
      failures++;
    } else if (r.archived) {
      lines.push(`  note: github ${e.github} is archived`);
      notes++;
    }
  }
  if (lines.length) console.log(`${e.id} (${e.name})\n${lines.join("\n")}`);
}
console.log(`${checked} IDs checked: ${failures} not found, ${notes} note(s).`);
process.exit(failures ? 1 : 0);
