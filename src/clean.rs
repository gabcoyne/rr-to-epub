use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Selector};

static HASH_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<span class="[a-zA-Z0-9]{30,}">.*?</span>"#).expect("span regex")
});

static STYLE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style\b[^>]*>(.*?)</style>").expect("style regex"));

static STYLE_RULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)\.([A-Za-z0-9_-]+)\s*\{([^}]*)\}").expect("rule regex"));

static FONT_ZERO_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `font-size: 0px` is hidden. `font-size: 0.9em` does not match because a
    // unit has to follow the 0 immediately.
    Regex::new(r"font-size\s*:\s*0(?:px|pt|em|%)").expect("font-size regex")
});

/// Classes Royal Road hides with `display: none` / `speak: never`.
///
/// Real paragraphs also carry a long random class, so the class *shape* is not
/// enough. Only classes the page stylesheet actually hides are collected, plus
/// any 30+ character alphanumeric class that is `display: none`.
pub fn hidden_classes(document: &str) -> HashSet<String> {
    let mut classes = HashSet::new();
    for style in STYLE_BLOCK_RE.captures_iter(document) {
        for rule in STYLE_RULE_RE.captures_iter(&style[1]) {
            let class = rule[1].to_string();
            let body = rule[2].to_ascii_lowercase();
            let speak_never = body.contains("speak") && body.contains("never");
            let display_none = body.contains("display") && DISPLAY_NONE_RE.is_match(&body);
            let hashish = class.len() >= 30 && class.chars().all(|c| c.is_ascii_alphanumeric());
            if speak_never || (display_none && hashish) {
                classes.insert(class);
            }
        }
    }
    classes
}

static DISPLAY_NONE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"display\s*:\s*none").expect("display none regex"));

/// Remove Royal Road's injected anti-piracy lines from a chapter HTML fragment.
pub fn strip_piracy(html: &str, hidden: &HashSet<String>) -> String {
    let mut cleaned = HASH_SPAN_RE.replace_all(html, "").to_string();
    for class in hidden {
        cleaned = remove_elements_with_class(&cleaned, class);
    }
    cleaned = strip_short_inline_hidden(&cleaned);
    strip_piracy_phrases(&cleaned)
}

fn remove_elements_with_class(html: &str, class: &str) -> String {
    remove_elements_where(
        html,
        |open_tag| class_list_contains(open_tag, class),
        |_| true,
    )
}

fn strip_short_inline_hidden(html: &str) -> String {
    remove_elements_where(html, is_inline_hidden_tag, |inner| {
        strip_tags(inner).chars().count() <= 400
    })
}

/// Drop elements whose opening tag matches `drop_tag`, when `drop_inner` also accepts the body.
fn remove_elements_where(
    html: &str,
    drop_tag: impl Fn(&str) -> bool,
    drop_inner: impl Fn(&str) -> bool,
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(tag) = opening_tag(after) {
            if drop_tag(tag.raw) {
                if let Some(end) = element_end(after, tag.name) {
                    let inner_start = tag.raw.len();
                    let inner_end = end.saturating_sub(format!("</{}>", tag.name).len());
                    let inner = if inner_end > inner_start {
                        &after[inner_start..inner_end]
                    } else {
                        ""
                    };
                    if drop_inner(inner) {
                        rest = &after[end..];
                        continue;
                    }
                }
            }
        }
        out.push('<');
        rest = &rest[start + 1..];
    }
    out.push_str(rest);
    out
}

struct OpeningTag<'a> {
    name: &'a str,
    raw: &'a str,
}

fn opening_tag(input: &str) -> Option<OpeningTag<'_>> {
    let body = input.strip_prefix('<')?;
    let name_len = body
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric())
        .count();
    if name_len == 0 {
        return None;
    }
    let name = &body[..name_len];
    let gt = input.find('>')?;
    Some(OpeningTag {
        name,
        raw: &input[..=gt],
    })
}

fn element_end(input: &str, name: &str) -> Option<usize> {
    let open_end = input.find('>')? + 1;
    if input[..open_end].ends_with("/>") {
        return Some(open_end);
    }
    let close = format!("</{name}>");
    let open = format!("<{name}");
    let mut depth = 1usize;
    let mut index = open_end;
    let lower = input.to_ascii_lowercase();
    let close_lower = close.to_ascii_lowercase();
    let open_lower = open.to_ascii_lowercase();
    while index < input.len() {
        if !input.is_char_boundary(index) {
            index += 1;
            continue;
        }
        if lower[index..].starts_with(&close_lower) {
            depth -= 1;
            if depth == 0 {
                return Some(index + close.len());
            }
            index += close.len();
            continue;
        }
        if lower[index..].starts_with(&open_lower) {
            let boundary = index + open_lower.len();
            let next = lower.as_bytes().get(boundary).copied();
            if next.is_none_or(|byte| !byte.is_ascii_alphanumeric()) {
                depth += 1;
            }
        }
        index += 1;
    }
    None
}

fn class_list_contains(open_tag: &str, class: &str) -> bool {
    let Some(value) = attr_value(open_tag, "class") else {
        return false;
    };
    value.split_whitespace().any(|token| token == class)
}

fn is_inline_hidden_tag(open_tag: &str) -> bool {
    let Some(style) = attr_value(open_tag, "style") else {
        return false;
    };
    let style = style.to_ascii_lowercase();
    (style.contains("display") && DISPLAY_NONE_RE.is_match(&style)) || FONT_ZERO_RE.is_match(&style)
}

fn attr_value<'a>(open_tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    let index = open_tag.find(&needle)?;
    let rest = &open_tag[index + needle.len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(&rest[..end])
}

fn strip_piracy_phrases(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    let selector = Selector::parse("p, span").expect("p, span selector");
    let mut removals = Vec::new();
    for element in fragment.select(&selector) {
        let text = element.text().collect::<String>();
        if looks_like_piracy_line(&text) {
            removals.push(element.html());
        }
    }
    let mut out = html.to_string();
    for chunk in removals {
        if let Some(pos) = out.find(&chunk) {
            out.replace_range(pos..pos + chunk.len(), "");
        }
    }
    out
}

fn looks_like_piracy_line(text: &str) -> bool {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let len = collapsed.chars().count();
    if !(20..=400).contains(&len) {
        return false;
    }
    let lower = collapsed.to_ascii_lowercase();
    const PHRASES: &[&str] = &[
        "stolen content",
        "has been stolen",
        "been unlawfully lifted",
        "not rightfully on amazon",
        "stolen versions",
        "case of theft",
        "report the violation",
        "genuine story",
    ];
    if PHRASES.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }
    lower.contains("stolen") && lower.contains("royal road")
}

fn strip_tags(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    fragment.root_element().text().collect()
}

#[cfg(test)]
mod tests {
    use super::{hidden_classes, strip_piracy};
    use std::collections::HashSet;

    const PIRACY_SPANS: &[&str] = &[
        r#"<span class="cmMxOGViZDM1ZmMxZDQyMjFhYWExZmYyNjMxZTY5OTU0"><br>You could be reading stolen content. Head to Royal Road for the genuine story.<br></span>"#,
        r#"<span class="cmMxNGY3ZDk1N2NmYTRhZDFiMjJiZGI1NzFjMzZhN2My"><br>The story has been stolen; if detected on Amazon, report the violation.<br></span>"#,
        r#"<span class="cjIzMTViZjM1ZGY5NTRmNmI4YzYxYzNmZTI5NDg0NzE1"><br>The tale has been stolen; if detected on Amazon, report the violation.<br></span>"#,
        r#"<span class="cjc5NjBlNjY4NDRjNTRmZTliNzc5ZTRjMjdlMGM2NTJi"><br>This tale has been unlawfully lifted from Royal Road; report any instances of this story if found elsewhere.<br></span>"#,
        r#"<span class="cjA1MTIwNDQzYzJjMzRjYjU5YTJjYWNjNjJkNDhhZjE5"><br>Support creative writers by reading their stories on Royal Road, not stolen versions.<br></span>"#,
    ];

    #[test]
    fn strips_hash_class_spans() {
        let hidden = HashSet::new();
        for sample in PIRACY_SPANS {
            let wrapped = format!("<p>before</p>{sample}<p>after</p>");
            assert_eq!(strip_piracy(&wrapped, &hidden), "<p>before</p><p>after</p>");
        }
    }

    #[test]
    fn keeps_real_paragraphs_that_use_a_long_class() {
        let hidden = HashSet::new();
        let html = r#"<p class="cnNkYWMwMzBkOGYxODRiZDJhYzA1MmM2NTNjMmE3Yjk4">Hello <em>italic</em> and <strong>bold</strong>.</p>"#;
        assert_eq!(strip_piracy(html, &hidden), html);
    }

    #[test]
    fn strips_stylesheet_hidden_paragraphs_only() {
        let class = "cjFmMDUwYWM0NzExZjQ2Yjg4MThhZDk3NGY2OGNjNDFh";
        let document = format!(
            r#"<style>.{class} {{ display: none; speak: never; }} .visible {{ color: black; }}</style>"#
        );
        let hidden = hidden_classes(&document);
        assert!(hidden.contains(class));
        assert!(!hidden.contains("visible"));

        let html = format!(
            r#"<p class="cnNkYWMwMzBkOGYxODRiZDJhYzA1MmM2NTNjMmE3Yjk4">Hello <em>italic</em> and <strong>bold</strong> and <i>slant</i> and <b>weight</b>.</p><p class="{class}">A case of theft: this story is not rightfully on Amazon; if you spot it, report the violation.</p><p>After.</p>"#
        );
        let cleaned = strip_piracy(&html, &hidden);
        assert!(cleaned.contains("<em>italic</em>"));
        assert!(cleaned.contains("<strong>bold</strong>"));
        assert!(cleaned.contains("<i>slant</i>"));
        assert!(cleaned.contains("<b>weight</b>"));
        assert!(cleaned.contains("<p>After.</p>"));
        assert!(!cleaned.contains("case of theft"));
        assert!(!cleaned.contains(class));
    }

    #[test]
    fn keeps_a_long_paragraph_that_mentions_theft_in_the_story() {
        let hidden = HashSet::new();
        let story = "a".repeat(450);
        let html = format!("<p>The stolen relic on amazon mattered. {story}</p>");
        assert_eq!(strip_piracy(&html, &hidden), html);
    }

    #[test]
    fn strips_a_short_inline_hidden_line() {
        let hidden = HashSet::new();
        let html = r#"<p>Keep.</p><p style="display: none">You could be reading stolen content. Head to Royal Road for the genuine story.</p><p>Stay.</p>"#;
        let cleaned = strip_piracy(html, &hidden);
        assert!(cleaned.contains("<p>Keep.</p>"));
        assert!(cleaned.contains("<p>Stay.</p>"));
        assert!(!cleaned.contains("stolen content"));
    }
}
