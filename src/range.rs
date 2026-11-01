//! Chapter range selection.
//!
//! A bare number matches a TOC title such as "Chapter 672", ignoring leading
//! or extra spaces. If nothing matches, it is a 1-based table-of-contents
//! index. `index:N`, `id:N`, a chapter URL, and `latest` are explicit.
//! The result is always a contiguous TOC slice, so interludes between the
//! bounds stay in order.

use crate::epub::Chapter;
use eyre::WrapErr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChapterQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub after: Option<String>,
}

impl ChapterQuery {
    pub fn is_ranged(&self) -> bool {
        self.from.is_some() || self.to.is_some() || self.after.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Bound {
    Latest,
    /// 1-based TOC index.
    Index1(usize),
    Id(u32),
    Url(String),
    /// Chapter title number, with TOC-index fallback.
    Number(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
    pub label: String,
}

pub fn select_range(chapters: &[Chapter], query: &ChapterQuery) -> eyre::Result<Selection> {
    if chapters.is_empty() {
        eyre::bail!("Fiction has no chapters");
    }
    if query.from.is_some() && query.after.is_some() {
        eyre::bail!("Use only one of --from and --after");
    }

    let start = if let Some(after) = &query.after {
        let index = resolve(chapters, &parse_bound(after)?)?;
        if index + 1 >= chapters.len() {
            eyre::bail!("Nothing to download after '{}'", chapters[index].title);
        }
        index + 1
    } else if let Some(from) = &query.from {
        resolve(chapters, &parse_bound(from)?)?
    } else {
        0
    };
    let end = if let Some(to) = &query.to {
        resolve(chapters, &parse_bound(to)?)?
    } else {
        chapters.len() - 1
    };
    if start > end {
        eyre::bail!(
            "Chapter range is empty: '{}' comes after '{}'",
            chapters[start].title.trim(),
            chapters[end].title.trim()
        );
    }
    Ok(Selection {
        start,
        end,
        label: range_label(chapters, start, end),
    })
}

fn parse_bound(raw: &str) -> eyre::Result<Bound> {
    let spec = raw.trim();
    if spec.is_empty() {
        eyre::bail!("Empty chapter selector");
    }
    if spec.eq_ignore_ascii_case("latest") {
        return Ok(Bound::Latest);
    }
    if let Some(rest) = strip_prefix_ignore_ascii(spec, "index:") {
        let index: usize = rest
            .trim()
            .parse()
            .wrap_err("TOC index must be a positive whole number")?;
        if index == 0 {
            eyre::bail!("TOC indexes are 1-based");
        }
        return Ok(Bound::Index1(index));
    }
    if let Some(rest) = strip_prefix_ignore_ascii(spec, "id:") {
        let id: u32 = rest
            .trim()
            .parse()
            .wrap_err("chapter id must be a positive whole number")?;
        return Ok(Bound::Id(id));
    }
    if spec.contains("/chapter/") || spec.starts_with("http://") || spec.starts_with("https://") {
        return Ok(Bound::Url(spec.to_string()));
    }
    if let Ok(number) = spec.parse::<u32>() {
        if number == 0 {
            eyre::bail!("Chapter numbers start at 1");
        }
        return Ok(Bound::Number(number));
    }
    eyre::bail!(
        "Unrecognized chapter selector '{spec}'. Use a chapter number, index:N, id:N, a chapter URL, or latest."
    );
}

fn resolve(chapters: &[Chapter], bound: &Bound) -> eyre::Result<usize> {
    match bound {
        Bound::Latest => Ok(chapters.len() - 1),
        Bound::Index1(index) => {
            if *index == 0 || *index > chapters.len() {
                eyre::bail!(
                    "TOC index {index} is outside 1-{} ({})",
                    chapters.len(),
                    chapters.len()
                );
            }
            Ok(index - 1)
        }
        Bound::Id(id) => chapters
            .iter()
            .position(|chapter| chapter.id == *id)
            .ok_or_else(|| eyre::eyre!("No chapter with Royal Road id {id}")),
        Bound::Url(url) => {
            let id = chapter_id_from_url(url)
                .ok_or_else(|| eyre::eyre!("No chapter id in URL '{url}'"))?;
            chapters
                .iter()
                .position(|chapter| chapter.id == id)
                .ok_or_else(|| eyre::eyre!("No chapter with Royal Road id {id}"))
        }
        Bound::Number(number) => {
            let hits: Vec<usize> = chapters
                .iter()
                .enumerate()
                .filter(|(_, chapter)| chapter_number(&chapter.title) == Some(*number))
                .map(|(index, _)| index)
                .collect();
            if let Some(&index) = hits.first() {
                if hits.len() > 1 {
                    tracing::warn!(
                        "Multiple chapters match Chapter {number}; using the first, '{}'",
                        chapters[index].title
                    );
                }
                return Ok(index);
            }
            let index = *number as usize;
            if (1..=chapters.len()).contains(&index) {
                tracing::info!(
                    "No chapter titled \"Chapter {number}\"; using 1-based TOC index {index} ('{}')",
                    chapters[index - 1].title
                );
                return Ok(index - 1);
            }
            eyre::bail!(
                "No chapter titled \"Chapter {number}\" and TOC index {number} is outside 1-{}",
                chapters.len()
            );
        }
    }
}

fn range_label(chapters: &[Chapter], start: usize, end: usize) -> String {
    if start == end {
        if let Some(number) = chapter_number(&chapters[start].title) {
            return format!("Ch {number}");
        }
        return format!("Ch {}", start + 1);
    }
    match (
        chapter_number(&chapters[start].title),
        chapter_number(&chapters[end].title),
    ) {
        (Some(from), Some(to)) => format!("Ch {from}-{to}"),
        _ => format!("Ch {}-{}", start + 1, end + 1),
    }
}

/// First "Chapter N" in a TOC title. Leading spaces and a doubled space
/// (`Chapter  325`) still match. Titles that are not numbered return None.
pub(crate) fn chapter_number(title: &str) -> Option<u32> {
    let re = regex::Regex::new(r"(?i)(?:^|[^a-z])chapter\s+(\d+)").unwrap();
    re.captures(title)
        .and_then(|caps| caps.get(1))
        .and_then(|number| number.as_str().parse().ok())
}

fn chapter_id_from_url(url: &str) -> Option<u32> {
    let marker = "/chapter/";
    let index = url.find(marker)?;
    let rest = &url[index + marker.len()..];
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn strip_prefix_ignore_ascii<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() < prefix.len() {
        return None;
    }
    let (head, tail) = value.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        Some(tail)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{select_range, ChapterQuery};
    use crate::epub::Chapter;

    fn chapter(id: u32, title: &str) -> Chapter {
        Chapter {
            id,
            title: title.to_string(),
            url: format!("/fiction/1/slug/chapter/{id}/slug"),
            ..Chapter::default()
        }
    }

    fn sample() -> Vec<Chapter> {
        vec![
            chapter(10, "Book 1 Summary for RR"),
            chapter(11, "Chapter 1: A"),
            chapter(12, "Interlude: Side Story"),
            chapter(13, " Chapter 2: B"),
            chapter(14, "Chapter  3: C"),
        ]
    }

    fn query(from: Option<&str>, to: Option<&str>, after: Option<&str>) -> ChapterQuery {
        ChapterQuery {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
            after: after.map(str::to_string),
        }
    }

    #[test]
    fn title_numbers_include_interludes_and_leading_space() {
        let chapters = sample();
        let selection = select_range(&chapters, &query(Some("1"), Some("2"), None)).unwrap();
        assert_eq!(selection.start, 1);
        assert_eq!(selection.end, 3);
        assert_eq!(selection.label, "Ch 1-2");
        let titles: Vec<_> = chapters[selection.start..=selection.end]
            .iter()
            .map(|chapter| chapter.title.as_str())
            .collect();
        assert_eq!(
            titles,
            vec!["Chapter 1: A", "Interlude: Side Story", " Chapter 2: B"]
        );
    }

    #[test]
    fn double_space_chapter_number_matches() {
        let chapters = sample();
        let selection = select_range(&chapters, &query(Some("3"), Some("latest"), None)).unwrap();
        assert_eq!(selection.start, 4);
        assert_eq!(selection.end, 4);
        assert_eq!(selection.label, "Ch 3");
    }

    #[test]
    fn after_is_exclusive_and_index_id_and_url_work() {
        let chapters = sample();
        let after = select_range(&chapters, &query(None, Some("2"), Some("1"))).unwrap();
        assert_eq!(after.start, 2);
        assert_eq!(chapters[after.start].title, "Interlude: Side Story");
        assert_eq!(after.end, 3);

        let index =
            select_range(&chapters, &query(Some("index:1"), Some("index:1"), None)).unwrap();
        assert_eq!(chapters[index.start].title, "Book 1 Summary for RR");
        assert_eq!(index.label, "Ch 1");

        let by_id = select_range(&chapters, &query(Some("id:12"), Some("id:14"), None)).unwrap();
        assert_eq!(by_id.start, 2);
        assert_eq!(by_id.end, 4);

        let by_url = select_range(
            &chapters,
            &query(
                Some("https://www.royalroad.com/fiction/1/slug/chapter/13/chapter-2"),
                None,
                None,
            ),
        )
        .unwrap();
        assert_eq!(by_url.start, 3);
        assert_eq!(by_url.end, 4);
    }

    #[test]
    fn full_book_when_unbounded_and_errors_on_empty_after() {
        let chapters = sample();
        let all = select_range(&chapters, &ChapterQuery::default()).unwrap();
        assert_eq!(all.start, 0);
        assert_eq!(all.end, 4);
        assert!(!ChapterQuery::default().is_ranged());
        assert!(select_range(&chapters, &query(None, None, Some("3"))).is_err());
        assert!(select_range(&chapters, &query(Some("2"), Some("1"), None)).is_err());
    }
}
