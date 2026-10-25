//! Site logins and proxy settings.

use base64::Engine as _;
use reqwest::Url;
use reqwest::header::{self, HeaderMap, HeaderValue};

/// A username and password for one site, sent as HTTP Basic auth (or as the
/// FTP login) to matching hosts only.
#[derive(Clone, PartialEq, Eq)]
pub struct SiteLogin {
    /// "example.com" (exact) or "*.example.com" (subdomains and the domain itself).
    pub site: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for SiteLogin {
    // Never print the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteLogin")
            .field("site", &self.site)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl SiteLogin {
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let site = self.site.trim().to_ascii_lowercase();
        match site.strip_prefix("*.") {
            Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
            None => host == site,
        }
    }
}

/// The login for `host`: an exact site beats a wildcard, a longer wildcard
/// beats a shorter one.
pub fn login_for<'a>(logins: &'a [SiteLogin], host: &str) -> Option<&'a SiteLogin> {
    logins
        .iter()
        .filter(|l| l.matches(host))
        .max_by_key(|l| (!l.site.trim().starts_with("*."), l.site.trim().len()))
}

/// Add `Authorization: Basic …` for the URL's host when a login matches, unless
/// the request already carries credentials (its own header or URL userinfo).
pub(crate) fn apply_login(headers: &mut HeaderMap, url: &Url, logins: &[SiteLogin]) {
    if headers.contains_key(header::AUTHORIZATION) || !url.username().is_empty() {
        return;
    }
    let Some(login) = url.host_str().and_then(|h| login_for(logins, h)) else { return };
    let token = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", login.username, login.password));
    if let Ok(mut value) = HeaderValue::from_str(&format!("Basic {token}")) {
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }
}

/// The lowercase host of a URL, if it parses.
pub fn url_host(url: &str) -> Option<String> {
    Url::parse(url.trim()).ok()?.host_str().map(str::to_ascii_lowercase)
}

/// Which proxy to use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxySetting {
    /// The system's proxy settings (and `HTTP_PROXY`-style variables).
    #[default]
    System,
    /// Connect directly.
    None,
    /// `http://host:port`, `socks5://user:pass@host:port`, ...
    Url(String),
}

impl std::str::FromStr for ProxySetting {
    type Err = String;

    /// "" or "system" → System, "none"/"direct" → None, else a proxy URL.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "" | "system" => Ok(Self::System),
            "none" | "direct" | "off" => Ok(Self::None),
            _ => {
                let url = Url::parse(s).map_err(|_| format!("`{s}` is not a proxy address"))?;
                match url.scheme() {
                    "http" | "https" | "socks5" | "socks5h" => Ok(Self::Url(s.to_owned())),
                    other => {
                        Err(format!("unsupported proxy type `{other}`; use http, https or socks5"))
                    }
                }
            }
        }
    }
}

impl std::fmt::Display for ProxySetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::None => f.write_str("none"),
            Self::Url(u) => f.write_str(u),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(site: &str) -> SiteLogin {
        SiteLogin { site: site.into(), username: "u".into(), password: "p".into() }
    }

    #[test]
    fn matching_and_precedence() {
        assert!(login("example.com").matches("Example.COM"));
        assert!(!login("example.com").matches("files.example.com"));
        assert!(login("*.example.com").matches("files.example.com"));
        assert!(login("*.example.com").matches("example.com"));
        assert!(!login("*.example.com").matches("badexample.com"));
        let all = [login("*.example.com"), login("files.example.com"), login("*.cdn.example.com")];
        assert_eq!(login_for(&all, "files.example.com").unwrap().site, "files.example.com");
        assert_eq!(login_for(&all, "x.cdn.example.com").unwrap().site, "*.cdn.example.com");
        assert_eq!(login_for(&all, "other.org"), None);
    }

    #[test]
    fn basic_auth_header_only_when_needed() {
        let logins = [SiteLogin {
            site: "files.test".into(),
            username: "ann".into(),
            password: "s3cret".into(),
        }];
        let mut h = HeaderMap::new();
        apply_login(&mut h, &Url::parse("https://files.test/a.zip").unwrap(), &logins);
        // base64("ann:s3cret")
        assert_eq!(h[header::AUTHORIZATION], "Basic YW5uOnMzY3JldA==");
        assert!(h[header::AUTHORIZATION].is_sensitive());

        let mut other = HeaderMap::new();
        apply_login(&mut other, &Url::parse("https://elsewhere.test/a.zip").unwrap(), &logins);
        assert!(other.is_empty(), "never sent to other hosts");

        let mut own = HeaderMap::new();
        own.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        apply_login(&mut own, &Url::parse("https://files.test/a").unwrap(), &logins);
        assert_eq!(own[header::AUTHORIZATION], "Bearer x", "the request's own credentials win");

        let mut userinfo = HeaderMap::new();
        apply_login(&mut userinfo, &Url::parse("https://bob:pw@files.test/a").unwrap(), &logins);
        assert!(userinfo.is_empty(), "URL credentials win");
    }

    #[test]
    fn debug_hides_the_password() {
        let s = format!(
            "{:?}",
            SiteLogin { site: "s".into(), username: "u".into(), password: "hunter2".into() }
        );
        assert!(!s.contains("hunter2"), "{s}");
    }

    #[test]
    fn proxy_settings() {
        assert_eq!("".parse(), Ok(ProxySetting::System));
        assert_eq!("System".parse(), Ok(ProxySetting::System));
        assert_eq!("none".parse(), Ok(ProxySetting::None));
        assert_eq!(
            "socks5://127.0.0.1:1080".parse(),
            Ok(ProxySetting::Url("socks5://127.0.0.1:1080".into()))
        );
        assert!("ftp://x:21".parse::<ProxySetting>().is_err());
        assert!("not a url".parse::<ProxySetting>().is_err());
        assert_eq!(ProxySetting::None.to_string(), "none");
    }
}
