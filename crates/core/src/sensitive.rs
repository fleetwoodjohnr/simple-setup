//! Path rules: secrets that are never copied unless the user explicitly opts in, and
//! transient files (caches, locks) that are never worth copying. Matching is by path only;
//! file contents are never read for classification.

use std::path::Path;
use std::sync::LazyLock;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

const SECRETS: &[(&str, &str)] = &[
    ("**/.ssh/**", "SSH keys and configuration"),
    ("**/.gnupg/**", "GnuPG keys"),
    ("**/.password-store/**", "Password store"),
    ("**/.local/share/keyrings/**", "Desktop keyring"),
    ("**/.pki/**", "Certificate store"),
    ("**/*.kdbx", "Password database"),
    ("**/*.keychain*", "Keychain"),
    ("**/logins.json", "Saved browser passwords"),
    ("**/logins-backup.json", "Saved browser passwords"),
    ("**/key[34].db", "Browser password key store"),
    ("**/Login Data*", "Saved browser passwords"),
    ("**/Cookies", "Browser cookies"),
    ("**/Cookies-journal", "Browser cookies"),
    ("**/cookies.sqlite*", "Browser cookies"),
    ("**/Web Data*", "Browser autofill data"),
    ("**/.netrc", "Network credentials"),
    ("**/.git-credentials", "Git credentials"),
    ("**/.aws/credentials", "Cloud credentials"),
    ("**/.azure/**", "Cloud credentials"),
    ("**/.config/gcloud/**", "Cloud credentials"),
    ("**/.kube/config", "Kubernetes credentials"),
    ("**/.docker/config.json", "Container registry credentials"),
    ("**/.config/gh/hosts.yml", "GitHub CLI token"),
    ("**/.npmrc", "Package registry token"),
    ("**/.pypirc", "Package registry token"),
    ("**/.cargo/credentials*", "Package registry token"),
    ("**/*.pem", "Private key or certificate"),
    ("**/*.key", "Private key"),
    ("**/*.p12", "Private key bundle"),
    ("**/*.pfx", "Private key bundle"),
    ("**/*recovery*code*", "Recovery codes"),
    ("**/.env", "Environment secrets"),
    ("**/.env.*", "Environment secrets"),
    ("**/wallet.dat", "Cryptocurrency wallet"),
    ("**/.electrum/**", "Cryptocurrency wallet"),
    ("**/Library/Keychains/**", "Keychain"),
    ("**/Library/Cookies/**", "Browser cookies"),
    (
        "**/AppData/*/Microsoft/Credentials/**",
        "Windows saved credentials",
    ),
    (
        "**/AppData/Local/Microsoft/Vault/**",
        "Windows saved credentials",
    ),
    (
        "**/AppData/Roaming/Microsoft/Protect/**",
        "Windows data-protection keys",
    ),
    (
        "**/AppData/Roaming/Microsoft/Crypto/**",
        "Windows private keys",
    ),
    (
        "**/AppData/Roaming/Microsoft/SystemCertificates/**",
        "Certificate store",
    ),
];

const TRANSIENT: &[(&str, &str)] = &[
    ("**/.cache/**", "Cache"),
    ("**/Cache/**", "Cache"),
    ("**/Code Cache/**", "Cache"),
    ("**/GPUCache/**", "Cache"),
    ("**/CachedData/**", "Cache"),
    ("**/cache2/**", "Cache"),
    ("**/.var/app/*/cache/**", "Cache"),
    ("**/Singleton*", "Running-app lock"),
    ("**/.parentlock", "Running-app lock"),
    ("**/lock", "Running-app lock"),
    ("**/.Trash*/**", "Trash"),
    ("**/node_modules/**", "Rebuildable dependency folder"),
    ("**/Library/Caches/**", "Cache"),
    ("**/AppData/Local/Temp/**", "Temporary files"),
    ("**/AppData/Local/Packages/*/LocalCache/**", "Cache"),
    (
        "**/AppData/Local/Packages/*/TempState/**",
        "Temporary files",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// Credentials, keys, tokens: excluded unless the user opts in.
    Secret(&'static str),
    /// Caches and locks: always excluded.
    Transient(&'static str),
}

struct Rules {
    secrets: GlobSet,
    transient: GlobSet,
}

fn build(patterns: &[(&str, &str)]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for (p, _) in patterns {
        // Patterns are compile-time constants covered by tests.
        #[allow(clippy::unwrap_used)]
        b.add(GlobBuilder::new(p).case_insensitive(true).build().unwrap());
    }
    #[allow(clippy::unwrap_used)]
    b.build().unwrap()
}

static RULES: LazyLock<Rules> = LazyLock::new(|| Rules {
    secrets: build(SECRETS),
    transient: build(TRANSIENT),
});

/// Classifies a path. `node_modules` etc. are only transient, never secret.
pub fn classify(path: &Path) -> Option<Flag> {
    let r = &*RULES;
    if let Some(i) = r.secrets.matches(path).first() {
        return Some(Flag::Secret(SECRETS[*i].1));
    }
    r.transient
        .matches(path)
        .first()
        .map(|i| Flag::Transient(TRANSIENT[*i].1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_secrets_and_transient() {
        let s = |p: &str| classify(Path::new(p));
        assert_eq!(
            s("/home/u/.ssh/id_ed25519"),
            Some(Flag::Secret("SSH keys and configuration"))
        );
        assert_eq!(
            s("/home/u/Passwords.kdbx"),
            Some(Flag::Secret("Password database"))
        );
        assert_eq!(
            s("/home/u/project/github-recovery-codes.txt"),
            Some(Flag::Secret("Recovery codes"))
        );
        assert!(matches!(
            s("/home/u/.mozilla/firefox/x.default/logins.json"),
            Some(Flag::Secret(_))
        ));
        assert!(matches!(
            s("/home/u/.var/app/com.brave.Browser/config/BraveSoftware/Default/Login Data"),
            Some(Flag::Secret(_))
        ));
        assert!(matches!(
            s("/home/u/.var/app/org.x/cache/a/b"),
            Some(Flag::Transient(_))
        ));
        assert!(matches!(
            s("/home/u/proj/node_modules/x/index.js"),
            Some(Flag::Transient(_))
        ));
        assert!(matches!(
            s("/Users/u/Library/Keychains/login.keychain-db"),
            Some(Flag::Secret(_))
        ));
        assert!(matches!(
            s("C:/Users/u/AppData/Roaming/Microsoft/Protect/S-1-5/abc"),
            Some(Flag::Secret(_))
        ));
        assert!(matches!(
            s("C:/Users/u/AppData/Local/Microsoft/Credentials/ABC"),
            Some(Flag::Secret(_))
        ));
        assert!(matches!(
            s("/Users/u/Library/Caches/com.x/data"),
            Some(Flag::Transient(_))
        ));
        assert_eq!(
            s("C:/Users/u/AppData/Roaming/Code/User/settings.json"),
            None
        );
        assert_eq!(s("/home/u/Documents/report.odt"), None);
        assert_eq!(s("/home/u/.config/Code/User/settings.json"), None);
    }
}
