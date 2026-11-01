use std::path::PathBuf;

use bytes::Bytes;

use crate::epub::Book;

pub struct Cache;
impl Cache {
    pub fn cache_path() -> eyre::Result<PathBuf> {
        let home_dir = dirs::home_dir().ok_or(eyre::eyre!("No home directory"))?;
        let cache_dir = home_dir.join(".cache/rr-to-epub");
        std::fs::create_dir_all(&cache_dir)?;
        Ok(cache_dir)
    }
    pub fn write_book(book: &Book) -> eyre::Result<()> {
        let cache_dir = Self::cache_path()?.join(book.id.to_string());
        std::fs::create_dir_all(&cache_dir)?;

        // Write the book without the cover.
        let mut cloned_book = book.clone();
        cloned_book.cover = None;
        let cache_file = cache_dir.join("book.json");
        let contents = serde_json::to_string(&cloned_book)?;
        std::fs::write(cache_file, contents)?;
        Ok(())
    }
    pub fn read_book(id: u32) -> eyre::Result<Option<Book>> {
        let cache_dir = Self::cache_path()?;
        let cache_file = cache_dir.join(id.to_string()).join("book.json");
        if !cache_file.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(cache_file)?;
        let book: Result<Book, _> = serde_json::from_str(&contents);
        let book = match book {
            Ok(book) => book,
            Err(err) => {
                tracing::error!("Failed to parse book from cache: {:?}", err);
                return Ok(None);
            }
        };
        Ok(Some(book))
    }
    pub fn write_inline_image(book: &Book, filename: &str, image: &[u8]) -> eyre::Result<()> {
        let cache_dir = Self::cache_path()?.join(book.id.to_string()).join("images");
        std::fs::create_dir_all(&cache_dir)?;

        // Write the image to the cache.
        let cache_file = cache_dir.join(filename);
        std::fs::write(cache_file, image)?;
        Ok(())
    }
    pub fn read_inline_image(book: &Book, filename: &str) -> eyre::Result<Option<Bytes>> {
        let cache_dir = Self::cache_path()?;
        let cache_file = cache_dir
            .join(book.id.to_string())
            .join("images")
            .join(filename);
        if !cache_file.exists() {
            return Ok(None);
        }
        let contents = std::fs::read(cache_file)?;
        Ok(Some(contents.into()))
    }

    pub fn save_download_progress(book_id: u32, next: usize, ids: &[u32]) -> eyre::Result<()> {
        let cache_dir = Self::cache_path()?.join(book_id.to_string());
        std::fs::create_dir_all(&cache_dir)?;
        let progress = serde_json::json!({ "next": next, "ids": ids });
        std::fs::write(
            cache_dir.join("download_progress.json"),
            serde_json::to_string(&progress)?,
        )?;
        Ok(())
    }

    pub fn read_download_progress(book_id: u32) -> eyre::Result<Option<DownloadProgress>> {
        let progress_file = Self::cache_path()?
            .join(book_id.to_string())
            .join("download_progress.json");
        if !progress_file.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(progress_file)?;
        Ok(Some(parse_progress(&contents)?))
    }

    pub fn clear_download_progress(book_id: u32) -> eyre::Result<()> {
        let progress_file = Self::cache_path()?
            .join(book_id.to_string())
            .join("download_progress.json");
        if progress_file.exists() {
            std::fs::remove_file(progress_file)?;
        }
        Ok(())
    }
}

/// Download checkpoint. Older builds stored a bare chapter index; new builds
/// store that index together with the chapter ids being downloaded so a
/// different range does not resume at the wrong chapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadProgress {
    Legacy(usize),
    Scoped { next: usize, ids: Vec<u32> },
}

pub(crate) fn parse_progress(text: &str) -> eyre::Result<DownloadProgress> {
    let text = text.trim();
    if let Ok(next) = serde_json::from_str::<usize>(text) {
        return Ok(DownloadProgress::Legacy(next));
    }
    #[derive(serde::Deserialize)]
    struct Scoped {
        next: usize,
        ids: Vec<u32>,
    }
    let scoped: Scoped = serde_json::from_str(text)?;
    Ok(DownloadProgress::Scoped {
        next: scoped.next,
        ids: scoped.ids,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_progress, DownloadProgress};

    #[test]
    fn progress_accepts_legacy_index_and_scoped_ids() {
        assert_eq!(
            parse_progress("  42\n").unwrap(),
            DownloadProgress::Legacy(42)
        );
        assert_eq!(
            parse_progress(r#"{"next":3,"ids":[9,8]}"#).unwrap(),
            DownloadProgress::Scoped {
                next: 3,
                ids: vec![9, 8]
            }
        );
    }
}
