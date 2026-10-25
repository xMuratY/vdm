//! Choosing a safe local file name for a download.

use percent_encoding::percent_decode_str;
use reqwest::Url;

/// Used when neither the server nor the URL gives a usable name.
pub const FALLBACK_NAME: &str = "download";

/// Longest name we produce, in bytes. Windows allows 255 UTF-16 units per
/// component; staying well below leaves room for ` (n)` and `.vdmpart.json`.
const MAX_NAME_BYTES: usize = 200;

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Pick a name from `Content-Disposition`, then the URL path, then [`FALLBACK_NAME`].
/// The result is always sanitized.
pub fn choose(content_disposition: Option<&str>, url: &Url) -> String {
    content_disposition
        .and_then(from_content_disposition)
        .or_else(|| from_url(url))
        .map(|n| sanitize(&n))
        .unwrap_or_else(|| FALLBACK_NAME.to_owned())
}

/// Extract the file name from a `Content-Disposition` header (RFC 6266),
/// preferring the RFC 5987 `filename*` form.
pub fn from_content_disposition(value: &str) -> Option<String> {
    let params = parse_params(value);
    let extended =
        params.iter().find(|(k, _)| k == "filename*").and_then(|(_, v)| decode_ext_value(v));
    extended
        .or_else(|| params.iter().find(|(k, _)| k == "filename").map(|(_, v)| v.clone()))
        .filter(|n| !n.trim().is_empty())
}

/// Last non-empty URL path segment, percent-decoded.
pub fn from_url(url: &Url) -> Option<String> {
    let last = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let name = percent_decode_str(last).decode_utf8_lossy().into_owned();
    (!name.trim().is_empty()).then_some(name)
}

/// Make a name safe to create on Windows, macOS and Linux: no directory
/// parts, no reserved characters or device names, no trailing dots/spaces.
pub fn sanitize(name: &str) -> String {
    // Keep only the last path component, which also defeats `../` traversal.
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || r#"<>:"|?*"#.contains(c) { '_' } else { c })
        .collect();
    let mut s = cleaned.trim().trim_end_matches(['.', ' ']).to_owned();
    if s.is_empty() {
        return FALLBACK_NAME.to_owned();
    }
    let stem = s.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    truncate(s)
}

/// `name` for `n == 0`, otherwise `stem (n).ext`, the way browsers number duplicates.
pub fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

fn truncate(s: String) -> String {
    if s.len() <= MAX_NAME_BYTES {
        return s;
    }
    // Keep a short extension, cut the stem on a char boundary.
    let ext = s.rfind('.').filter(|&i| i > 0 && s.len() - i <= 16).map_or("", |i| &s[i..]);
    let mut cut = MAX_NAME_BYTES - ext.len();
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = s[..cut].trim_end_matches(['.', ' ']);
    format!("{stem}{ext}")
}

/// Decode an RFC 5987 `charset'lang'pct-encoded` value.
fn decode_ext_value(v: &str) -> Option<String> {
    let mut parts = v.splitn(3, '\'');
    let charset = parts.next()?;
    let _lang = parts.next()?;
    let encoded = parts.next()?;
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.iter().map(|&b| char::from(b)).collect())
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Parse `type; key=value; key="quoted \" value"` into lowercase keys and unquoted values.
fn parse_params(header: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some((_, mut rest)) = header.split_once(';') else {
        return out;
    };
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let key_end = rest.find(['=', ';']).unwrap_or(rest.len());
        let key = rest[..key_end].trim().to_ascii_lowercase();
        rest = &rest[key_end..];
        let value = if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            if let Some(quoted) = after_eq.strip_prefix('"') {
                let mut value = String::new();
                let mut escaped = false;
                let mut end = quoted.len();
                for (i, c) in quoted.char_indices() {
                    if escaped {
                        value.push(c);
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        end = i + 1;
                        break;
                    } else {
                        value.push(c);
                    }
                }
                let tail = &quoted[end..];
                rest = tail.find(';').map_or("", |i| &tail[i + 1..]);
                value
            } else {
                let end = after_eq.find(';').unwrap_or(after_eq.len());
                let value = after_eq[..end].trim().to_owned();
                rest = after_eq.get(end + 1..).unwrap_or("");
                value
            }
        } else {
            rest = rest.strip_prefix(';').unwrap_or("");
            String::new()
        };
        if !key.is_empty() {
            out.push((key, value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn content_disposition_forms() {
        let cd = from_content_disposition;
        assert_eq!(cd(r#"attachment; filename="a b.zip""#).as_deref(), Some("a b.zip"));
        assert_eq!(cd("attachment; filename=plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(cd(r#"attachment; filename="q\"uote.txt""#).as_deref(), Some("q\"uote.txt"));
        assert_eq!(
            cd(r#"attachment; filename="fallback.txt"; filename*=UTF-8''na%C3%AFve%20file.txt"#)
                .as_deref(),
            Some("naïve file.txt")
        );
        assert_eq!(
            cd("attachment; filename*=iso-8859-1'en'%E9t%E9.txt").as_deref(),
            Some("été.txt")
        );
        assert_eq!(cd(r#"attachment; FILENAME="Upper.bin""#).as_deref(), Some("Upper.bin"));
        assert_eq!(
            cd(r#"attachment; filename="semi;colon.txt"; size=3"#).as_deref(),
            Some("semi;colon.txt")
        );
        assert_eq!(cd("attachment"), None);
        assert_eq!(cd(r#"attachment; filename="""#), None);
        assert_eq!(cd(r#"inline; filename="unterminated"#).as_deref(), Some("unterminated"));
    }

    #[test]
    fn url_fallback() {
        assert_eq!(
            from_url(&url("https://x/dir/file%20name.iso?x=1")).as_deref(),
            Some("file name.iso")
        );
        assert_eq!(from_url(&url("https://x/dir/")).as_deref(), Some("dir"));
        assert_eq!(from_url(&url("https://x/")), None);
    }

    #[test]
    fn choose_order() {
        let u = url("https://x/from-url.bin");
        assert_eq!(choose(Some(r#"attachment; filename="cd.bin""#), &u), "cd.bin");
        assert_eq!(choose(Some("attachment"), &u), "from-url.bin");
        assert_eq!(choose(None, &url("https://x/")), FALLBACK_NAME);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize(r"..\..\boot.ini"), "boot.ini");
        assert_eq!(sanitize("a<b>c:d\"e|f?g*h.txt"), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(sanitize("tab\there.txt"), "tab_here.txt");
        assert_eq!(sanitize("trailing. . "), "trailing");
        assert_eq!(sanitize(".."), FALLBACK_NAME);
        assert_eq!(sanitize("   "), FALLBACK_NAME);
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul.txt"), "_nul.txt");
        assert_eq!(sanitize("console.txt"), "console.txt");
        assert_eq!(sanitize(".bashrc"), ".bashrc");
    }

    #[test]
    fn long_names_keep_extension() {
        let long = format!("{}.tar.gz", "é".repeat(300));
        let s = sanitize(&long);
        assert!(s.len() <= MAX_NAME_BYTES);
        assert!(s.ends_with(".gz"));
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("a.zip", 0), "a.zip");
        assert_eq!(numbered("a.zip", 2), "a (2).zip");
        assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
    }
}
