//! Turn a Royal Road chapter page into XHTML-ish fragments.
//!
//! Paragraphs keep their inline markup (`<p>`, `<em>`, `<i>`, `<strong>`, `<b>`).
//! Author-note portlets are kept separately so the EPUB can mark them as notes.
//! Royal Road injects anti-theft lines (a random class hidden with
//! `display: none` / `speak: never`, or a short paragraph of warning text).
//! Those nodes are removed. Real paragraphs also carry random classes and must
//! stay.

use std::collections::HashSet;

use ego_tree::NodeId;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

const THEFT_MARKERS: &[&str] = &[
    "stolen from royal road",
    "stolen from its original source",
    "unlawfully lifted",
    "narrative has been purloined",
    "tale has been pilfered",
    "has been misappropriated",
    "official page without consent",
    "without the author's approval",
    "this story is from royal road",
    "read the official version for free",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedChapter {
    pub content: String,
    pub note_start: Option<String>,
    pub note_end: Option<String>,
}

pub fn clean_chapter_html(page: &str) -> eyre::Result<CleanedChapter> {
    let hidden = hidden_classes(page);
    let mut document = Html::parse_document(page);
    let content_sel = Selector::parse(".chapter-inner.chapter-content").unwrap();
    let portlet_sel = Selector::parse(".author-note-portlet").unwrap();

    let content_id = document
        .select(&content_sel)
        .next()
        .map(|el| el.id())
        .ok_or_else(|| eyre::eyre!("No chapter content found"))?;
    let note_ids: HashSet<NodeId> = document.select(&portlet_sel).map(|el| el.id()).collect();

    remove_unwanted(&mut document, &hidden, content_id, &note_ids);

    let content = document
        .select(&content_sel)
        .next()
        .map(|el| strip_dangerous(&el.inner_html()))
        .ok_or_else(|| eyre::eyre!("No chapter content found"))?;

    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let portlets: Vec<NodeId> = document.select(&portlet_sel).map(|el| el.id()).collect();
    for id in portlets {
        let Some(portlet) = document.tree.get(id).and_then(ElementRef::wrap) else {
            continue;
        };
        let rendered = render_note(portlet);
        if rendered.trim().is_empty() {
            continue;
        }
        if is_before(&document, id, content_id) {
            starts.push(rendered);
        } else {
            ends.push(rendered);
        }
    }

    Ok(CleanedChapter {
        content,
        note_start: join_notes(starts),
        note_end: join_notes(ends),
    })
}

pub(crate) fn contains_theft_marker(html: &str) -> bool {
    let lower = html.to_lowercase();
    THEFT_MARKERS.iter().any(|marker| lower.contains(marker))
}

fn join_notes(notes: Vec<String>) -> Option<String> {
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("<hr class=\"authors-note-separator\"/>"))
    }
}

fn render_note(portlet: ElementRef<'_>) -> String {
    let caption_sel = Selector::parse(".caption-subject").unwrap();
    let body_sel = Selector::parse(".author-note").unwrap();
    let caption = portlet
        .select(&caption_sel)
        .next()
        .map(|el| collapse_ws(&el.text().collect::<String>()))
        .unwrap_or_default();
    let body = portlet
        .select(&body_sel)
        .next()
        .map(|el| el.inner_html())
        .unwrap_or_default();
    if caption.is_empty() && body.trim().is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if !caption.is_empty() {
        out.push_str("<p class=\"authors-note-caption\">");
        out.push_str(&escape_text(&caption));
        out.push_str("</p>");
    }
    out.push_str(body.trim());
    strip_dangerous(&out)
}

fn remove_unwanted(
    document: &mut Html,
    hidden: &HashSet<String>,
    content_id: NodeId,
    note_ids: &HashSet<NodeId>,
) {
    let mut phrase_roots = note_ids.clone();
    phrase_roots.insert(content_id);

    let mut doomed = Vec::new();
    for element in document.select(&Selector::parse("*").unwrap()) {
        if should_remove(
            document,
            element,
            hidden,
            content_id,
            note_ids,
            &phrase_roots,
        ) {
            doomed.push(element.id());
        }
    }
    let doomed_set: HashSet<NodeId> = doomed.iter().copied().collect();
    let roots: Vec<NodeId> = doomed
        .into_iter()
        .filter(|id| !ancestor_in_set(document, *id, &doomed_set))
        .collect();
    for id in roots {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

fn ancestor_in_set(document: &Html, id: NodeId, doomed: &HashSet<NodeId>) -> bool {
    let mut parent = document.tree.get(id).and_then(|node| node.parent());
    while let Some(node) = parent {
        if doomed.contains(&node.id()) {
            return true;
        }
        parent = node.parent();
    }
    false
}

fn should_remove(
    document: &Html,
    element: ElementRef<'_>,
    hidden: &HashSet<String>,
    content_id: NodeId,
    note_ids: &HashSet<NodeId>,
    phrase_roots: &HashSet<NodeId>,
) -> bool {
    let name = element.value().name();
    if matches!(name, "html" | "body" | "head" | "style" | "script") {
        return false;
    }
    let id = element.id();
    if id == content_id || note_ids.contains(&id) {
        return false;
    }
    if element
        .value()
        .classes()
        .any(|class| hidden.contains(class))
    {
        return true;
    }
    if element.value().attr("style").is_some_and(style_is_hidden) {
        return true;
    }
    // Only the injected line itself, not a parent that also holds real prose.
    // Royal Road's line is a span (sometimes with <br>) whose whole text is the warning.
    if within_any(document, id, phrase_roots) && is_theft_leaf(element) {
        return true;
    }
    false
}

fn is_theft_leaf(element: ElementRef<'_>) -> bool {
    let leafish = element
        .children()
        .all(|child| match ElementRef::wrap(child) {
            None => true,
            Some(nested) => nested.value().name() == "br",
        });
    if !leafish {
        return false;
    }
    let text = collapse_ws(&element.text().collect::<String>());
    text.len() <= 500 && contains_theft_marker(&text)
}

fn within_any(document: &Html, id: NodeId, ancestors: &HashSet<NodeId>) -> bool {
    let mut current = document.tree.get(id);
    while let Some(node) = current {
        if ancestors.contains(&node.id()) {
            return true;
        }
        current = node.parent();
    }
    false
}

fn is_before(document: &Html, a: NodeId, b: NodeId) -> bool {
    let mut seen_a = false;
    for node in document.tree.root().descendants() {
        if node.id() == a {
            seen_a = true;
        }
        if node.id() == b {
            return seen_a;
        }
    }
    false
}

fn hidden_classes(page: &str) -> HashSet<String> {
    let mut classes = HashSet::new();
    let style_re = Regex::new(r"(?is)<style\b[^>]*>(.*?)</style>").unwrap();
    let block_re = Regex::new(r"(?s)([^{}]+)\{([^}]*)\}").unwrap();
    let class_re = Regex::new(r"\.([A-Za-z_][A-Za-z0-9_-]*)").unwrap();
    for style in style_re.captures_iter(page) {
        for block in block_re.captures_iter(&style[1]) {
            if !rule_hides(&block[2]) {
                continue;
            }
            for class in class_re.captures_iter(&block[1]) {
                classes.insert(class[1].to_string());
            }
        }
    }
    classes
}

fn rule_hides(body: &str) -> bool {
    let compact: String = body
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    compact.contains("display:none")
        || compact.contains("visibility:hidden")
        || compact.contains("speak:never")
        || compact.contains("font-size:0")
        || compact.contains("opacity:0")
}

fn style_is_hidden(style: &str) -> bool {
    rule_hides(style)
}

fn strip_dangerous(html: &str) -> String {
    let script = Regex::new(r"(?is)<script\b[^>]*>.*?</script>").unwrap();
    let iframe = Regex::new(r"(?is)<iframe\b[^>]*>.*?</iframe>").unwrap();
    let without_script = script.replace_all(html, "");
    iframe.replace_all(&without_script, "").into_owned()
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::clean_chapter_html;

    fn page() -> String {
        r#"
        <html><head><style>
            .hideme { display: none; speak: never; }
            .alsogone { visibility: hidden; }
        </style></head><body>
        <div class="portlet solid author-note-portlet">
          <div class="portlet-title"><div class="caption">
            <span class="caption-subject bold uppercase">A note from Kuropon</span>
          </div></div>
          <div class="portlet-body author-note"><p>Thanks for reading.</p>
            <span class="hideme">official page without consent</span>
          </div>
        </div>
        <div class="chapter-inner chapter-content">
          <p class="cnREAL"><span style="font-weight: 400">Dawn broke over <em>Albrook</em>.</span></p>
          <p>He spoke <strong>boldly</strong> and <i>quietly</i>.</p>
          <span class="hideme"><br/>A straightforward fact: this copy came from the author's official page without consent.<br/></span>
          <span class="alsogone">SECRET_HIDDEN_LINE</span>
          <span>Stolen from Royal Road, this story should be reported.</span>
          <div style="max-width: 100%; overflow:auto"><table><tr><td><strong>Name:</strong></td><td>Millie</td></tr></table></div>
        </div>
        <div class="portlet solid author-note-portlet">
          <div class="portlet-body author-note"><p>End note.</p></div>
        </div>
        </body></html>
        "#
        .to_string()
    }

    #[test]
    fn keeps_formatting_and_strips_theft_and_splits_notes() {
        let cleaned = clean_chapter_html(&page()).unwrap();
        assert!(cleaned.content.contains("<em>Albrook</em>"));
        assert!(cleaned.content.contains("<strong>boldly</strong>"));
        assert!(cleaned.content.contains("<i>quietly</i>"));
        assert!(cleaned.content.contains("<strong>Name:</strong>"));
        assert!(cleaned.content.contains("<table>"));
        assert!(cleaned.content.contains("Dawn broke"));
        assert!(!cleaned.content.to_lowercase().contains("without consent"));
        assert!(!cleaned.content.contains("SECRET_HIDDEN_LINE"));
        assert!(!cleaned
            .content
            .to_lowercase()
            .contains("stolen from royal road"));
        let start = cleaned.note_start.expect("start note");
        assert!(start.contains("A note from Kuropon"));
        assert!(start.contains("Thanks for reading."));
        assert!(!start.to_lowercase().contains("without consent"));
        let end = cleaned.note_end.expect("end note");
        assert!(end.contains("End note."));
    }
}
