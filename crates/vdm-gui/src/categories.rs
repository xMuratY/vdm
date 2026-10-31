//! File-type categories and clipboard download-link detection.
//!
//! Plain Rust with no iced dependency, so everything here can be unit-tested.

/// File-type groups, each with its own subfolder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Other,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Compressed,
        Category::Documents,
        Category::Music,
        Category::Programs,
        Category::Video,
        Category::Other,
    ];

    /// Category of a file name, by its extension (case-insensitive).
    ///
    /// A `?query` or `#fragment` is ignored, and only the last path segment counts.
    /// Multi-part extensions count by their last part (`a.tar.gz` is compressed).
    /// No extension, or an unknown one, is [`Category::Other`].
    pub fn from_name(file_name: &str) -> Category {
        extension(file_name).map_or(Category::Other, |ext| extension_category(&ext))
    }

    /// Subfolder name, or `None` for [`Category::Other`].
    pub fn folder(self) -> Option<&'static str> {
        match self {
            Category::Compressed => Some("Compressed"),
            Category::Documents => Some("Documents"),
            Category::Music => Some("Music"),
            Category::Programs => Some("Programs"),
            Category::Video => Some("Video"),
            Category::Other => None,
        }
    }

    /// Human-readable label.
    pub fn label(self) -> String {
        match self {
            Category::Compressed => crate::i18n::t!("category.compressed"),
            Category::Documents => crate::i18n::t!("category.documents"),
            Category::Music => crate::i18n::t!("category.music"),
            Category::Programs => crate::i18n::t!("category.programs"),
            Category::Video => crate::i18n::t!("category.video"),
            Category::Other => crate::i18n::t!("category.other"),
        }
    }
}

/// The lowercase extension of the last path segment, ignoring `?query` and `#fragment`.
/// `None` when there is no extension, including dot-files such as `.bashrc`.
fn extension(file_name: &str) -> Option<String> {
    let name = file_name.split(['?', '#']).next().unwrap_or(file_name);
    let segment = name.rsplit('/').next().unwrap_or(name);
    let (stem, ext) = segment.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Category for a lowercase extension.
fn extension_category(ext: &str) -> Category {
    match ext {
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "cab" => {
            Category::Compressed
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        | "txt" | "rtf" | "csv" | "epub" => Category::Documents,
        "mp3" | "flac" | "wav" | "aac" | "ogg" | "oga" | "m4a" | "opus" | "wma" => Category::Music,
        "exe" | "msi" | "msix" | "appx" | "apk" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" => {
            Category::Programs
        }
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" => {
            Category::Video
        }
        _ => Category::Other,
    }
}

/// True if `text` (after trimming) is a single http:// or https:// URL with no whitespace
/// inside, whose last non-empty path segment has an extension of a known category.
/// Query strings and fragments are allowed and ignored. The scheme is case-insensitive.
pub fn is_download_link(text: &str) -> bool {
    let text = text.trim();
    if text.chars().any(char::is_whitespace) {
        return false;
    }
    if text.get(..20).is_some_and(|s| s.eq_ignore_ascii_case("magnet:?xt=urn:btih:")) {
        return true;
    }
    let Some(rest) = strip_http_scheme(text) else {
        return false;
    };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let Some((authority, path)) = rest.split_once('/') else {
        return false;
    };
    if authority.is_empty() {
        return false;
    }
    let Some(segment) = path.split('/').rev().find(|s| !s.is_empty()) else {
        return false;
    };
    Category::from_name(segment) != Category::Other
}

/// The part after `http://` or `https://`, matching the scheme case-insensitively.
fn strip_http_scheme(text: &str) -> Option<&str> {
    ["https://", "http://"].into_iter().find_map(|scheme| {
        let head = text.get(..scheme.len())?;
        head.eq_ignore_ascii_case(scheme).then(|| &text[scheme.len()..])
    })
}

/// Remembers the last clipboard text so the same copy only triggers once.
#[derive(Debug, Default)]
pub struct ClipboardWatcher {
    last: Option<String>,
    primed: bool,
}

impl ClipboardWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the current clipboard text (`None` when it holds no text). Returns the trimmed
    /// URL when the clipboard has changed since the previous call and now holds a download
    /// link. The very first call only records the starting content.
    pub fn observe(&mut self, text: Option<&str>) -> Option<String> {
        let previous = std::mem::replace(&mut self.last, text.map(str::to_owned));
        let was_primed = std::mem::replace(&mut self.primed, true);
        if !was_primed || previous.as_deref() == text {
            return None;
        }
        let url = text?.trim();
        is_download_link(url).then(|| url.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://x.com/files/setup.exe";

    #[test]
    fn from_name_maps_every_extension_to_its_category() {
        for ext in ["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "cab"] {
            assert_eq!(Category::from_name(&format!("f.{ext}")), Category::Compressed, "{ext}");
        }
        for ext in [
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "rtf",
            "csv", "epub",
        ] {
            assert_eq!(Category::from_name(&format!("f.{ext}")), Category::Documents, "{ext}");
        }
        for ext in ["mp3", "flac", "wav", "aac", "ogg", "oga", "m4a", "opus", "wma"] {
            assert_eq!(Category::from_name(&format!("f.{ext}")), Category::Music, "{ext}");
        }
        for ext in ["exe", "msi", "msix", "appx", "apk", "dmg", "pkg", "deb", "rpm", "appimage"] {
            assert_eq!(Category::from_name(&format!("f.{ext}")), Category::Programs, "{ext}");
        }
        for ext in ["mp4", "mkv", "avi", "mov", "webm", "wmv", "flv", "m4v", "mpg", "mpeg"] {
            assert_eq!(Category::from_name(&format!("f.{ext}")), Category::Video, "{ext}");
        }
        assert_eq!(Category::from_name("photo.png"), Category::Other);
    }

    #[test]
    fn from_name_representative_names() {
        assert_eq!(Category::from_name("backup.zip"), Category::Compressed);
        assert_eq!(Category::from_name("manual.pdf"), Category::Documents);
        assert_eq!(Category::from_name("song.mp3"), Category::Music);
        assert_eq!(Category::from_name("setup.exe"), Category::Programs);
        assert_eq!(Category::from_name("clip.mp4"), Category::Video);
    }

    #[test]
    fn from_name_is_case_insensitive() {
        assert_eq!(Category::from_name("SETUP.EXE"), Category::Programs);
        assert_eq!(Category::from_name("Movie.MkV"), Category::Video);
        assert_eq!(Category::from_name("Report.PdF"), Category::Documents);
    }

    #[test]
    fn from_name_uses_last_part_of_multi_part_extension() {
        assert_eq!(Category::from_name("a.tar.gz"), Category::Compressed);
        assert_eq!(Category::from_name("data.tar.bz2"), Category::Compressed);
        assert_eq!(Category::from_name("notes.txt.zip"), Category::Compressed);
    }

    #[test]
    fn from_name_ignores_query_and_fragment() {
        assert_eq!(Category::from_name("setup.EXE?sig=1"), Category::Programs);
        assert_eq!(Category::from_name("doc.pdf#page=2"), Category::Documents);
        assert_eq!(Category::from_name("page.html?file=a.zip"), Category::Other);
        assert_eq!(Category::from_name("https://x.com/a/b.mp3?t=1#x"), Category::Music);
    }

    #[test]
    fn from_name_without_known_extension_is_other() {
        assert_eq!(Category::from_name("README"), Category::Other);
        assert_eq!(Category::from_name(".bashrc"), Category::Other);
        assert_eq!(Category::from_name(".zip"), Category::Other);
        assert_eq!(Category::from_name("file."), Category::Other);
        assert_eq!(Category::from_name("folder.zip/readme"), Category::Other);
        assert_eq!(Category::from_name(""), Category::Other);
    }

    #[test]
    fn folder_and_label_for_every_category() {
        assert_eq!(Category::ALL.len(), 6);
        assert_eq!(Category::Compressed.folder(), Some("Compressed"));
        assert_eq!(Category::Documents.folder(), Some("Documents"));
        assert_eq!(Category::Music.folder(), Some("Music"));
        assert_eq!(Category::Programs.folder(), Some("Programs"));
        assert_eq!(Category::Video.folder(), Some("Video"));
        assert_eq!(Category::Other.folder(), None);

        assert_eq!(Category::Compressed.label(), "Compressed");
        assert_eq!(Category::Documents.label(), "Documents");
        assert_eq!(Category::Music.label(), "Music");
        assert_eq!(Category::Programs.label(), "Programs");
        assert_eq!(Category::Video.label(), "Video");
        assert_eq!(Category::Other.label(), "Other");

        for category in Category::ALL {
            assert_eq!(category.label(), category.folder().unwrap_or("Other"));
        }
    }

    #[test]
    fn all_is_in_declared_order() {
        assert_eq!(
            Category::ALL,
            [
                Category::Compressed,
                Category::Documents,
                Category::Music,
                Category::Programs,
                Category::Video,
                Category::Other,
            ]
        );
    }

    #[test]
    fn download_link_accepts_known_file_urls() {
        assert!(is_download_link("https://x.com/a/setup.EXE?sig=1"));
        assert!(is_download_link("http://x.com/a.zip"));
        assert!(is_download_link("https://x.com/a/b.zip#frag"));
        assert!(is_download_link("https://x.com/a.zip/"));
    }

    #[test]
    fn download_link_rejects_pages_and_non_files() {
        assert!(!is_download_link("https://x.com/page.html"));
        assert!(!is_download_link("https://x.com/"));
        assert!(!is_download_link("https://x.com"));
        assert!(!is_download_link("https://x.com/a.unknown"));
        assert!(!is_download_link("https:///a.zip"));
        assert!(!is_download_link("https://"));
        assert!(!is_download_link("x.com/a.zip"));
        assert!(!is_download_link("ftp://x.com/a.zip"));
        assert!(!is_download_link(""));
    }

    #[test]
    fn download_link_rejects_text_around_the_url() {
        assert!(!is_download_link("see https://x.com/a.zip"));
        assert!(!is_download_link("https://x.com/a.zip and more"));
        assert!(!is_download_link("https://x.com/a.zip\nhttps://y.com/b.zip"));
    }

    #[test]
    fn download_link_scheme_is_case_insensitive() {
        assert!(is_download_link("HTTPS://x.com/a.zip"));
        assert!(is_download_link("Http://x.com/a.zip"));
    }

    #[test]
    fn download_link_handles_percent_encoded_names() {
        assert!(is_download_link("https://x.com/my%20file.zip"));
        assert!(is_download_link("https://x.com/%E4%BD%A0.pdf"));
    }

    #[test]
    fn download_link_ignores_extension_only_in_query() {
        assert!(!is_download_link("https://x.com/page?file=a.zip"));
        assert!(!is_download_link("https://x.com/page#a.zip"));
        assert!(is_download_link("https://x.com/a.zip?file=page.html"));
    }

    #[test]
    fn download_link_rejects_whitespace_inside() {
        assert!(!is_download_link("https://x.com/my file.zip"));
        assert!(!is_download_link("https://x.com/a\t.zip"));
    }

    #[test]
    fn download_link_trims_surrounding_whitespace() {
        assert!(is_download_link("  https://x.com/a.zip \n"));
        assert!(is_download_link("\thttps://x.com/a.mp4\r\n"));
    }

    #[test]
    fn clipboard_first_call_only_records() {
        let mut watcher = ClipboardWatcher::new();
        assert_eq!(watcher.observe(Some(LINK)), None);
        assert_eq!(watcher.observe(Some(LINK)), None);
    }

    #[test]
    fn clipboard_change_to_link_triggers_once() {
        let mut watcher = ClipboardWatcher::new();
        assert_eq!(watcher.observe(Some("hello")), None);
        assert_eq!(watcher.observe(Some(LINK)), Some(LINK.to_owned()));
        assert_eq!(watcher.observe(Some(LINK)), None);
    }

    #[test]
    fn clipboard_returns_trimmed_url() {
        let mut watcher = ClipboardWatcher::new();
        watcher.observe(Some("hello"));
        assert_eq!(
            watcher.observe(Some("  https://x.com/a.zip\n")),
            Some("https://x.com/a.zip".into())
        );
    }

    #[test]
    fn clipboard_ignores_non_links() {
        let mut watcher = ClipboardWatcher::new();
        watcher.observe(Some("hello"));
        assert_eq!(watcher.observe(Some("https://x.com/page.html")), None);
        assert_eq!(watcher.observe(Some("just some words")), None);
        assert_eq!(watcher.observe(Some("   ")), None);
    }

    #[test]
    fn clipboard_same_link_after_other_text_triggers_again() {
        let mut watcher = ClipboardWatcher::new();
        watcher.observe(Some("hello"));
        assert_eq!(watcher.observe(Some(LINK)), Some(LINK.to_owned()));
        assert_eq!(watcher.observe(Some("something else")), None);
        assert_eq!(watcher.observe(Some(LINK)), Some(LINK.to_owned()));
    }

    #[test]
    fn clipboard_none_counts_as_change() {
        let mut watcher = ClipboardWatcher::new();
        assert_eq!(watcher.observe(Some(LINK)), None);
        assert_eq!(watcher.observe(None), None);
        assert_eq!(watcher.observe(Some(LINK)), Some(LINK.to_owned()));
    }
}
