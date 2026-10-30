//! App-specific settings converters for restoring on another operating system. Each adapter
//! knows where its app keeps settings on every system, which files carry over as they are,
//! which need rewriting, and which belong to the old machine; those stay in the snapshot and
//! are reported. Destinations come from this table, never from the snapshot.

use crate::platform::OsFamily;

pub struct Adapter {
    /// Canonical app ID in the registry.
    pub app: &'static str,
    pub name: &'static str,
    /// Settings folder on Linux: the native one first, then sandboxed (Flatpak, Snap) copies.
    linux: &'static [&'static str],
    windows: &'static str,
    macos: &'static str,
    rule: fn(&str) -> Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Copy,
    /// Rewritten for the new system (see [`convert`]).
    Convert,
    /// Stays in the snapshot, for this reason.
    Keep(&'static str),
}

const MACHINE: Rule = Rule::Keep("Tied to the old computer.");

fn vscode(l: &str) -> Rule {
    let p: Vec<&str> = l.split('/').collect();
    // Profiles keep the same files under profiles/<name>/.
    let p = if p.len() > 2 && p[0] == "profiles" {
        &p[2..]
    } else {
        &p[..]
    };
    match p {
        ["settings.json" | "tasks.json" | "mcp.json"] | ["snippets", ..] => Rule::Copy,
        ["keybindings.json"] => Rule::Keep("Keyboard shortcuts use different keys on each system."),
        _ => MACHINE,
    }
}

fn git(l: &str) -> Rule {
    match l {
        ".gitconfig" | ".config/git/config" => Rule::Convert,
        ".config/git/ignore" | ".config/git/attributes" => Rule::Copy,
        _ => MACHINE,
    }
}

fn mozilla(l: &str) -> Rule {
    let last = l.rsplit('/').next().unwrap_or(l);
    match () {
        _ if l == "profiles.ini" => Rule::Convert,
        _ if l == "installs.ini" => Rule::Keep("Links profiles to the old installation."),
        _ if last == "compatibility.ini" => {
            Rule::Keep("Describes the old installation; it is rebuilt on first start.")
        }
        _ if last == "pkcs11.txt" => {
            Rule::Keep("Lists security modules by the old system's file paths.")
        }
        _ if l.starts_with("Crash Reports/") || l.starts_with("Pending Pings/") => {
            Rule::Keep("Crash reports and pending telemetry.")
        }
        _ => Rule::Copy,
    }
}

fn gh(l: &str) -> Rule {
    match l {
        "config.yml" => Rule::Copy,
        "hosts.yml" => Rule::Keep("Holds your GitHub sign-in; sign in again with gh auth login."),
        _ => MACHINE,
    }
}

fn copy_all(_: &str) -> Rule {
    Rule::Copy
}

pub const ADAPTERS: &[Adapter] = &[
    Adapter {
        app: "vscode",
        name: "Visual Studio Code settings",
        linux: &[
            "~/.config/Code/User",
            "~/.var/app/com.visualstudio.code/config/Code/User",
        ],
        windows: "~/AppData/Roaming/Code/User",
        macos: "~/Library/Application Support/Code/User",
        rule: vscode,
    },
    Adapter {
        app: "git",
        name: "Git settings",
        linux: &["~"],
        windows: "~",
        macos: "~",
        rule: git,
    },
    Adapter {
        app: "firefox",
        name: "Firefox profiles",
        linux: &[
            "~/.mozilla/firefox",
            "~/.config/mozilla/firefox",
            "~/.var/app/org.mozilla.firefox/.mozilla/firefox",
            "~/snap/firefox/common/.mozilla/firefox",
        ],
        windows: "~/AppData/Roaming/Mozilla/Firefox",
        macos: "~/Library/Application Support/Firefox",
        rule: mozilla,
    },
    Adapter {
        app: "thunderbird",
        name: "Thunderbird profiles",
        linux: &[
            "~/.thunderbird",
            "~/.var/app/org.mozilla.Thunderbird/.thunderbird",
            "~/.var/app/net.thunderbird.Thunderbird/.thunderbird",
        ],
        windows: "~/AppData/Roaming/Thunderbird",
        macos: "~/Library/Thunderbird",
        rule: mozilla,
    },
    Adapter {
        app: "github-cli",
        name: "GitHub CLI settings",
        linux: &["~/.config/gh"],
        windows: "~/AppData/Roaming/GitHub CLI",
        macos: "~/.config/gh",
        rule: gh,
    },
    Adapter {
        app: "neovim",
        name: "Neovim configuration",
        linux: &["~/.config/nvim", "~/.var/app/io.neovim.nvim/config/nvim"],
        windows: "~/AppData/Local/nvim",
        macos: "~/.config/nvim",
        rule: copy_all,
    },
];

pub fn for_app(canonical: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.app == canonical)
}

/// `path` without the `dir/` prefix; Windows and macOS compare names without case.
fn under(path: &str, dir: &str, os: OsFamily) -> Option<String> {
    if dir == "~" {
        return path.strip_prefix("~/").map(String::from);
    }
    let n = dir.len();
    let fold = os != OsFamily::Linux;
    let head = path.get(..n)?;
    let same = if fold {
        head.eq_ignore_ascii_case(dir)
    } else {
        head == dir
    };
    (same && path[n..].starts_with('/')).then(|| path[n + 1..].to_string())
}

impl Adapter {
    fn folders(&self, os: OsFamily) -> Vec<&'static str> {
        match os {
            OsFamily::Linux => self.linux.to_vec(),
            OsFamily::Windows => vec![self.windows],
            OsFamily::Macos => vec![self.macos],
        }
    }

    /// Where the app's settings go on `os` (`~/…`).
    pub fn folder(&self, os: OsFamily) -> &'static str {
        self.folders(os)[0]
    }

    /// The path of a file (`~/…`, as recorded on a computer running `os`) inside the app's
    /// settings folder, if it is in one.
    pub fn logical(&self, os: OsFamily, path: &str) -> Option<String> {
        self.folders(os)
            .into_iter()
            .find_map(|d| under(path, d, os))
            .filter(|l| !l.is_empty())
    }

    /// Whether a recorded location (`~/…`) holds or is inside the app's settings folder.
    pub fn covers(&self, os: OsFamily, location: &str) -> bool {
        self.folders(os).into_iter().any(|d| {
            location == d || under(location, d, os).is_some() || under(d, location, os).is_some()
        })
    }

    pub fn rule(&self, logical: &str) -> Rule {
        (self.rule)(logical)
    }
}

fn is_abs_path(v: &str, os: OsFamily) -> bool {
    let v = v.trim().trim_matches('"');
    match os {
        OsFamily::Windows => {
            let b = v.as_bytes();
            v.starts_with("\\\\")
                || b.len() > 2
                    && b[0].is_ascii_alphabetic()
                    && b[1] == b':'
                    && matches!(b[2], b'\\' | b'/')
        }
        _ => v.starts_with('/'),
    }
}

/// `.gitconfig`: credential helpers differ per system, line-ending conversion is a Windows
/// habit, and absolute paths name the old system's programs. Those lines are kept as comments.
fn gitconfig(text: &str, from: OsFamily, to: OsFamily) -> String {
    let mut section = String::new();
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            section = t
                .trim_start_matches('[')
                .split([']', ' ', '"', '.'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
        }
        let disable = t.split_once('=').is_some_and(|(k, v)| {
            let k = k.trim().to_ascii_lowercase();
            section == "credential" && k == "helper"
                || section == "core"
                    && k == "autocrlf"
                    && (from == OsFamily::Windows) != (to == OsFamily::Windows)
                || is_abs_path(v, from)
        });
        if disable && !t.starts_with(['#', ';']) {
            out.push_str(&format!("# [Simple Setup] not valid on {to}: {t}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// `profiles.ini`: `[Install…]` sections bind profiles to the old installation's path; without
/// them the new installation adopts the default profile. Relative profile paths work as they are.
fn profiles_ini(text: &str) -> String {
    let mut skip = false;
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            skip = t.starts_with("[Install");
        }
        if !skip {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Rewrites a [`Rule::Convert`] file for `to`. Files that are not UTF-8 text are left as they are.
pub fn convert(logical: &str, bytes: &[u8], from: OsFamily, to: OsFamily) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let last = logical.rsplit('/').next().unwrap_or(logical);
    match last {
        ".gitconfig" | "config" => gitconfig(text, from, to).into_bytes(),
        "profiles.ini" => profiles_ini(text).into_bytes(),
        _ => bytes.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use OsFamily::*;

    #[test]
    fn maps_settings_between_systems() {
        let code = for_app("vscode").unwrap();
        assert_eq!(
            code.logical(Linux, "~/.config/Code/User/settings.json")
                .as_deref(),
            Some("settings.json")
        );
        assert_eq!(
            code.logical(Windows, "~/appdata/roaming/code/user/snippets/rs.json")
                .as_deref(),
            Some("snippets/rs.json")
        );
        assert_eq!(
            code.logical(Linux, "~/.config/code/User/settings.json"),
            None,
            "Linux names are case-sensitive"
        );
        assert_eq!(code.rule("settings.json"), Rule::Copy);
        assert_eq!(code.rule("profiles/Work/snippets/a.json"), Rule::Copy);
        assert!(matches!(code.rule("keybindings.json"), Rule::Keep(_)));
        assert!(matches!(
            code.rule("globalStorage/state.vscdb"),
            Rule::Keep(_)
        ));
        assert_eq!(
            code.folder(Macos),
            "~/Library/Application Support/Code/User"
        );

        // A Flatpak root above the profile folder still maps file by file.
        let ff = for_app("firefox").unwrap();
        assert!(ff.covers(Linux, "~/.var/app/org.mozilla.firefox/.mozilla"));
        assert!(!ff.covers(Linux, "~/.var/app/org.mozilla.firefox/cache"));
        assert_eq!(
            ff.logical(
                Linux,
                "~/.var/app/org.mozilla.firefox/.mozilla/firefox/abc.default/prefs.js"
            )
            .as_deref(),
            Some("abc.default/prefs.js")
        );
        assert_eq!(
            ff.logical(
                Linux,
                "~/.var/app/org.mozilla.firefox/.mozilla/extensions/x.xpi"
            ),
            None
        );
        assert!(matches!(
            ff.rule("abc.default/compatibility.ini"),
            Rule::Keep(_)
        ));
        assert_eq!(ff.rule("abc.default/places.sqlite"), Rule::Copy);

        let git = for_app("git").unwrap();
        assert_eq!(
            git.logical(Windows, "~/.gitconfig").as_deref(),
            Some(".gitconfig")
        );
        assert_eq!(git.rule(".gitconfig"), Rule::Convert);
    }

    #[test]
    fn rewrites_machine_specific_lines() {
        let cfg = "[user]\n\tname = Ada\n[credential]\n\thelper = /usr/libexec/git-core/git-credential-libsecret\n[core]\n\teditor = /usr/bin/vim\n\tpager = less\n[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n";
        let out = String::from_utf8(convert(".gitconfig", cfg.as_bytes(), Linux, Windows)).unwrap();
        assert!(out.contains("\tname = Ada\n") && out.contains("\tpager = less\n"));
        assert!(out.contains("# [Simple Setup] not valid on windows: editor = /usr/bin/vim"));
        assert_eq!(out.matches("# [Simple Setup]").count(), 3, "{out}");

        let win = "[core]\n\tautocrlf = true\n\teditor = C:\\\\Tools\\\\vim.exe\n";
        let out = String::from_utf8(convert(".gitconfig", win.as_bytes(), Windows, Macos)).unwrap();
        assert_eq!(out.matches("# [Simple Setup]").count(), 2, "{out}");

        let ini = "[Install4F96D1932A9F858E]\nDefault=Profiles/abc.default\nLocked=1\n\n[Profile0]\nName=default\nIsRelative=1\nPath=abc.default\nDefault=1\n\n[General]\nStartWithLastProfile=1\n";
        let out =
            String::from_utf8(convert("profiles.ini", ini.as_bytes(), Linux, Windows)).unwrap();
        assert!(!out.contains("Install") && !out.contains("Locked"));
        assert!(out.contains("Path=abc.default\n") && out.contains("[General]"));
    }
}
