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

    pub fn save_download_progress(book: &Book, last_processed_index: usize) -> eyre::Result<()> {
        let cache_dir = Self::cache_path()?.join(book.id.to_string());
        std::fs::create_dir_all(&cache_dir)?;
        
        let progress_file = cache_dir.join("download_progress.json");
        let progress = serde_json::to_string(&last_processed_index)?;
        std::fs::write(progress_file, progress)?;
        Ok(())
    }

    pub fn read_download_progress(book: &Book) -> eyre::Result<Option<usize>> {
        let cache_dir = Self::cache_path()?;
        let progress_file = cache_dir.join(book.id.to_string()).join("download_progress.json");
        if !progress_file.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(progress_file)?;
        let progress: usize = serde_json::from_str(&contents)?;
        Ok(Some(progress))
    }
}
