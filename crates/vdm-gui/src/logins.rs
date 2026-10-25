//! Site logins: a saved username and password per site.
//! The settings file keeps the list of (site, username) pairs; the
//! passwords live in the OS credential store. No iced here, so it is
//! unit-testable as plain Rust.

#[cfg(test)]
use std::collections::HashMap;
use std::fmt;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, PoisonError};

use keyring::Error as KeyringError;
use serde::{Deserialize, Serialize};

/// Service name the passwords are filed under in the OS credential store.
const SERVICE: &str = "VDM";

/// A saved login, as stored in settings.toml (no password).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginEntry {
    /// Host it applies to: "example.com" (exact) or "*.example.com" (any subdomain, and example.com itself).
    pub site: String,
    pub username: String,
}

impl LoginEntry {
    /// Lowercased, trimmed, without scheme/port/path if the user pasted a URL
    /// ("https://Files.Example.com:8080/x" → "files.example.com"; "*.Example.com" stays a wildcard).
    /// None for empty or malformed input.
    pub fn normalized_site(input: &str) -> Option<String> {
        let mut s = input.trim().to_lowercase();
        if let Some(pos) = s.find("://") {
            s.drain(..pos + 3);
        }
        if let Some(end) = s.find(['/', '?', '#']) {
            s.truncate(end);
        }
        // Drop user info ("user:pass@host").
        if let Some(at) = s.rfind('@') {
            s.drain(..=at);
        }
        let (wildcard, rest) = match s.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, s.as_str()),
        };
        let host = strip_port(rest)?;
        let host = host.strip_suffix('.').unwrap_or(host);
        if !valid_host(host) {
            return None;
        }
        Some(if wildcard { format!("*.{host}") } else { host.to_string() })
    }

    /// Whether this entry applies to `host` (case-insensitive). The engine does
    /// the real matching (`vdm_core::login_for`); this mirrors it for tests.
    #[cfg(test)]
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_lowercase();
        if host.is_empty() {
            return false;
        }
        let site = self.site.trim().to_lowercase();
        match site.strip_prefix("*.") {
            Some(base) => {
                !base.is_empty()
                    && (host == base
                        || host.strip_suffix(base).is_some_and(|rest| rest.ends_with('.')))
            }
            None => host == site,
        }
    }

    /// Sort key for `best_match`: exact sites beat wildcards, and a longer
    /// wildcard suffix beats a shorter one.
    #[cfg(test)]
    fn specificity(&self) -> (bool, usize) {
        let site = self.site.trim().to_lowercase();
        match site.strip_prefix("*.") {
            Some(base) => (false, base.len()),
            None => (true, 0),
        }
    }
}

/// "host" or "host:port" → "host". The port must be digits (or empty).
fn strip_port(hostport: &str) -> Option<&str> {
    match hostport.split_once(':') {
        None => Some(hostport),
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => Some(host),
        Some(_) => None,
    }
}

/// Dot-separated labels of letters, digits, '-' or '_'; no empty labels.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        })
}

/// The key a password is stored under: "<site>|<username>".
fn account(entry: &LoginEntry) -> String {
    format!("{}|{}", entry.site.trim().to_lowercase(), entry.username.trim())
}

/// Same site and username (the identity of an entry).
fn same_login(a: &LoginEntry, b: &LoginEntry) -> bool {
    account(a) == account(b)
}

/// Normalize the site and username of an entry the user typed.
fn validated(entry: LoginEntry) -> Result<LoginEntry, String> {
    let site = LoginEntry::normalized_site(&entry.site)
        .ok_or_else(|| format!("\"{}\" is not a valid site", entry.site.trim()))?;
    let username = entry.username.trim().to_string();
    if username.is_empty() {
        return Err("Username must not be empty".into());
    }
    Ok(LoginEntry { site, username })
}

/// Where passwords live.
pub trait SecretStore: Send + Sync {
    fn get(&self, entry: &LoginEntry) -> Result<Option<String>, String>;
    fn set(&self, entry: &LoginEntry, password: &str) -> Result<(), String>;
    fn delete(&self, entry: &LoginEntry) -> Result<(), String>; // deleting a missing secret is Ok
}

/// The OS credential store via `keyring` (service name "VDM", account "<site>|<username>").
pub struct KeyringStore;

impl KeyringStore {
    /// Check that the platform's default credential store is available. The
    /// `keyring` crate picks and initializes it on first use; this forces that
    /// now so the error can be reported up front.
    pub fn new() -> Result<Self, String> {
        match keyring::Entry::store_status() {
            Ok(()) => Ok(Self),
            Err(err) => Err(format!("cannot open the OS credential store: {err}")),
        }
    }

    fn entry(entry: &LoginEntry) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, &account(entry)).map_err(|err| err.to_string())
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, entry: &LoginEntry) -> Result<Option<String>, String> {
        match Self::entry(entry)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn set(&self, entry: &LoginEntry, password: &str) -> Result<(), String> {
        Self::entry(entry)?.set_password(password).map_err(|err| err.to_string())
    }

    fn delete(&self, entry: &LoginEntry) -> Result<(), String> {
        match Self::entry(entry)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }
}

/// In-memory store for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    secrets: Mutex<HashMap<String, String>>,
}

#[cfg(test)]
impl MemoryStore {
    fn secrets(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.secrets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, entry: &LoginEntry) -> Result<Option<String>, String> {
        Ok(self.secrets().get(&account(entry)).cloned())
    }

    fn set(&self, entry: &LoginEntry, password: &str) -> Result<(), String> {
        self.secrets().insert(account(entry), password.to_string());
        Ok(())
    }

    fn delete(&self, entry: &LoginEntry) -> Result<(), String> {
        self.secrets().remove(&account(entry));
        Ok(())
    }
}

/// A login with its password, ready to hand to the engine.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedLogin {
    pub site: String,
    pub username: String,
    pub password: String,
}

// Hand-written so the password never ends up in a log line.
impl fmt::Debug for ResolvedLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedLogin")
            .field("site", &self.site)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Look up the password of every entry; entries whose password is missing or
/// unreadable are skipped (and reported in the second vector as "site (username): error").
pub fn resolve(
    entries: &[LoginEntry],
    store: &dyn SecretStore,
) -> (Vec<ResolvedLogin>, Vec<String>) {
    let mut logins = Vec::new();
    let mut problems = Vec::new();
    for entry in entries {
        match store.get(entry) {
            Ok(Some(password)) => logins.push(ResolvedLogin {
                site: entry.site.clone(),
                username: entry.username.clone(),
                password,
            }),
            Ok(None) => {
                problems.push(format!("{} ({}): no password stored", entry.site, entry.username))
            }
            Err(err) => problems.push(format!("{} ({}): {err}", entry.site, entry.username)),
        }
    }
    (logins, problems)
}

/// Add or replace (same site + username) an entry and store its password.
/// The password is stored first; if that fails, `entries` is left unchanged.
pub fn upsert(
    entries: &mut Vec<LoginEntry>,
    entry: LoginEntry,
    password: &str,
    store: &dyn SecretStore,
) -> Result<(), String> {
    let entry = validated(entry)?;
    if password.is_empty() {
        return Err("Password must not be empty".into());
    }
    store.set(&entry, password)?;
    match entries.iter_mut().find(|e| same_login(e, &entry)) {
        Some(existing) => *existing = entry,
        None => entries.push(entry),
    }
    Ok(())
}

/// Remove an entry and its password. The password is deleted first; if that
/// fails, `entries` is left unchanged.
pub fn remove(
    entries: &mut Vec<LoginEntry>,
    entry: &LoginEntry,
    store: &dyn SecretStore,
) -> Result<(), String> {
    store.delete(entry)?;
    entries.retain(|e| !same_login(e, entry));
    Ok(())
}

/// The best entry for `host`: exact site match wins over wildcard; longer wildcard wins over shorter.
#[cfg(test)]
pub fn best_match<'a>(entries: &'a [LoginEntry], host: &str) -> Option<&'a LoginEntry> {
    entries.iter().filter(|e| e.matches(host)).fold(None, |best, e| match best {
        Some(b) if b.specificity() >= e.specificity() => Some(b),
        _ => Some(e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(site: &str, username: &str) -> LoginEntry {
        LoginEntry { site: site.into(), username: username.into() }
    }

    /// A store whose reads always fail, to check that `resolve` reports them.
    struct BrokenStore;

    impl SecretStore for BrokenStore {
        fn get(&self, _: &LoginEntry) -> Result<Option<String>, String> {
            Err("vault locked".into())
        }
        fn set(&self, _: &LoginEntry, _: &str) -> Result<(), String> {
            Err("vault locked".into())
        }
        fn delete(&self, _: &LoginEntry) -> Result<(), String> {
            Err("vault locked".into())
        }
    }

    #[test]
    fn normalized_site_cleans_urls_ports_and_case() {
        let n = LoginEntry::normalized_site;
        assert_eq!(n("https://Files.Example.com:8080/x").as_deref(), Some("files.example.com"));
        assert_eq!(n("  example.com  ").as_deref(), Some("example.com"));
        assert_eq!(
            n("ftp://user:pw@ftp.example.com/pub?x=1#y").as_deref(),
            Some("ftp.example.com")
        );
        assert_eq!(n("example.com:21").as_deref(), Some("example.com"));
        assert_eq!(n("example.com.").as_deref(), Some("example.com"));
        assert_eq!(n("my_host-1.local").as_deref(), Some("my_host-1.local"));
    }

    #[test]
    fn normalized_site_keeps_wildcards() {
        let n = LoginEntry::normalized_site;
        assert_eq!(n("*.Example.com").as_deref(), Some("*.example.com"));
        assert_eq!(n("https://*.example.com:443/a").as_deref(), Some("*.example.com"));
    }

    #[test]
    fn normalized_site_rejects_garbage() {
        let n = LoginEntry::normalized_site;
        for bad in [
            "",
            "   ",
            "://",
            "http://",
            "*",
            "*.",
            ".",
            "a..b",
            "not a site",
            "a*.b.com",
            "example.com:http",
            "a:b:c",
            "*.*.com",
            "ex ample.com",
        ] {
            assert_eq!(n(bad), None, "{bad:?} should be rejected");
        }
    }

    #[test]
    fn matches_exact_and_case_insensitive() {
        let e = login("example.com", "bob");
        assert!(e.matches("example.com"));
        assert!(e.matches("EXAMPLE.com"));
        assert!(e.matches("example.com."));
        assert!(!e.matches("www.example.com"));
        assert!(!e.matches(""));
    }

    #[test]
    fn matches_wildcard_includes_bare_domain() {
        let e = login("*.example.com", "bob");
        assert!(e.matches("example.com"));
        assert!(e.matches("files.example.com"));
        assert!(e.matches("A.B.Example.COM"));
        assert!(!e.matches("notexample.com"));
    }

    #[test]
    fn matches_wildcard_is_not_a_suffix_trick() {
        let e = login("*.example.com", "bob");
        assert!(!e.matches("badexample.com"));
        assert!(!e.matches("evil.com.example.org"));
        assert!(!e.matches("example.com.evil.net"));
        assert!(!login("*.", "bob").matches("com"));
    }

    #[test]
    fn best_match_precedence() {
        let entries = [
            login("*.example.com", "wild"),
            login("*.files.example.com", "longer-wild"),
            login("files.example.com", "exact"),
            login("other.org", "unrelated"),
        ];
        let pick = |host| best_match(&entries, host).map(|e| e.username.as_str());
        assert_eq!(pick("files.example.com"), Some("exact"));
        assert_eq!(pick("x.files.example.com"), Some("longer-wild"));
        assert_eq!(pick("mail.example.com"), Some("wild"));
        assert_eq!(pick("example.com"), Some("wild"));
        assert_eq!(pick("nomatch.net"), None);
        assert_eq!(best_match(&[], "example.com"), None);
    }

    #[test]
    fn upsert_replaces_and_stores_password() {
        let store = MemoryStore::default();
        let mut entries = Vec::new();
        upsert(&mut entries, login("Example.com", "bob"), "one", &store).unwrap();
        assert_eq!(entries, [login("example.com", "bob")]);
        assert_eq!(store.get(&login("example.com", "bob")).unwrap().as_deref(), Some("one"));

        upsert(&mut entries, login("example.com", "bob"), "two", &store).unwrap();
        upsert(&mut entries, login("example.com", "alice"), "three", &store).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(store.get(&login("example.com", "bob")).unwrap().as_deref(), Some("two"));
        assert_eq!(store.get(&login("example.com", "alice")).unwrap().as_deref(), Some("three"));
    }

    #[test]
    fn upsert_rejects_bad_input_and_leaves_list_alone() {
        let store = MemoryStore::default();
        let mut entries = vec![login("example.com", "bob")];
        assert!(upsert(&mut entries, login("not a site", "bob"), "pw", &store).is_err());
        assert!(upsert(&mut entries, login("example.org", "  "), "pw", &store).is_err());
        assert!(upsert(&mut entries, login("example.org", "amy"), "", &store).is_err());
        assert_eq!(entries, [login("example.com", "bob")]);
        assert_eq!(store.get(&login("example.org", "amy")).unwrap(), None);
    }

    #[test]
    fn upsert_failure_from_store_leaves_list_alone() {
        let mut entries = Vec::new();
        let err =
            upsert(&mut entries, login("example.com", "bob"), "pw", &BrokenStore).unwrap_err();
        assert!(err.contains("vault locked"));
        assert!(entries.is_empty());
    }

    #[test]
    fn remove_deletes_entry_and_secret() {
        let store = MemoryStore::default();
        let mut entries = Vec::new();
        upsert(&mut entries, login("example.com", "bob"), "pw", &store).unwrap();
        upsert(&mut entries, login("example.com", "alice"), "pw2", &store).unwrap();

        remove(&mut entries, &login("EXAMPLE.com", "bob"), &store).unwrap();
        assert_eq!(entries, [login("example.com", "alice")]);
        assert_eq!(store.get(&login("example.com", "bob")).unwrap(), None);
        assert_eq!(store.get(&login("example.com", "alice")).unwrap().as_deref(), Some("pw2"));

        // Removing something that is not there is fine.
        remove(&mut entries, &login("gone.org", "x"), &store).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn remove_failure_leaves_list_alone() {
        let mut entries = vec![login("example.com", "bob")];
        assert!(remove(&mut entries, &login("example.com", "bob"), &BrokenStore).is_err());
        assert_eq!(entries, [login("example.com", "bob")]);
    }

    #[test]
    fn resolve_skips_missing_passwords_with_a_message() {
        let store = MemoryStore::default();
        let mut entries = Vec::new();
        upsert(&mut entries, login("example.com", "bob"), "pw", &store).unwrap();
        let orphan = login("orphan.org", "carl");

        let mut all = entries.clone();
        all.push(orphan);
        let (logins, problems) = resolve(&all, &store);
        assert_eq!(
            logins,
            [ResolvedLogin {
                site: "example.com".into(),
                username: "bob".into(),
                password: "pw".into()
            }]
        );
        assert_eq!(problems, ["orphan.org (carl): no password stored"]);
    }

    #[test]
    fn resolve_reports_unreadable_passwords() {
        let (logins, problems) = resolve(&[login("example.com", "bob")], &BrokenStore);
        assert!(logins.is_empty());
        assert_eq!(problems, ["example.com (bob): vault locked"]);
    }

    #[test]
    fn resolved_login_debug_hides_password() {
        let login = ResolvedLogin {
            site: "example.com".into(),
            username: "bob".into(),
            password: "hunter2".into(),
        };
        let text = format!("{login:?}");
        assert!(text.contains("example.com"));
        assert!(!text.contains("hunter2"));
    }

    #[test]
    fn serde_round_trip_through_toml() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct File {
            #[serde(default)]
            logins: Vec<LoginEntry>,
        }

        let file =
            File { logins: vec![login("*.example.com", "bob"), login("ftp.example.org", "alice")] };
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(!text.to_lowercase().contains("password"));
        assert_eq!(toml::from_str::<File>(&text).unwrap(), file);
        assert_eq!(toml::from_str::<File>("").unwrap(), File { logins: Vec::new() });
    }

    /// Talks to the real OS credential store. Ignored so CI never needs a keychain:
    /// run with `cargo test -p vdm-gui logins -- --ignored`.
    #[test]
    #[ignore = "uses the OS credential store"]
    fn keyring_store_round_trip() {
        let store = KeyringStore::new().expect("OS credential store is available");
        let entry = login(&format!("vdm-test-{}.invalid", std::process::id()), "tester");

        // Deletes the credential when the test ends, even if an assertion fails.
        struct Cleanup<'a>(&'a KeyringStore, &'a LoginEntry);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.delete(self.1);
            }
        }
        let _cleanup = Cleanup(&store, &entry);

        assert_eq!(store.get(&entry).unwrap(), None);
        store.set(&entry, "first").unwrap();
        assert_eq!(store.get(&entry).unwrap().as_deref(), Some("first"));
        store.set(&entry, "second").unwrap();
        assert_eq!(store.get(&entry).unwrap().as_deref(), Some("second"));
        store.delete(&entry).unwrap();
        assert_eq!(store.get(&entry).unwrap(), None);
        store.delete(&entry).unwrap(); // deleting a missing secret is Ok
    }
}
