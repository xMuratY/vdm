//! `cargo xtask site`: builds the project website (GitHub Pages) from `site/`.
//!
//! - `site/templates/base.html` wraps every page; `{{name}}` placeholders are
//!   filled per page.
//! - `site/pages/*.html` are page bodies, `site/blog/*.md` blog posts. Both
//!   start with TOML front matter between `+++` lines.
//! - `site/static/` is copied as is; the app's screenshots and icons are
//!   copied from `docs/images` and `assets/icons`.
//!
//! Content may use `{{root}}` (the site's path prefix, e.g. `/repo/`),
//! `{{github}}`, `{{releases}}`, `{{version}}` and `{{year}}`.
//! Besides the pages it writes `blog/`, `feed.xml`, `sitemap.xml`,
//! `robots.txt` and `404.html`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Output folder.
    #[arg(long, default_value = "target/site")]
    out: PathBuf,
    /// Public address of the site, e.g. https://user.github.io/repo.
    #[arg(long, default_value = "http://localhost:8000")]
    base_url: String,
    /// GitHub repository as owner/name; defaults to $GITHUB_REPOSITORY.
    #[arg(long)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageMeta {
    title: String,
    description: String,
    /// Site path, e.g. "/download/"; "/" for the home page.
    path: String,
    /// Extra structured data: "software" or "faq".
    #[serde(default)]
    schema: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PostMeta {
    title: String,
    description: String,
    /// YYYY-MM-DD.
    date: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
struct Post {
    slug: String,
    meta: PostMeta,
    html: String,
    words: usize,
}

/// Values shared by every page.
struct Site {
    /// Scheme and host, e.g. "https://user.github.io".
    origin: String,
    /// Path prefix, e.g. "/repo/" (or "/").
    root: String,
    repo: String,
    version: String,
    year: String,
}

impl Site {
    /// A full address for a site link such as `link("/blog/")`.
    fn absolute(&self, link: &str) -> String {
        format!("{}{}", self.origin, link)
    }

    fn link(&self, path: &str) -> String {
        format!("{}{}", self.root, path.trim_start_matches('/'))
    }

    /// Fill the placeholders content may use.
    fn expand(&self, text: &str) -> String {
        text.replace("{{root}}", &self.root)
            .replace("{{github}}", &format!("https://github.com/{}", self.repo))
            .replace("{{releases}}", &format!("https://github.com/{}/releases/latest", self.repo))
            .replace("{{version}}", &self.version)
            .replace("{{year}}", &self.year)
    }
}

pub fn run(args: Args) -> Result<()> {
    let root_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().context("no workspace root")?;
    let src = root_dir.join("site");
    let out = if args.out.is_absolute() { args.out.clone() } else { root_dir.join(&args.out) };
    let repo = args
        .repo
        .or_else(|| std::env::var("GITHUB_REPOSITORY").ok())
        .unwrap_or_else(|| "OWNER/vibe-download-manager".to_owned());
    let (origin, root) = split_base_url(&args.base_url);
    let site =
        Site { origin, root, repo, version: workspace_version(root_dir)?, year: "2026".to_owned() };

    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("clearing {}", out.display()))?;
    }
    fs::create_dir_all(&out)?;
    copy_dir(&src.join("static"), &out)?;
    copy_dir(&root_dir.join("docs/images"), &out.join("images"))?;
    fs::copy(root_dir.join("assets/icons/vdm.ico"), out.join("favicon.ico"))?;
    fs::copy(root_dir.join("assets/icons/vdm-256.png"), out.join("icon-256.png"))?;

    let base = fs::read_to_string(src.join("templates/base.html"))?;
    let posts = load_posts(&src.join("blog"))?;
    let mut urls: Vec<(String, String)> = Vec::new(); // (path, lastmod)
    let newest = posts.first().map(|p| p.meta.date.clone()).unwrap_or_default();

    for entry in sorted_entries(&src.join("pages"), "html")? {
        let text = fs::read_to_string(&entry)?;
        let (meta, body): (PageMeta, &str) =
            front_matter(&text).with_context(|| format!("front matter of {}", entry.display()))?;
        let body = body.replace("{{latest_posts}}", &latest_posts_html(&site, &posts, 3));
        let json_ld = page_json_ld(&site, &meta, &body);
        let html = render(
            &base,
            &site,
            &Page {
                title: &meta.title,
                description: &meta.description,
                path: &meta.path,
                og_type: "website",
                json_ld,
                content: &site.expand(&body),
            },
        );
        write_page(&out, &meta.path, &html)?;
        urls.push((meta.path.clone(), newest.clone()));
    }

    // Blog index and posts.
    let index = blog_index_html(&site, &posts);
    let html = render(
        &base,
        &site,
        &Page {
            title: "Blog: downloading, faster",
            description: "Guides and news from Vibe Download Manager: faster downloads, resuming, video streams and open-source alternatives.",
            path: "/blog/",
            og_type: "website",
            json_ld: breadcrumbs(&site, &[("Home", "/"), ("Blog", "/blog/")]),
            content: &index,
        },
    );
    write_page(&out, "/blog/", &html)?;
    urls.push(("/blog/".to_owned(), newest.clone()));
    for post in &posts {
        let path = format!("/blog/{}/", post.slug);
        let html = render(
            &base,
            &site,
            &Page {
                title: &post.meta.title,
                description: &post.meta.description,
                path: &path,
                og_type: "article",
                json_ld: post_json_ld(&site, post, &path),
                content: &post_html(&site, post, &posts),
            },
        );
        write_page(&out, &path, &html)?;
        urls.push((path, post.meta.date.clone()));
    }

    // 404, feed, sitemap, robots.
    let not_found = render(
        &base,
        &site,
        &Page {
            title: "Page not found",
            description: "This page doesn't exist. Go back to the Vibe Download Manager home page.",
            path: "/404.html",
            og_type: "website",
            json_ld: String::new(),
            content: &site.expand(
                "<section class=\"wrap prose notfound\"><h1>This page doesn't exist</h1>\
             <p>The link may be old or mistyped. Try the <a href=\"{{root}}\">home page</a> \
             or the <a href=\"{{root}}blog/\">blog</a>.</p></section>",
            ),
        },
    )
    .replace(
        "<link rel=\"canonical\"",
        "<meta name=\"robots\" content=\"noindex\"><link rel=\"canonical\"",
    );
    fs::write(out.join("404.html"), not_found)?;
    fs::write(out.join("feed.xml"), feed_xml(&site, &posts))?;
    fs::write(out.join("sitemap.xml"), sitemap_xml(&site, &urls))?;
    fs::write(
        out.join("robots.txt"),
        format!(
            "User-agent: *\nAllow: /\n\nSitemap: {}\n",
            site.absolute(&site.link("sitemap.xml"))
        ),
    )?;

    println!(
        "built {} pages and {} posts into {}",
        urls.len() - posts.len(),
        posts.len(),
        out.display()
    );
    Ok(())
}

struct Page<'a> {
    title: &'a str,
    description: &'a str,
    path: &'a str,
    og_type: &'a str,
    json_ld: String,
    content: &'a str,
}

fn render(base: &str, site: &Site, page: &Page<'_>) -> String {
    let full_title = full_title(page.title, page.path == "/");
    let current = |prefix: &str| {
        let here = if prefix == "/" { page.path == "/" } else { page.path.starts_with(prefix) };
        if here { " aria-current=\"page\"" } else { "" }
    };
    let html = base
        .replace("{{title}}", &esc(&full_title))
        .replace("{{description}}", &esc(page.description))
        .replace("{{canonical}}", &esc(&site.absolute(&site.link(page.path))))
        .replace("{{og_type}}", page.og_type)
        .replace("{{og_image}}", &esc(&site.absolute(&site.link("images/main-light.png"))))
        .replace("{{json_ld}}", &page.json_ld)
        .replace("{{nav_download}}", current("/download/"))
        .replace("{{nav_compare}}", current("/compare/"))
        .replace("{{nav_blog}}", current("/blog/"))
        .replace("{{nav_faq}}", current("/faq/"))
        .replace("{{content}}", page.content);
    site.expand(&html)
}

/// The `<title>`: the page title plus the site name, kept within the ~60
/// characters search results show.
fn full_title(title: &str, home: bool) -> String {
    const LIMIT: usize = 60;
    if home || title.contains("Vibe") {
        return title.to_owned();
    }
    let long = format!("{title} | Vibe Download Manager");
    if long.chars().count() <= LIMIT { long } else { format!("{title} | Vibe") }
}

fn write_page(out: &Path, path: &str, html: &str) -> Result<()> {
    let file = if path.ends_with('/') {
        out.join(path.trim_matches('/')).join("index.html")
    } else {
        out.join(path.trim_start_matches('/'))
    };
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&file, html).with_context(|| format!("writing {}", file.display()))
}

// ---------------------------------------------------------------- content

fn front_matter<T: serde::de::DeserializeOwned>(text: &str) -> Result<(T, &str)> {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("+++") else { bail!("missing +++ front matter") };
    let Some((meta, body)) = rest.split_once("\n+++") else { bail!("unclosed front matter") };
    let meta: T = toml::from_str(meta)?;
    Ok((meta, body.trim_start_matches(['\r', '\n'])))
}

fn load_posts(dir: &Path) -> Result<Vec<Post>> {
    let mut posts = Vec::new();
    for file in sorted_entries(dir, "md")? {
        let text = fs::read_to_string(&file)?;
        let (meta, body): (PostMeta, &str) =
            front_matter(&text).with_context(|| format!("front matter of {}", file.display()))?;
        if meta.date.len() != 10 {
            bail!("{}: date must be YYYY-MM-DD", file.display());
        }
        let slug = file.file_stem().and_then(|s| s.to_str()).context("bad file name")?.to_owned();
        posts.push(Post {
            slug,
            words: body.split_whitespace().count(),
            html: markdown(body),
            meta,
        });
    }
    // Newest first; same day by title.
    posts.sort_by(|a, b| b.meta.date.cmp(&a.meta.date).then(a.meta.title.cmp(&b.meta.title)));
    Ok(posts)
}

/// Markdown to HTML, with ids on h2/h3 so sections can be linked. Links
/// starting with `/` are made relative to the site root, and `@github` stands
/// for the repository's address.
fn markdown(text: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_SMART_PUNCTUATION;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(text, options));
    let mut out =
        out.replace("href=\"/", "href=\"{{root}}").replace("href=\"@github", "href=\"{{github}}");
    for level in ["h2", "h3"] {
        let open = format!("<{level}>");
        let close = format!("</{level}>");
        let mut done = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(at) = rest.find(&open) {
            let Some(end) = rest[at..].find(&close) else { break };
            let inner = &rest[at + open.len()..at + end];
            done.push_str(&rest[..at]);
            let _ = write!(done, "<{level} id=\"{}\">{inner}{close}", slug(&strip_tags(inner)));
            rest = &rest[at + end + close.len()..];
        }
        done.push_str(rest);
        out = done;
    }
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

/// "How it works, really?" -> "how-it-works-really"
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn reading_minutes(words: usize) -> usize {
    words.div_ceil(220).max(1)
}

fn human_date(date: &str) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let parts: Vec<&str> = date.split('-').collect();
    match parts.as_slice() {
        [y, m, d] => {
            let month = m.parse::<usize>().ok().and_then(|m| MONTHS.get(m.wrapping_sub(1)));
            match (month, d.parse::<u32>()) {
                (Some(month), Ok(day)) => format!("{month} {day}, {y}"),
                _ => date.to_owned(),
            }
        }
        _ => date.to_owned(),
    }
}

fn post_list_item(site: &Site, post: &Post) -> String {
    format!(
        "<li><a href=\"{}\"><span class=\"post-title\">{}</span></a><p>{}</p>\
         <time datetime=\"{}\">{}</time></li>",
        site.link(&format!("/blog/{}/", post.slug)),
        esc(&post.meta.title),
        esc(&post.meta.description),
        post.meta.date,
        human_date(&post.meta.date),
    )
}

fn latest_posts_html(site: &Site, posts: &[Post], n: usize) -> String {
    let items: String = posts.iter().take(n).map(|p| post_list_item(site, p)).collect();
    format!("<ul class=\"post-list\">{items}</ul>")
}

fn blog_index_html(site: &Site, posts: &[Post]) -> String {
    let items: String = posts.iter().map(|p| post_list_item(site, p)).collect();
    format!(
        "<section class=\"wrap blog-index\"><h1>Blog</h1>\
         <p class=\"lede\">How downloads work, how to get the most out of Vibe, and what's new. \
         Follow along with the <a href=\"{}\">RSS feed</a>.</p>\
         <ul class=\"post-list\">{items}</ul></section>",
        site.link("feed.xml")
    )
}

fn post_html(site: &Site, post: &Post, all: &[Post]) -> String {
    let tags: String = post.meta.tags.iter().map(|t| format!("<li>{}</li>", esc(t))).collect();
    let more: String = all
        .iter()
        .filter(|p| p.slug != post.slug)
        .take(3)
        .map(|p| post_list_item(site, p))
        .collect();
    format!(
        "<article class=\"wrap post\">\
           <nav class=\"crumbs\" aria-label=\"Breadcrumb\"><a href=\"{blog}\">Blog</a></nav>\
           <header><h1>{title}</h1>\
             <p class=\"post-meta\"><time datetime=\"{date}\">{human}</time>, {minutes} min read</p>\
           </header>\
           <div class=\"prose\">{body}</div>\
           <ul class=\"tags\" aria-label=\"Topics\">{tags}</ul>\
           <aside class=\"post-cta\"><h2>Try Vibe Download Manager</h2>\
             <p>Free and open source. Multi-connection downloads that resume, a video stream grabber and browser integration.</p>\
             <a class=\"button primary\" href=\"{{{{root}}}}download/\">Download Vibe</a></aside>\
         </article>\
         <section class=\"wrap more-posts\"><h2>More from the blog</h2><ul class=\"post-list\">{more}</ul></section>",
        blog = site.link("/blog/"),
        title = esc(&post.meta.title),
        date = post.meta.date,
        human = human_date(&post.meta.date),
        minutes = reading_minutes(post.words),
        body = site.expand(&post.html),
    )
}

// ---------------------------------------------------------------- structured data

fn json_ld(value: &serde_json::Value) -> String {
    // `</` can't appear inside a script element.
    let text = serde_json::to_string(value).unwrap_or_default().replace("</", "<\\/");
    format!("<script type=\"application/ld+json\">{text}</script>")
}

fn breadcrumbs(site: &Site, trail: &[(&str, &str)]) -> String {
    let items: Vec<serde_json::Value> = trail
        .iter()
        .enumerate()
        .map(|(i, (name, path))| {
            serde_json::json!({
                "@type": "ListItem", "position": i + 1, "name": name,
                "item": site.absolute(&site.link(path)),
            })
        })
        .collect();
    json_ld(&serde_json::json!({
        "@context": "https://schema.org", "@type": "BreadcrumbList", "itemListElement": items,
    }))
}

fn software_json_ld(site: &Site) -> String {
    json_ld(&serde_json::json!({
        "@context": "https://schema.org",
        "@type": "SoftwareApplication",
        "name": "Vibe Download Manager",
        "alternateName": "VDM",
        "description": "A free, open-source download manager and accelerator: multi-connection downloads that resume, an HLS/DASH video grabber, and browser integration.",
        "applicationCategory": "UtilitiesApplication",
        "operatingSystem": "Windows 10, Windows 11, macOS 11, Linux",
        "softwareVersion": site.version,
        "license": "https://opensource.org/license/mit",
        "isAccessibleForFree": true,
        "offers": { "@type": "Offer", "price": "0", "priceCurrency": "USD" },
        "downloadUrl": format!("https://github.com/{}/releases/latest", site.repo),
        "codeRepository": format!("https://github.com/{}", site.repo),
        "image": site.absolute(&site.link("icon-256.png")),
        "screenshot": site.absolute(&site.link("images/main-light.png")),
        "inLanguage": ["en", "es", "fr", "de", "pt-BR", "it", "ru", "tr", "pl", "zh-CN", "ja", "ko"],
    }))
}

/// FAQ structured data from `<details><summary>Q</summary>A</details>` blocks.
fn faq_json_ld(body: &str) -> String {
    let mut items = Vec::new();
    for block in body.split("<details").skip(1) {
        let Some((_, rest)) = block.split_once("<summary>") else { continue };
        let Some((question, rest)) = rest.split_once("</summary>") else { continue };
        let answer = rest.split("</details>").next().unwrap_or_default();
        let answer = strip_tags(answer).split_whitespace().collect::<Vec<_>>().join(" ");
        items.push(serde_json::json!({
            "@type": "Question", "name": strip_tags(question).trim(),
            "acceptedAnswer": { "@type": "Answer", "text": answer },
        }));
    }
    json_ld(&serde_json::json!({
        "@context": "https://schema.org", "@type": "FAQPage", "mainEntity": items,
    }))
}

fn page_json_ld(site: &Site, meta: &PageMeta, body: &str) -> String {
    let mut out = match meta.schema.as_str() {
        "software" => software_json_ld(site),
        "faq" => faq_json_ld(body),
        _ => String::new(),
    };
    if meta.path == "/" {
        out.push_str(&json_ld(&serde_json::json!({
            "@context": "https://schema.org", "@type": "WebSite",
            "name": "Vibe Download Manager", "url": site.absolute(&site.root),
        })));
    } else {
        out.push_str(&breadcrumbs(
            site,
            &[("Home", "/"), (meta.title.as_str(), meta.path.as_str())],
        ));
    }
    out
}

fn post_json_ld(site: &Site, post: &Post, path: &str) -> String {
    let url = site.absolute(&site.link(path));
    json_ld(&serde_json::json!({
        "@context": "https://schema.org",
        "@type": "BlogPosting",
        "headline": post.meta.title,
        "description": post.meta.description,
        "datePublished": post.meta.date,
        "dateModified": post.meta.date,
        "keywords": post.meta.tags.join(", "),
        "wordCount": post.words,
        "mainEntityOfPage": url,
        "url": url,
        "image": site.absolute(&site.link("images/main-light.png")),
        "author": { "@type": "Organization", "name": "Vibe Download Manager contributors" },
        "publisher": {
            "@type": "Organization", "name": "Vibe Download Manager",
            "logo": { "@type": "ImageObject", "url": site.absolute(&site.link("icon-256.png")) },
        },
    })) + &breadcrumbs(site, &[("Home", "/"), ("Blog", "/blog/"), (post.meta.title.as_str(), path)])
}

// ---------------------------------------------------------------- feed and sitemap

fn feed_xml(site: &Site, posts: &[Post]) -> String {
    let updated = posts.first().map_or("2026-01-01".to_owned(), |p| p.meta.date.clone());
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n\
         <title>Vibe Download Manager blog</title>\n\
         <link href=\"{}\"/>\n<link rel=\"self\" href=\"{}\"/>\n<id>{}</id>\n<updated>{updated}T00:00:00Z</updated>\n",
        esc(&site.absolute(&site.link("/blog/"))),
        esc(&site.absolute(&site.link("feed.xml"))),
        esc(&site.absolute(&site.link("/blog/"))),
    );
    for post in posts {
        let url = site.absolute(&site.link(&format!("/blog/{}/", post.slug)));
        let _ = write!(
            xml,
            "<entry>\n<title>{}</title>\n<link href=\"{}\"/>\n<id>{}</id>\n\
             <updated>{}T00:00:00Z</updated>\n<author><name>Vibe Download Manager</name></author>\n\
             <summary>{}</summary>\n<content type=\"html\">{}</content>\n</entry>\n",
            esc(&post.meta.title),
            esc(&url),
            esc(&url),
            post.meta.date,
            esc(&post.meta.description),
            esc(&site.expand(&post.html)),
        );
    }
    xml.push_str("</feed>\n");
    xml
}

fn sitemap_xml(site: &Site, urls: &[(String, String)]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    for (path, lastmod) in urls {
        let _ = write!(xml, "<url><loc>{}</loc>", esc(&site.absolute(&site.link(path))));
        if !lastmod.is_empty() {
            let _ = write!(xml, "<lastmod>{lastmod}</lastmod>");
        }
        xml.push_str("</url>\n");
    }
    xml.push_str("</urlset>\n");
    xml
}

// ---------------------------------------------------------------- helpers

fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn sorted_entries(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .collect();
    files.sort();
    Ok(files)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// "https://u.github.io/repo/" -> ("https://u.github.io", "/repo/").
fn split_base_url(base: &str) -> (String, String) {
    let base = base.trim_end_matches('/');
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    match rest.split_once('/') {
        Some((host, path)) => {
            (format!("{scheme}://{host}"), format!("/{}/", path.trim_matches('/')))
        }
        None => (format!("{scheme}://{rest}"), "/".to_owned()),
    }
}

fn workspace_version(root: &Path) -> Result<String> {
    let text = fs::read_to_string(root.join("Cargo.toml"))?;
    text.lines()
        .find_map(|l| l.strip_prefix("version = \"").and_then(|v| v.strip_suffix('"')))
        .map(str::to_owned)
        .context("no workspace version")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(base: &str) -> Site {
        let (origin, root) = split_base_url(base);
        Site {
            origin,
            root,
            repo: "me/repo".to_owned(),
            version: "1.2.3".to_owned(),
            year: "2026".to_owned(),
        }
    }

    #[test]
    fn front_matter_splits_meta_and_body() {
        let text = "+++\ntitle = \"T\"\ndescription = \"D\"\ndate = \"2026-01-02\"\n+++\n\n# Body";
        let (meta, body): (PostMeta, &str) = front_matter(text).unwrap();
        assert_eq!(meta.title, "T");
        assert_eq!(body, "# Body");
        assert!(front_matter::<PostMeta>("no front matter").is_err());
    }

    #[test]
    fn headings_get_ids() {
        let html = markdown("## How it works, really?\n\nText\n\n### A `code` bit");
        assert!(
            html.contains("<h2 id=\"how-it-works-really\">How it works, really?</h2>"),
            "{html}"
        );
        assert!(html.contains("<h3 id=\"a-code-bit\">"), "{html}");
        let links = markdown("[a](/blog/x/) [b](@github/issues) [c](https://e.com/)");
        assert!(links.contains("href=\"{{root}}blog/x/\""), "{links}");
        assert!(links.contains("href=\"{{github}}/issues\""), "{links}");
        assert!(links.contains("href=\"https://e.com/\""), "{links}");
    }

    #[test]
    fn placeholders_and_links() {
        let s = site("https://me.github.io/repo");
        assert_eq!(s.link("/blog/"), "/repo/blog/");
        assert_eq!(s.absolute(&s.link("/")), "https://me.github.io/repo/");
        assert_eq!(s.expand("{{github}} {{version}}"), "https://github.com/me/repo 1.2.3");
    }

    #[test]
    fn faq_schema_reads_details_blocks() {
        let ld = faq_json_ld(
            "<details><summary>Is it free?</summary><p>Yes, <b>always</b>.</p></details>",
        );
        assert!(ld.contains("\"name\":\"Is it free?\""), "{ld}");
        assert!(ld.contains("\"text\":\"Yes, always.\""), "{ld}");
    }

    #[test]
    fn sitemap_and_feed_escape() {
        let s = site("https://me.github.io/repo");
        let xml = sitemap_xml(&s, &[("/a&b/".to_owned(), "2026-01-01".to_owned())]);
        assert!(xml.contains(
            "<loc>https://me.github.io/repo/a&amp;b/</loc><lastmod>2026-01-01</lastmod>"
        ));
        let post = Post {
            slug: "x".into(),
            meta: PostMeta {
                title: "A < B".into(),
                description: "d".into(),
                date: "2026-01-02".into(),
                tags: vec![],
            },
            html: "<p>hi</p>".into(),
            words: 1,
        };
        let feed = feed_xml(&s, &[post]);
        assert!(feed.contains("<title>A &lt; B</title>"));
        assert!(feed.contains("&lt;p&gt;hi&lt;/p&gt;"));
    }

    #[test]
    fn titles_fit_search_results() {
        assert_eq!(full_title("FAQ", false), "FAQ | Vibe Download Manager");
        assert_eq!(full_title("Download Vibe for Windows", false), "Download Vibe for Windows");
        let long = full_title("How multi-connection downloads work, and why", false);
        assert!(long.ends_with(" | Vibe"), "{long}");
        assert_eq!(full_title("Anything", true), "Anything");
    }

    #[test]
    fn base_urls() {
        assert_eq!(
            split_base_url("https://u.github.io/repo/"),
            ("https://u.github.io".into(), "/repo/".into())
        );
        assert_eq!(
            split_base_url("http://localhost:8000"),
            ("http://localhost:8000".into(), "/".into())
        );
        assert_eq!(
            split_base_url("https://vibe.example"),
            ("https://vibe.example".into(), "/".into())
        );
    }

    #[test]
    fn small_helpers() {
        assert_eq!(slug("Résumé & more!"), "résumé-more");
        assert_eq!(human_date("2026-10-24"), "October 24, 2026");
        assert_eq!(reading_minutes(10), 1);
        assert_eq!(reading_minutes(661), 4);
        assert_eq!(esc("<a href=\"x\">&</a>"), "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;");
    }
}
