//! Heurystyczna ekstrakcja treści z HTML. Bez JavaScriptu, bez zewnętrznych
//! modeli i bez „readability” zależnego od sieci.
//!
//! Priorytet kontenera: `article` → `main` → `[role=main]` → `body`. Z każdego
//! kontenera bierzemy przede wszystkim akapity, listy i nagłówki, a nie cały
//! tekst (żeby nie wciągać menu, stopki i skryptów).

use scraper::{Html, Selector};
use url::Url;

const MAX_TEXT_CHARS: usize = 120_000;

#[derive(Debug, Clone, Default)]
pub struct ExtractedDocument {
    pub title: String,
    pub description: String,
    pub text: String,
    pub canonical: Option<String>,
    pub lang: Option<String>,
    pub published: Option<String>,
    pub noindex: bool,
}

pub fn extract_html(html: &str, base: &Url) -> ExtractedDocument {
    let document = Html::parse_document(html);

    let title = first_text(&document, "title")
        .or_else(|| meta_content(&document, "meta[property=\"og:title\"]"))
        .or_else(|| first_text(&document, "h1"))
        .map(|s| truncate_chars(&clean_inline(&s), 300))
        .unwrap_or_default();

    let description = meta_content(&document, "meta[name=\"description\"]")
        .or_else(|| meta_content(&document, "meta[property=\"og:description\"]"))
        .map(|s| truncate_chars(&clean_inline(&s), 500))
        .unwrap_or_default();

    let lang = document
        .select(&selector("html"))
        .next()
        .and_then(|el| el.value().attr("lang"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let canonical = document
        .select(&selector("link[rel=\"canonical\"]"))
        .next()
        .and_then(|el| el.value().attr("href"))
        .and_then(|href| base.join(href).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|u| u.to_string());

    let published = [
        "meta[property=\"article:published_time\"]",
        "meta[name=\"date\"]",
        "meta[name=\"pubdate\"]",
        "meta[itemprop=\"datePublished\"]",
        "time[datetime]",
    ]
    .iter()
    .find_map(|sel| {
        let el = document.select(&selector(sel)).next()?;
        el.value()
            .attr("content")
            .or_else(|| el.value().attr("datetime"))
            .map(|s| s.trim().to_string())
    })
    .filter(|s| !s.is_empty());

    let noindex = ["meta[name=\"robots\"]", "meta[name=\"googlebot\"]"]
        .iter()
        .filter_map(|sel| meta_content(&document, sel))
        .any(|content| {
            let content = content.to_ascii_lowercase();
            content.contains("noindex") || content.contains("none")
        });

    let mut text = String::new();
    for container in ["article", "main", "[role=\"main\"]", "body"] {
        let Some(element) = document.select(&selector(container)).next() else {
            continue;
        };
        let candidate = text_from_container(element);
        if candidate.chars().count() > text.chars().count() {
            text = candidate;
        }
        if text.chars().count() >= 400 {
            break;
        }
    }

    if text.is_empty() {
        text = clean_block(&document.root_element().text().collect::<Vec<_>>().join(" "));
    }

    ExtractedDocument {
        title,
        description,
        text: truncate_chars(&text, MAX_TEXT_CHARS),
        canonical,
        lang,
        published,
        noindex,
    }
}

pub fn extract_plain(text: &str, base: &Url) -> ExtractedDocument {
    let cleaned = clean_block(text);
    let title = cleaned
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| truncate_chars(line.trim(), 300))
        .unwrap_or_else(|| base.to_string());
    let description = truncate_chars(&cleaned, 500);

    ExtractedDocument {
        title,
        description,
        text: truncate_chars(&cleaned, MAX_TEXT_CHARS),
        canonical: None,
        lang: None,
        published: None,
        noindex: false,
    }
}

fn text_from_container(element: scraper::ElementRef<'_>) -> String {
    let blocks = selector("p, li, blockquote, h1, h2, h3, h4, h5, h6, pre, td");
    let mut lines: Vec<String> = Vec::new();

    for block in element.select(&blocks) {
        let text = clean_inline(&block.text().collect::<Vec<_>>().join(" "));
        if text.is_empty() {
            continue;
        }
        if lines.last() != Some(&text) {
            lines.push(text);
        }
    }

    if lines.is_empty() {
        clean_block(&element.text().collect::<Vec<_>>().join(" "))
    } else {
        lines.join("\n")
    }
}

fn selector(raw: &str) -> Selector {
    Selector::parse(raw).expect("static selector")
}

fn first_text(document: &Html, sel: &str) -> Option<String> {
    document
        .select(&selector(sel))
        .next()
        .map(|el| el.text().collect::<Vec<_>>().join(" "))
}

fn meta_content(document: &Html, sel: &str) -> Option<String> {
    document
        .select(&selector(sel))
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string())
}

fn clean_inline(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_block(value: &str) -> String {
    value
        .lines()
        .map(clean_inline)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://example.com/news/story").unwrap()
    }

    #[test]
    fn extracts_article_without_scripts_or_navigation() {
        let html = r#"<!doctype html>
            <html lang="pl">
              <head>
                <title>  Cichy balon — analiza  </title>
                <meta name="description" content="Opis dokumentu">
                <link rel="canonical" href="/canonical/story">
                <meta property="article:published_time" content="2024-01-02T03:04:05Z">
              </head>
              <body>
                <nav>Menu główne</nav>
                <article>
                  <h1>Cichy balon</h1>
                  <p>Pierwszy akapit z <strong>ważnym</strong> szczegółem.</p>
                  <p>Drugi akapit.</p>
                  <script>alert('nie indeksuj')</script>
                </article>
                <footer>Stopka</footer>
              </body>
            </html>"#;

        let doc = extract_html(html, &url());
        assert_eq!(doc.title, "Cichy balon — analiza");
        assert_eq!(doc.description, "Opis dokumentu");
        assert_eq!(doc.lang.as_deref(), Some("pl"));
        assert_eq!(
            doc.canonical.as_deref(),
            Some("https://example.com/canonical/story")
        );
        assert_eq!(doc.published.as_deref(), Some("2024-01-02T03:04:05Z"));
        assert!(doc.text.contains("Pierwszy akapit"));
        assert!(doc.text.contains("Drugi akapit"));
        assert!(!doc.text.contains("Menu główne"));
        assert!(!doc.text.contains("Stopka"));
        assert!(!doc.text.contains("nie indeksuj"));
    }

    #[test]
    fn extracts_plain_text() {
        let doc = extract_plain("Tytuł\n\nTreść dokumentu.", &url());
        assert_eq!(doc.title, "Tytuł");
        assert!(doc.text.contains("Treść dokumentu"));
        assert!(doc.canonical.is_none());
    }

    #[test]
    fn detects_noindex() {
        let html = r#"<html><head><title>x</title>
            <meta name="robots" content="noindex, nofollow">
            </head><body><article><p>Treść</p></article></body></html>"#;
        assert!(extract_html(html, &url()).noindex);
    }

    #[test]
    fn falls_back_to_body_text() {
        let html = r#"<html><head><title>Bez article</title></head>
            <body><div>Akapit bez znacznika p.</div></body></html>"#;
        let doc = extract_html(html, &url());
        assert!(doc.text.contains("Akapit bez znacznika p."));
    }
}
