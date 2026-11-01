use crate::cache::Cache;
use crate::clean::{hidden_classes, strip_piracy};
use crate::http::{polite_delay, HttpClient};
use crate::range::RangeRequest;
use crate::xml_ext::write_elements;
use eyre::bail;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::io::Reader;
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use url::Url;
use uuid::Uuid;
use webp::Decoder;
use xml::writer::XmlEvent;
use xml::EmitterConfig;

/// Where an existing EPUB came from, including an optional chapter slice.
#[derive(Clone, Debug, Default)]
pub struct EpubOrigin {
    pub id: Option<u32>,
    pub range: RangeRequest,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Book {
    pub id: u32,
    pub title: String,
    pub author: String,
    pub description: String,
    pub date_published: String,
    pub cover_url: String,
    pub cover: Option<Vec<u8>>,
    /// MIME type of `cover` bytes. Empty means sniff the bytes when writing.
    #[serde(default)]
    pub cover_media_type: String,
    pub chapters: Vec<Chapter>,

    /// Royal Road id of the first chapter in a partial EPUB. Unset for a full book.
    #[serde(skip)]
    pub range_start_id: Option<u32>,
    /// Royal Road id of the last chapter when the end is pinned. Unset when the end is "latest".
    #[serde(skip)]
    pub range_end_id: Option<u32>,
    /// The slice runs through whatever the latest chapter is at download time.
    #[serde(skip)]
    pub range_end_latest: bool,

    #[serde(skip)]
    client: HttpClient,
}
impl Book {
    pub fn origin_from_file(path: impl Into<PathBuf>) -> eyre::Result<EpubOrigin> {
        let mut reader = zip::ZipArchive::new(std::fs::File::open(path.into())?)?;
        let mut content_opf = reader.by_name("OEBPS/content.opf")?;
        let mut contents = String::new();
        content_opf.read_to_string(&mut contents)?;

        let mut origin = EpubOrigin::default();
        let mut start_id: Option<u32> = None;
        let mut end_id: Option<u32> = None;
        let mut end_latest = false;

        let mut parsed = xml::EventReader::new(contents.as_bytes());
        while let Ok(event) = parsed.next() {
            let xml::reader::XmlEvent::StartElement { attributes, .. } = event else {
                if matches!(event, xml::reader::XmlEvent::EndDocument) {
                    break;
                }
                continue;
            };
            let name = attributes
                .iter()
                .find(|attribute| attribute.name.local_name == "name")
                .map(|attribute| attribute.value.as_str());
            let content = attributes
                .iter()
                .find(|attribute| attribute.name.local_name == "content")
                .map(|attribute| attribute.value.as_str());
            let Some((name, content)) = name.zip(content) else {
                continue;
            };
            match name {
                "rr-to-epub:royal-road-id" => origin.id = content.parse().ok(),
                "rr-to-epub:range-start-id" => start_id = content.parse().ok(),
                "rr-to-epub:range-end-id" => end_id = content.parse().ok(),
                "rr-to-epub:range-end-latest" => end_latest = content == "true",
                _ => {}
            }
        }

        if let Some(start) = start_id {
            origin.range = RangeRequest {
                from: Some(format!("id:{start}")),
                to: if end_latest {
                    None
                } else {
                    end_id.map(|id| format!("id:{id}"))
                },
                after: None,
            };
        }
        Ok(origin)
    }

    pub async fn new(client: HttpClient, id: u32) -> eyre::Result<Self> {
        let url = format!("https://www.royalroad.com/fiction/{id}");
        let response = client.get_html(&url, None).await?;
        let parsed = Html::parse_document(&response);
        let json_ld = json_ld_book(&response);

        let title = text_of(&parsed, "h1")
            .or_else(|| json_ld_string(&json_ld, "name"))
            .ok_or_else(|| eyre::eyre!("No title found"))?;
        let author = text_of(&parsed, "h4 a")
            .or_else(|| meta_content(&parsed, "books:author"))
            .or_else(|| {
                json_ld.as_ref().and_then(|value| {
                    value
                        .get("author")
                        .and_then(|author| author.get("name"))
                        .and_then(|name| name.as_str())
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                })
            })
            .ok_or_else(|| eyre::eyre!("No author found"))?;
        let description = html_of(&parsed, ".description > .hidden-content")
            .or_else(|| json_ld_string(&json_ld, "description"))
            .ok_or_else(|| eyre::eyre!("No description found"))?;

        let cover_regex = Regex::new(r#"window\.fictionCover = "(.*)";"#).unwrap();
        let cover = cover_regex
            .captures(&response)
            .map(|captures| captures[1].to_string())
            .or_else(|| json_ld_string(&json_ld, "image"))
            .or_else(|| meta_content(&parsed, "og:image"))
            .ok_or_else(|| eyre::eyre!("No cover found"))?;

        let chapters = parse_chapters(&response)?;
        if chapters.is_empty() {
            bail!("Fiction {id} has no chapters");
        }

        Ok(Self {
            id,
            cover_url: cover,
            cover: None,
            cover_media_type: String::new(),
            title,
            author,
            description,
            date_published: chapters[0].date.clone(),
            chapters,
            range_start_id: None,
            range_end_id: None,
            range_end_latest: false,
            client,
        })
    }

    /// Download the largest cover variant that responds, without resizing it.
    pub async fn update_cover(&mut self) -> eyre::Result<()> {
        let referer = format!("https://www.royalroad.com/fiction/{}", self.id);
        let mut best: Option<(Vec<u8>, String)> = None;
        for url in cover_candidate_urls(&self.cover_url) {
            match self.client.get_bytes(&url, Some(&referer)).await {
                Ok(bytes) if sniff_image_type(&bytes).is_some() || is_webp(&bytes) => {
                    let replace = best
                        .as_ref()
                        .map(|(existing, _)| bytes.len() > existing.len())
                        .unwrap_or(true);
                    if replace {
                        tracing::info!("Cover candidate {} ({} bytes)", url, bytes.len());
                        best = Some((bytes, url));
                    }
                }
                Ok(_) => tracing::warn!("Cover candidate was not an image: {url}"),
                Err(err) => tracing::warn!("Cover candidate failed ({url}): {err}"),
            }
        }
        let (bytes, url) = best.ok_or_else(|| eyre::eyre!("No cover image could be downloaded"))?;
        let (bytes, media_type) = normalize_cover(bytes)?;
        tracing::info!("Using cover {url} as {media_type} ({} bytes)", bytes.len());
        self.cover_media_type = media_type.to_string();
        self.cover = Some(bytes);
        Ok(())
    }

    /// Fill chapter bodies for the inclusive TOC slice `start..=end`.
    ///
    /// Chapters that already have content are left alone, so a range download
    /// can resume from the on-disk cache without walking the whole fiction.
    pub async fn update_chapter_content(&mut self, start: usize, end: usize) -> eyre::Result<()> {
        if self.chapters.is_empty() {
            bail!("Fiction has no chapters");
        }
        if start > end || end >= self.chapters.len() {
            bail!(
                "Chapter slice {start}..={end} is outside 0..{}",
                self.chapters.len()
            );
        }

        let total = end - start + 1;
        let mut downloaded = 0usize;
        for index in start..=end {
            let position = index - start + 1;
            if self.chapters[index].content.is_some() {
                tracing::info!(
                    "Using cached chapter '{}' ({position} of {total})",
                    self.chapters[index].title
                );
                continue;
            }

            let title = self.chapters[index].title.clone();
            let path = self.chapters[index].url.clone();
            tracing::info!("Downloading chapter '{title}' ({position} of {total})");
            let url = if path.starts_with("http://") || path.starts_with("https://") {
                path
            } else {
                format!("https://www.royalroad.com{path}")
            };
            let referer = format!("https://www.royalroad.com/fiction/{}", self.id);
            let text = self.client.get_html(&url, Some(&referer)).await?;
            let parts = extract_chapter_parts(&text)?;
            let chapter = &mut self.chapters[index];
            chapter.content = Some(parts.content);
            chapter.authors_note_start = parts.authors_note_start;
            chapter.authors_note_end = parts.authors_note_end;

            downloaded += 1;
            if downloaded.is_multiple_of(20) {
                tracing::info!("Saving checkpoint after {downloaded} new chapters");
                Cache::write_book(self)?;
            }
            if index != end {
                polite_delay().await;
            }
        }

        let progress_file = Cache::cache_path()?
            .join(self.id.to_string())
            .join("download_progress.json");
        if progress_file.exists() {
            let _ = std::fs::remove_file(progress_file);
        }
        Ok(())
    }

    pub fn cover_filename(&self) -> String {
        match self.cover_media_type.as_str() {
            "image/png" => "cover.png".to_string(),
            "image/gif" => "cover.gif".to_string(),
            "image/webp" => "cover.webp".to_string(),
            _ => "cover.jpg".to_string(),
        }
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Chapter {
    pub id: u32,
    pub date: String,
    pub slug: String,
    pub title: String,
    pub url: String,
    pub content: Option<String>,

    pub authors_note_start: Option<String>,
    pub authors_note_end: Option<String>,
}

pub async fn write_epub(book: &Book, outfile: Option<String>) -> eyre::Result<()> {
    // Create a temp dir.
    let temp_folder = tempfile::tempdir()?;

    // Choose the filename.
    let outfile = match outfile {
        Some(outfile) => outfile,
        None => format!(
            "{}.epub",
            book.title
                .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "-")
        ),
    };

    // Open the file.
    let epub_path = temp_folder
        .path()
        .join(Uuid::new_v4().to_string())
        .with_extension("epub");
    tracing::debug!("Writing epub to {:?}", epub_path);
    let file = std::fs::File::create(&epub_path)?;
    let mut epub_file = zip::ZipWriter::new(file);

    // Write the mimetype.
    epub_file.start_file("mimetype", zip::write::FileOptions::default())?;
    epub_file.write_all(b"application/epub+zip")?;

    // Write the META-INF folder.
    epub_file.add_directory("META-INF", zip::write::FileOptions::default())?;

    // Write the container.xml file.
    epub_file.start_file("META-INF/container.xml", zip::write::FileOptions::default())?;
    container_xml(book, &mut epub_file)?;

    // Write the content.opf file.
    epub_file.start_file("OEBPS/content.opf", zip::write::FileOptions::default())?;
    content_opf(book, &mut epub_file)?;

    // Write the table of contents (toc.ncx) file.
    epub_file.start_file("OEBPS/toc.ncx", zip::write::FileOptions::default())?;
    toc_ncx(book, &mut epub_file)?;

    // Write each chapter.
    for chapter in book.chapters.iter() {
        // Write the chapter file.
        epub_file.start_file(
            format!("OEBPS/text/{}.xhtml", chapter.id),
            zip::write::FileOptions::default(),
        )?;
        chapter_html(chapter, &mut epub_file)?;

        // Parse and download each inline image in the content, as well as Author's Notes.
        let mut images = parse_images(&chapter.content.clone().unwrap_or("".to_string()))?;
        images.extend(parse_images(
            &chapter.authors_note_start.clone().unwrap_or("".to_string()),
        )?);
        images.extend(parse_images(
            &chapter.authors_note_end.clone().unwrap_or("".to_string()),
        )?);
        if !images.is_empty() {
            tracing::info!("Downloading inline images for chapter '{}'.", chapter.title);
            for url in images.iter() {
                epub_file.start_file(
                    format!("OEBPS/images/{}", extract_image_name(url)?),
                    zip::write::FileOptions::default(),
                )?;
                download_image(book, url.to_string(), &mut epub_file).await?;
            }
        }
    }

    // Write the cover from the bytes chosen in update_cover (largest variant, not resized).
    let cover_name = book.cover_filename();
    let cover_bytes = book
        .cover
        .clone()
        .ok_or_else(|| eyre::eyre!("Book is missing embedded cover bytes"))?;
    epub_file.start_file(
        format!("OEBPS/images/{cover_name}"),
        zip::write::FileOptions::default(),
    )?;
    epub_file.write_all(&cover_bytes)?;

    // Write the title page.
    epub_file.start_file("OEBPS/text/title.xhtml", zip::write::FileOptions::default())?;
    title_html(book, &mut epub_file)?;

    // Write the stylesheet.
    epub_file.start_file(
        "OEBPS/styles/stylesheet.css",
        zip::write::FileOptions::default(),
    )?;
    stylesheet(&mut epub_file)?;

    // Finish and copy to user destination.
    epub_file.finish()?;
    tracing::debug!("Copying epub from {:?} to {:?}", epub_path, outfile);
    std::fs::copy(epub_path, &outfile)?;

    tracing::info!("Wrote EPUB to {:?}", outfile);
    Ok(())
}

fn stylesheet(file: &mut impl Write) -> eyre::Result<()> {
    file.write_all(include_bytes!("./assets/styles.css"))?;
    Ok(())
}

fn title_html(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let cover_src = format!("../images/{}", book.cover_filename());
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    // Write the body
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .into(),
            // Write the head.
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&book.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .attr("href", "../styles/stylesheet.css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            // Write the cover.
            XmlEvent::start_element("img")
                .attr("src", &cover_src)
                .attr("alt", "Cover")
                .attr("class", "cover")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("h1").attr("class", "title").into(),
            XmlEvent::characters(&book.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("h2").attr("class", "author").into(),
            XmlEvent::characters(&book.author),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn chapter_html(chapter: &Chapter, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new().perform_indent(true);
    xml.perform_escaping = false;
    let mut xml = xml.create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .attr("xml:lang", "en")
                .into(),
            // Write the head.
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&chapter.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "generator")
                .attr("content", "text/html; charset=UTF-8")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("href", "../styles/stylesheet.css")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            // Write the body.
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("h1")
                .attr("class", "chapter-title")
                .into(),
            XmlEvent::characters(&chapter.title),
            XmlEvent::end_element().into(),
        ],
    )?;

    let hidden = HashSet::new();
    // Write the starting author's note, if any.
    if let Some(authors_note_start) = chapter.authors_note_start.clone() {
        write_author_note(
            &mut xml,
            "authors-note-start",
            &rewrite_images(strip_piracy(&authors_note_start, &hidden))?,
        )?;
    }
    // Write the content.
    if let Some(mut content) = chapter.content.clone() {
        // Remove the font-family: *; from styles.
        let font_family_regex = Regex::new(r#"font-family:.*?;"#).unwrap();
        content = font_family_regex.replace_all(&content, "").to_string();

        // Remove font-weight: normal and font-weight: 400 from styles.
        let font_weight_regex = Regex::new(r#"font-weight:\s?normal"#).unwrap();
        content = font_weight_regex.replace_all(&content, "").to_string();
        let font_weight_regex = Regex::new(r#"font-weight:\s?400"#).unwrap();
        content = font_weight_regex.replace_all(&content, "").to_string();

        // Remove overflow: auto.
        let overflow_regex = Regex::new(r#"overflow:\s?auto"#).unwrap();
        content = overflow_regex.replace_all(&content, "").to_string();
        content = strip_piracy(&content, &hidden);

        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("div")
                    .attr("class", "chapter-content")
                    .into(),
                // Rewrite the images to be pointing to our downloaded ones.
                XmlEvent::characters(&rewrite_images(content)?),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    // Write the ending author's note, if any.
    if let Some(authors_note_end) = chapter.authors_note_end.clone() {
        write_author_note(
            &mut xml,
            "authors-note-end",
            &rewrite_images(strip_piracy(&authors_note_end, &hidden))?,
        )?;
    }

    // Close elements.
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn write_author_note(
    xml: &mut xml::EventWriter<&mut impl Write>,
    class: &str,
    html: &str,
) -> eyre::Result<()> {
    write_elements(
        xml,
        vec![
            XmlEvent::start_element("aside")
                .attr("class", class)
                .attr("role", "note")
                .into(),
            XmlEvent::start_element("p")
                .attr("class", "authors-note-label")
                .into(),
            XmlEvent::characters("Author's Note"),
            XmlEvent::end_element().into(),
            XmlEvent::characters(html),
            XmlEvent::end_element().into(),
        ],
    )
}

fn chapter_item_id(id: u32) -> String {
    format!("ch-{id}")
}

fn container_xml(_: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("container")
                .attr("version", "1.0")
                .ns("a", "urn:oasis:names:tc:opendocument:xmlns:container")
                .into(),
            XmlEvent::start_element("rootfiles").into(),
            XmlEvent::start_element("rootfile")
                .attr("full-path", "OEBPS/content.opf")
                .attr("media-type", "application/oebps-package+xml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn content_opf(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    let cover_href = format!("images/{}", book.cover_filename());
    let cover_type = if book.cover_media_type.is_empty() {
        "image/jpeg".to_string()
    } else {
        book.cover_media_type.clone()
    };
    let fiction_id = book.id.to_string();
    let range_start = book.range_start_id.map(|id| id.to_string());
    let range_end = book.range_end_id.map(|id| id.to_string());
    let range_latest = if book.range_end_latest {
        "true"
    } else {
        "false"
    };
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("package")
                .ns("", "http://www.idpf.org/2007/opf")
                .attr("version", "3.0")
                .attr("unique-identifier", "bookid")
                .into(),
            XmlEvent::start_element("metadata")
                .ns("dc", "http://purl.org/dc/elements/1.1/")
                .into(),
            XmlEvent::start_element("dc:title").into(),
            XmlEvent::characters(&book.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:creator").into(),
            XmlEvent::characters(&book.author),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:description").into(),
            XmlEvent::characters(&book.description),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:date").into(),
            XmlEvent::characters(&book.date_published),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:identifier")
                .attr("id", "bookid")
                .into(),
            XmlEvent::characters(&book.id.to_string()),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:language").into(),
            XmlEvent::characters("en"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "cover")
                .attr("content", "cover")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "primary-writing-mode")
                .attr("content", "horizontal-lr")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "rr-to-epub:royal-road-id")
                .attr("content", &fiction_id)
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    if let Some(start) = range_start.as_deref() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("meta")
                    .attr("name", "rr-to-epub:range-start-id")
                    .attr("content", start)
                    .into(),
                XmlEvent::end_element().into(),
                XmlEvent::start_element("meta")
                    .attr("name", "rr-to-epub:range-end-latest")
                    .attr("content", range_latest)
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
        if let Some(end) = range_end.as_deref() {
            write_elements(
                &mut xml,
                vec![
                    XmlEvent::start_element("meta")
                        .attr("name", "rr-to-epub:range-end-id")
                        .attr("content", end)
                        .into(),
                    XmlEvent::end_element().into(),
                ],
            )?;
        }
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            // Write the manifest.
            XmlEvent::start_element("manifest").into(),
            // Write the title page.
            XmlEvent::start_element("item")
                .attr("id", "title")
                .attr("href", "text/title.xhtml")
                .attr("media-type", "application/xhtml+xml")
                .into(),
            XmlEvent::end_element().into(),
            // Write the cover. properties="cover-image" is the EPUB 3 cover.
            XmlEvent::start_element("item")
                .attr("id", "cover")
                .attr("href", &cover_href)
                .attr("media-type", &cover_type)
                .attr("properties", "cover-image")
                .into(),
            XmlEvent::end_element().into(),
            // Write the stylesheet.
            XmlEvent::start_element("item")
                .attr("id", "stylesheet")
                .attr("href", "styles/stylesheet.css")
                .attr("media-type", "text/css")
                .into(),
            XmlEvent::end_element().into(),
            // Write the table of contents.
            XmlEvent::start_element("item")
                .attr("id", "toc")
                .attr("href", "toc.ncx")
                .attr("media-type", "application/xhtml+xml")
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    // Write each chapter.
    for chapter in book.chapters.iter() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("item")
                    .attr("id", &chapter_item_id(chapter.id))
                    .attr("href", &format!("text/{}.xhtml", chapter.id))
                    .attr("media-type", "application/xhtml+xml")
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            // Start the spine.
            XmlEvent::start_element("spine").attr("toc", "ncx").into(),
            // Write the title page entry.
            XmlEvent::start_element("itemref")
                .attr("idref", "title")
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    // For each chapter, write a link.
    for chapter in book.chapters.iter() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("itemref")
                    .attr("idref", &chapter_item_id(chapter.id))
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    write_elements(
        &mut xml,
        vec![
            // End the spine.
            XmlEvent::end_element().into(),
            XmlEvent::start_element("guide").into(),
            XmlEvent::start_element("reference")
                .attr("type", "cover")
                .attr("title", "Cover")
                .attr("href", "text/title.xhtml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            // End the package.
            XmlEvent::end_element().into(),
        ],
    )?;

    Ok(())
}

fn toc_ncx(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("ncx")
                .ns("", "http://www.daisy.org/z3986/2005/ncx/")
                .attr("version", "2005-1")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:uid")
                .attr("content", &format!("{}", book.id))
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:depth")
                .attr("content", "2")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:totalPageCount")
                .attr("content", "0")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:maxPageNumber")
                .attr("content", "0")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("docTitle").into(),
            XmlEvent::start_element("text").into(),
            XmlEvent::characters(&book.title),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("navMap").into(),
            XmlEvent::start_element("navPoint")
                .attr("id", "cover")
                .attr("playOrder", "0")
                .into(),
            XmlEvent::start_element("navLabel").into(),
            XmlEvent::start_element("text").into(),
            XmlEvent::characters("Cover"),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("content")
                .attr("src", "text/title.xhtml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    // For each chapter, write a link.
    for (index, chapter) in book.chapters.iter().enumerate() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("navPoint")
                    .attr("id", &chapter_item_id(chapter.id))
                    .attr("playOrder", &format!("{}", index + 1))
                    .into(),
                XmlEvent::start_element("navLabel").into(),
                XmlEvent::start_element("text").into(),
                XmlEvent::characters(&chapter.title),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
                XmlEvent::start_element("content")
                    .attr("src", &format!("text/{}.xhtml", chapter.id))
                    .into(),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }

    // Write the end of the document.
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    Ok(())
}

fn extract_image_name(url: &str) -> eyre::Result<String> {
    let mut url = Url::parse(url)?;
    url.set_query(None);
    url.set_fragment(None);

    Ok(url
        .path_segments()
        .ok_or(eyre::eyre!("Invalid image URL"))?
        .next_back()
        .ok_or(eyre::eyre!("Invalid image URL"))?
        .to_string())
}
fn parse_images(body: &str) -> eyre::Result<Vec<String>> {
    let parsed = Html::parse_fragment(body);
    let selector = Selector::parse("img").unwrap();

    let mut sources = Vec::new();
    for element in parsed.select(&selector) {
        if let Some(src) = element.value().attr("src") {
            sources.push(src.to_string());
        }
    }
    Ok(sources)
}
fn rewrite_images(mut body: String) -> eyre::Result<String> {
    let parsed = Html::parse_fragment(&body);
    let selector = Selector::parse("img").unwrap();

    for element in parsed.select(&selector) {
        if let Some(src) = element.value().attr("src") {
            let new_src = format!("../images/{}", extract_image_name(src)?);
            body = body.replace(src, &new_src);
        }
    }
    Ok(body)
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.len() > 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
        && bytes[4] == 0x0D
        && bytes[5] == 0x0A
        && bytes[6] == 0x1A
        && bytes[7] == 0x0A
}
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}
fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() > 11
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
}
fn is_gif(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 && bytes[3] == 0x38
}
fn is_svg(bytes: &[u8]) -> bool {
    if let Ok(text) = std::str::from_utf8(bytes) {
        text.to_lowercase().trim().starts_with("<?xml")
            || text.to_lowercase().trim().starts_with("<svg")
    } else {
        false
    }
}
fn is_html(bytes: &[u8]) -> bool {
    if let Ok(text) = std::str::from_utf8(bytes) {
        text.to_lowercase().trim().starts_with("<!doctype html>")
            || text.to_lowercase().trim().starts_with("<html")
    } else {
        false
    }
}

async fn download_image(book: &Book, url: String, file: &mut impl Write) -> eyre::Result<()> {
    let filename = extract_image_name(&url)?;
    match Cache::read_inline_image(book, &filename)? {
        Some(image) => {
            file.write_all(&image)?;
        }
        None => {
            let bytes = match book
                .client
                .get_bytes(
                    &url,
                    Some(&format!("https://www.royalroad.com/fiction/{}", book.id)),
                )
                .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    // Handle DNS and connection errors gracefully
                    tracing::warn!(
                        "Failed to download image from URL ({}): {}. This is likely NOT a bug with rr-to-epub.",
                        url,
                        e
                    );
                    return Ok(());
                }
            };

            let image = if is_html(&bytes) {
                // Skip HTML pages.
                return Ok(());
            } else if is_gif(&bytes) || is_svg(&bytes) {
                // Directly pass through the image.
                file.write_all(&bytes)?;
                return Ok(());
            } else if is_webp(&bytes) {
                // Decode the image.
                let Some(webp) = Decoder::new(&bytes).decode() else {
                    tracing::warn!(
                        "Failed to decode webp image. Please report this as a bug and include the following URL: {}",
                        url
                    );
                    return Ok(()); // Skip the image.
                };
                webp.to_image()
            } else if is_jpeg(&bytes) || is_png(&bytes) {
                // Decode the image.
                Reader::new(Cursor::new(&bytes))
                    .with_guessed_format()?
                    .decode()?
            } else {
                bail!("Unsupported inline image format. Please report this as a bug and include the following URL: {}", url);
            };

            // Resize to max width of 600px.
            let width = image.width();
            let height = image.height();
            let image = image.resize(
                600,
                600 * height / width,
                image::imageops::FilterType::Lanczos3,
            );

            // Encode the image.
            let mut buffer = Vec::new();
            if is_png(&bytes) || is_webp(&bytes) {
                // We write both PNG and WebP as PNG because WebP is not supported by some e-readers.
                image.write_with_encoder(PngEncoder::new_with_quality(
                    Cursor::new(&mut buffer),
                    CompressionType::Fast,
                    FilterType::Adaptive,
                ))?;
            } else if is_jpeg(&bytes) {
                image.write_with_encoder(JpegEncoder::new_with_quality(
                    Cursor::new(&mut buffer),
                    80,
                ))?;
            } else {
                bail!("Unsupported inline image format. Please report this as a bug and include the following URL: {}", url);
            };

            // Save the image in the cache.
            Cache::write_inline_image(book, &filename, &buffer)?;

            // Write the image to the file.
            file.write_all(&buffer)?;
        }
    }

    Ok(())
}

fn parse_chapters(response: &str) -> eyre::Result<Vec<Chapter>> {
    let marker = "window.chapters = ";
    let index = response
        .find(marker)
        .ok_or_else(|| eyre::eyre!("No chapters found"))?;
    let mut deserializer = serde_json::Deserializer::from_str(&response[index + marker.len()..]);
    let mut chapters = Vec::<Chapter>::deserialize(&mut deserializer)
        .map_err(|err| eyre::eyre!("Failed to parse chapter list: {err}"))?;
    for chapter in &mut chapters {
        chapter.title = normalize_chapter_title(&chapter.title);
    }
    Ok(chapters)
}

pub fn normalize_chapter_title(title: &str) -> String {
    title.trim().to_string()
}

fn text_of(document: &Html, selector: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    let text = document
        .select(&selector)
        .next()?
        .text()
        .collect::<String>();
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn html_of(document: &Html, selector: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    let html = document.select(&selector).next()?.inner_html();
    if html.trim().is_empty() {
        None
    } else {
        Some(html)
    }
}

fn meta_content(document: &Html, property: &str) -> Option<String> {
    let selector = Selector::parse(&format!(
        r#"meta[property="{property}"], meta[name="{property}"]"#
    ))
    .ok()?;
    let value = document
        .select(&selector)
        .next()?
        .value()
        .attr("content")?
        .trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn json_ld_book(html: &str) -> Option<serde_json::Value> {
    let re = Regex::new(r#"(?s)<script type="application/ld\+json">(.*?)</script>"#).unwrap();
    for capture in re.captures_iter(html) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&capture[1]) else {
            continue;
        };
        if value.get("@type").and_then(|kind| kind.as_str()) == Some("Book") {
            return Some(value);
        }
    }
    None
}

fn json_ld_string(value: &Option<serde_json::Value>, key: &str) -> Option<String> {
    let text = value.as_ref()?.get(key)?.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

pub fn cover_candidate_urls(url: &str) -> Vec<String> {
    const VARIANTS: &[&str] = &[
        "covers-thumbnail",
        "covers-full",
        "covers-large",
        "covers-small",
    ];
    let mut urls = Vec::new();
    if let Some(large) = rewrite_cover_variant(url, VARIANTS, "covers-large") {
        urls.push(large);
    }
    if !urls.iter().any(|candidate| candidate == url) {
        urls.push(url.to_string());
    }
    urls
}

fn rewrite_cover_variant(url: &str, variants: &[&str], target: &str) -> Option<String> {
    for variant in variants {
        let needle = format!("/{variant}/");
        if let Some(start) = url.find(&needle) {
            let mut rewritten = String::new();
            rewritten.push_str(&url[..start]);
            rewritten.push('/');
            rewritten.push_str(target);
            rewritten.push('/');
            rewritten.push_str(&url[start + needle.len()..]);
            return Some(rewritten);
        }
    }
    None
}

fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    if is_png(bytes) {
        Some("image/png")
    } else if is_jpeg(bytes) {
        Some("image/jpeg")
    } else if is_gif(bytes) {
        Some("image/gif")
    } else {
        None
    }
}

fn normalize_cover(bytes: Vec<u8>) -> eyre::Result<(Vec<u8>, &'static str)> {
    if let Some(media_type) = sniff_image_type(&bytes) {
        return Ok((bytes, media_type));
    }
    if is_webp(&bytes) {
        let image = Decoder::new(&bytes)
            .decode()
            .ok_or_else(|| eyre::eyre!("Failed to decode cover webp"))?
            .to_image();
        let mut buffer = Vec::new();
        image.write_with_encoder(PngEncoder::new(Cursor::new(&mut buffer)))?;
        return Ok((buffer, "image/png"));
    }
    bail!("Unsupported cover image format");
}

struct ChapterParts {
    content: String,
    authors_note_start: Option<String>,
    authors_note_end: Option<String>,
}

fn extract_chapter_parts(html: &str) -> eyre::Result<ChapterParts> {
    let parsed = Html::parse_document(html);
    let content_selector = Selector::parse(".chapter-inner.chapter-content").unwrap();
    let not_stolen_selector = Selector::parse(":scope > :not(div[class])").unwrap();
    let note_selector = Selector::parse(".author-note").unwrap();
    let content_el = parsed
        .select(&content_selector)
        .next()
        .ok_or_else(|| eyre::eyre!("No chapter content found"))?;
    let content_html = content_el
        .select(&not_stolen_selector)
        .map(|element| element.html())
        .collect::<Vec<_>>()
        .join("\n");
    let hidden = hidden_classes(html);
    let content = strip_piracy(&content_html, &hidden);
    let mut start_notes = Vec::new();
    let mut end_notes = Vec::new();
    for note in parsed.select(&note_selector) {
        let text: String = note.text().collect();
        if text.trim().is_empty() {
            continue;
        }
        let cleaned = strip_piracy(&note.inner_html(), &hidden);
        if element_is_before(&parsed, note, content_el) {
            start_notes.push(cleaned);
        } else {
            end_notes.push(cleaned);
        }
    }
    Ok(ChapterParts {
        content,
        authors_note_start: join_notes(start_notes),
        authors_note_end: join_notes(end_notes),
    })
}

fn element_is_before(
    document: &Html,
    earlier: scraper::ElementRef,
    later: scraper::ElementRef,
) -> bool {
    for node in document.tree.root().descendants() {
        if node.id() == earlier.id() {
            return true;
        }
        if node.id() == later.id() {
            return false;
        }
    }
    false
}

fn join_notes(notes: Vec<String>) -> Option<String> {
    let notes: Vec<String> = notes
        .into_iter()
        .filter(|note| !note.trim().is_empty())
        .collect();
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cover_candidate_urls, extract_chapter_parts, normalize_chapter_title, write_epub, Book,
        Chapter,
    };
    use image::ImageEncoder;
    use std::io::{Cursor, Read};

    #[test]
    fn trims_titles_with_leading_space() {
        assert_eq!(
            normalize_chapter_title(" Chapter 144 – Work. "),
            "Chapter 144 – Work."
        );
    }

    #[test]
    fn cover_candidates_prefer_the_large_variant() {
        let large =
            "https://www.royalroadcdn.com/public/covers-large/33844-the-runesmith.jpg?time=1";
        assert_eq!(cover_candidate_urls(large), vec![large.to_string()]);

        let full = "https://www.royalroadcdn.com/public/covers-full/33844-the-runesmith.jpg?time=1";
        assert_eq!(
            cover_candidate_urls(full),
            vec![
                "https://www.royalroadcdn.com/public/covers-large/33844-the-runesmith.jpg?time=1"
                    .to_string(),
                full.to_string(),
            ]
        );
    }

    #[test]
    fn chapter_html_keeps_formatting_notes_and_drops_piracy_lines() {
        let class = "cjFmMDUwYWM0NzExZjQ2Yjg4MThhZDk3NGY2OGNjNDFh";
        let html = format!(
            r#"<html><head><style>.{class} {{ display: none; speak: never; }}</style></head><body>
            <div class="portlet solid author-note-portlet"><div class="portlet-body author-note"><p>Start note here.</p></div></div>
            <div class="chapter-inner chapter-content">
                <p class="cnNkYWMwMzBkOGYxODRiZDJhYzA1MmM2NTNjMmE3Yjk4">Hello <em>italic</em> and <strong>bold</strong> and <i>slant</i> and <b>weight</b>.</p>
                <p class="{class}">A case of theft: this story is not rightfully on Amazon; if you spot it, report the violation.</p>
                <span class="cmMxOGViZDM1ZmMxZDQyMjFhYWExZmYyNjMxZTY5OTU0"><br>You could be reading stolen content. Head to Royal Road for the genuine story.<br></span>
                <div class="advert">Advertisement</div>
                <p>After the interlude.</p>
            </div>
            <div class="portlet solid author-note-portlet"><div class="portlet-body author-note"><p>Thanks for reading.</p></div></div>
            </body></html>"#
        );
        let parts = extract_chapter_parts(&html).unwrap();
        assert!(parts.content.contains("<em>italic</em>"));
        assert!(parts.content.contains("<strong>bold</strong>"));
        assert!(parts.content.contains("<i>slant</i>"));
        assert!(parts.content.contains("<b>weight</b>"));
        assert!(parts.content.contains("After the interlude."));
        assert!(!parts.content.contains("case of theft"));
        assert!(!parts.content.contains("stolen content"));
        assert!(!parts.content.contains("Advertisement"));
        assert_eq!(
            parts.authors_note_start.as_deref(),
            Some("<p>Start note here.</p>")
        );
        assert_eq!(
            parts.authors_note_end.as_deref(),
            Some("<p>Thanks for reading.</p>")
        );
    }

    #[tokio::test]
    async fn epub_has_a_spine_entry_cover_and_note_per_chapter() {
        let png = tiny_png();
        let book = Book {
            id: 33844,
            title: "The Runesmith (Ch 672-673)".to_string(),
            author: "Kuropon".to_string(),
            description: "A story.".to_string(),
            date_published: "2026-10-30T08:58:17Z".to_string(),
            cover_url: "https://www.royalroadcdn.com/public/covers-large/33844.jpg".to_string(),
            cover: Some(png.clone()),
            cover_media_type: "image/png".to_string(),
            chapters: vec![
                Chapter {
                    id: 3402749,
                    title: "Chapter 672 – First Apprentice Acquired.".to_string(),
                    content: Some(
                        r#"<p>Hello <em>italic</em> and <strong>bold</strong>.</p><span class="cmMxOGViZDM1ZmMxZDQyMjFhYWExZmYyNjMxZTY5OTU0"><br>You could be reading stolen content. Head to Royal Road for the genuine story.<br></span>"#
                            .to_string(),
                    ),
                    authors_note_end: Some("<p>Thanks for reading.</p>".to_string()),
                    ..Chapter::default()
                },
                Chapter {
                    id: 3417326,
                    title: "Chapter 673 – Too Many Portals.".to_string(),
                    content: Some("<p>Next.</p>".to_string()),
                    authors_note_start: Some("<p>Before.</p>".to_string()),
                    ..Chapter::default()
                },
            ],
            range_start_id: Some(3402749),
            range_end_id: Some(3417326),
            range_end_latest: false,
            ..Book::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slice.epub");
        write_epub(&book, Some(path.to_string_lossy().into_owned()))
            .await
            .unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut opf = String::new();
        archive
            .by_name("OEBPS/content.opf")
            .unwrap()
            .read_to_string(&mut opf)
            .unwrap();
        assert!(opf.contains("The Runesmith (Ch 672-673)"));
        assert!(opf.contains(r#"properties="cover-image""#));
        assert!(opf.contains("images/cover.png"));
        assert!(opf.contains("image/png"));
        assert!(opf.contains(r#"idref="ch-3402749""#));
        assert!(opf.contains(r#"idref="ch-3417326""#));
        assert!(opf.contains(r#"rr-to-epub:range-start-id" content="3402749""#));
        assert!(opf.contains(r#"rr-to-epub:range-end-id" content="3417326""#));
        let mut reader = xml::EventReader::new(opf.as_bytes());
        loop {
            match reader.next() {
                Ok(xml::reader::XmlEvent::EndDocument) => break,
                Ok(_) => {}
                Err(err) => panic!("content.opf is not well-formed XML: {err}"),
            }
        }

        let mut ncx = String::new();
        archive
            .by_name("OEBPS/toc.ncx")
            .unwrap()
            .read_to_string(&mut ncx)
            .unwrap();
        assert!(ncx.contains("Chapter 672 – First Apprentice Acquired."));
        assert!(ncx.contains("Chapter 673 – Too Many Portals."));
        assert_eq!(ncx.matches("<navPoint").count(), 3);

        let mut chapter = String::new();
        archive
            .by_name("OEBPS/text/3402749.xhtml")
            .unwrap()
            .read_to_string(&mut chapter)
            .unwrap();
        assert!(chapter.contains("<em>italic</em>"));
        assert!(chapter.contains("<strong>bold</strong>"));
        assert!(chapter.contains("Author's Note"));
        assert!(chapter.contains(r#"role="note""#));
        assert!(chapter.contains("Thanks for reading."));
        assert!(!chapter.contains("stolen content"));

        let mut cover = Vec::new();
        archive
            .by_name("OEBPS/images/cover.png")
            .unwrap()
            .read_to_end(&mut cover)
            .unwrap();
        assert_eq!(cover, png);

        let origin = Book::origin_from_file(&path).unwrap();
        assert_eq!(origin.id, Some(33844));
        assert_eq!(origin.range.from.as_deref(), Some("id:3402749"));
        assert_eq!(origin.range.to.as_deref(), Some("id:3417326"));
    }

    fn tiny_png() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(2, 2, image::Rgb([12, 34, 56]));
        let mut buffer = Vec::new();
        image::codecs::png::PngEncoder::new(Cursor::new(&mut buffer))
            .write_image(image.as_raw(), 2, 2, image::ColorType::Rgb8)
            .unwrap();
        buffer
    }
}
