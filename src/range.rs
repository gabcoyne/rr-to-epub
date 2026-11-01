/// Which slice of a fiction to put in the EPUB.
///
/// Bounds accept:
/// - a 1-based table-of-contents index (`672`, `index:672`, `#672`)
/// - a Royal Road chapter id (`id:3402749`, or a bare id that is not itself a valid index)
/// - a chapter URL (`https://www.royalroad.com/fiction/33844/.../chapter/3402749/...`)
///
/// The slice is a contiguous TOC range, so interludes and side chapters that sit
/// between the bounds are included in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeRequest {
    pub from: Option<String>,
    pub to: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRange {
    /// Inclusive index into the full TOC.
    pub start: usize,
    /// Inclusive index into the full TOC.
    pub end: usize,
    /// The caller did not pin `--to`, so the end is "latest" and should grow on update.
    pub end_is_latest: bool,
}

impl RangeRequest {
    pub fn resolve(&self, chapter_ids: &[u32]) -> eyre::Result<ResolvedRange> {
        if chapter_ids.is_empty() {
            eyre::bail!("Fiction has no chapters");
        }
        if self.from.is_some() && self.after.is_some() {
            eyre::bail!("Pass only one of --from and --after");
        }

        let start = if let Some(spec) = &self.after {
            let index = locate(chapter_ids, spec)?;
            if index + 1 >= chapter_ids.len() {
                eyre::bail!("--after {spec} is the last chapter; nothing follows it");
            }
            index + 1
        } else if let Some(spec) = &self.from {
            locate(chapter_ids, spec)?
        } else {
            0
        };

        let (end, end_is_latest) = if let Some(spec) = &self.to {
            (locate(chapter_ids, spec)?, false)
        } else {
            (chapter_ids.len() - 1, true)
        };

        if start > end {
            eyre::bail!(
                "Chapter range is empty (TOC {} starts after TOC {})",
                start + 1,
                end + 1
            );
        }

        Ok(ResolvedRange {
            start,
            end,
            end_is_latest,
        })
    }
}

/// Title suffix such as `The Runesmith (Ch 672-723)`.
pub fn ranged_title(base: &str, start_toc: usize, end_toc: usize) -> String {
    let base = base.split(" (Ch ").next().unwrap_or(base).trim();
    if start_toc == end_toc {
        format!("{base} (Ch {start_toc})")
    } else {
        format!("{base} (Ch {start_toc}-{end_toc})")
    }
}

fn locate(chapter_ids: &[u32], spec: &str) -> eyre::Result<usize> {
    let spec = spec.trim();
    if spec.is_empty() {
        eyre::bail!("Empty chapter bound");
    }

    if let Some(id) = chapter_id_from_url(spec) {
        return find_id(chapter_ids, id, spec);
    }

    if let Some(rest) = strip_prefix_ci(spec, "id:") {
        let id = parse_u32(rest, spec)?;
        return find_id(chapter_ids, id, spec);
    }

    if let Some(rest) = strip_prefix_ci(spec, "index:").or_else(|| spec.strip_prefix('#')) {
        return toc_index(chapter_ids, rest.trim());
    }

    if spec.chars().all(|c| c.is_ascii_digit()) {
        let number: usize = spec
            .parse()
            .map_err(|_| eyre::eyre!("Invalid chapter bound '{spec}'"))?;
        if (1..=chapter_ids.len()).contains(&number) {
            return Ok(number - 1);
        }
        if let Ok(id) = u32::try_from(number) {
            if let Some(index) = chapter_ids.iter().position(|chapter_id| *chapter_id == id) {
                return Ok(index);
            }
        }
        eyre::bail!(
            "Chapter bound {spec} is outside TOC 1..={} and is not a known chapter id. \
             Use id:CHAPTER for a Royal Road chapter id.",
            chapter_ids.len()
        );
    }

    eyre::bail!(
        "Unrecognized chapter bound '{spec}'. Use a TOC index (672), id:CHAPTER, index:N, or a chapter URL."
    );
}

fn find_id(chapter_ids: &[u32], id: u32, spec: &str) -> eyre::Result<usize> {
    chapter_ids
        .iter()
        .position(|chapter_id| *chapter_id == id)
        .ok_or_else(|| eyre::eyre!("No chapter with Royal Road id {id} (from '{spec}')"))
}

fn toc_index(chapter_ids: &[u32], spec: &str) -> eyre::Result<usize> {
    let number: usize = spec
        .parse()
        .map_err(|_| eyre::eyre!("Invalid TOC index '{spec}'"))?;
    if number == 0 || number > chapter_ids.len() {
        eyre::bail!("TOC index {number} is outside 1..={}", chapter_ids.len());
    }
    Ok(number - 1)
}

fn parse_u32(spec: &str, original: &str) -> eyre::Result<u32> {
    spec.trim()
        .parse()
        .map_err(|_| eyre::eyre!("Invalid chapter id '{original}'"))
}

fn strip_prefix_ci<'a>(spec: &'a str, prefix: &str) -> Option<&'a str> {
    if spec.len() >= prefix.len() && spec[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&spec[prefix.len()..])
    } else {
        None
    }
}

fn chapter_id_from_url(spec: &str) -> Option<u32> {
    let marker = "/chapter/";
    let index = spec.find(marker)?;
    let rest = &spec[index + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{ranged_title, RangeRequest};

    fn ids() -> Vec<u32> {
        vec![526587, 100, 3402749, 3500000, 4270013]
    }

    #[test]
    fn toc_slice_keeps_entries_between_the_bounds() {
        // Index 2 through 4 covers three TOC rows, including whatever sits in the middle.
        let range = RangeRequest {
            from: Some("2".to_string()),
            to: Some("4".to_string()),
            after: None,
        };
        let resolved = range.resolve(&ids()).unwrap();
        assert_eq!(resolved.start, 1);
        assert_eq!(resolved.end, 3);
        assert!(!resolved.end_is_latest);
        let slice: Vec<u32> = ids()[resolved.start..=resolved.end].to_vec();
        assert_eq!(slice, vec![100, 3402749, 3500000]);
    }

    #[test]
    fn after_is_exclusive_and_runs_to_latest() {
        let range = RangeRequest {
            from: None,
            to: None,
            after: Some("2".to_string()),
        };
        let resolved = range.resolve(&ids()).unwrap();
        assert_eq!(resolved.start, 2);
        assert_eq!(resolved.end, 4);
        assert!(resolved.end_is_latest);
    }

    #[test]
    fn chapter_url_and_explicit_id() {
        let range = RangeRequest {
            from: Some(
                "https://www.royalroad.com/fiction/33844/the-runesmith/chapter/3402749/chapter-672"
                    .to_string(),
            ),
            to: Some("id:4270013".to_string()),
            after: None,
        };
        let resolved = range.resolve(&ids()).unwrap();
        assert_eq!((resolved.start, resolved.end), (2, 4));
    }

    #[test]
    fn bare_number_larger_than_the_toc_is_a_chapter_id() {
        let range = RangeRequest {
            from: Some("3402749".to_string()),
            to: None,
            after: None,
        };
        let resolved = range.resolve(&ids()).unwrap();
        assert_eq!(resolved.start, 2);
        assert!(resolved.end_is_latest);
    }

    #[test]
    fn index_prefix_is_not_confused_with_a_chapter_id() {
        let range = RangeRequest {
            from: Some("index:1".to_string()),
            to: Some("#2".to_string()),
            after: None,
        };
        let resolved = range.resolve(&ids()).unwrap();
        assert_eq!((resolved.start, resolved.end), (0, 1));
    }

    #[test]
    fn rejects_an_empty_reversed_range() {
        let range = RangeRequest {
            from: Some("4".to_string()),
            to: Some("2".to_string()),
            after: None,
        };
        assert!(range.resolve(&ids()).is_err());
    }

    #[test]
    fn title_reflects_the_toc_bounds() {
        assert_eq!(
            ranged_title("The Runesmith", 672, 723),
            "The Runesmith (Ch 672-723)"
        );
        assert_eq!(
            ranged_title("The Runesmith", 672, 672),
            "The Runesmith (Ch 672)"
        );
        assert_eq!(
            ranged_title("The Runesmith (Ch 1-10)", 672, 723),
            "The Runesmith (Ch 672-723)"
        );
    }
}
