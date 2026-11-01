use crate::api::RoyalRoadApi;
use crate::epub::{write_epub, StoredBook};
use crate::range::ChapterQuery;
use clap::Parser;
use eyre::WrapErr;
use url::Url;

mod api;
mod cache;
mod clean;
mod epub;
mod http;
mod range;
mod xml_ext;

#[derive(Debug, Parser)]
pub struct DownloadArgs {
    /// The file path to write the EPUB to. Defaults to "<title>.epub", or
    /// "<title> (Ch A-B).epub" when a range is selected.
    pub output_file: Option<String>,

    #[arg(long = "book-id", short = 'b')]
    /// The ID of the book to download. One of --book-id or --book-url is required.
    pub book_id: Option<u32>,

    #[arg(long = "book-url", short = 'u')]
    /// The URL of the book to download. One of --book-id or --book-url is required.
    pub book_url: Option<String>,

    /// Inclusive first chapter. A number matches a title like "Chapter 672"
    /// (leading spaces are ignored). If no title matches, it is a 1-based TOC
    /// index. Also accepts index:N, id:N, a chapter URL, or latest.
    #[arg(long, value_name = "CHAPTER", conflicts_with = "after")]
    pub from: Option<String>,

    /// Inclusive last chapter. Defaults to the latest chapter. Same syntax as --from.
    #[arg(long, value_name = "CHAPTER")]
    pub to: Option<String>,

    /// Start at the chapter after this one. Same syntax as --from.
    #[arg(long, value_name = "CHAPTER", conflicts_with = "from")]
    pub after: Option<String>,
}

#[derive(Debug, Parser)]
pub struct UpdateArgs {
    /// The folder of books to update, or the path to a single book file.
    pub folder_or_file: String,
}

#[derive(Debug, Parser)]
pub struct GlobalArgs {
    #[arg(long = "ignore-cache", global = true)]
    /// Ignore the cache and redownload chapters even if they were not modified.
    pub ignore_cache: bool,
}

#[derive(Debug, Parser)]
pub enum Command {
    #[clap(name = "download", about = "Download a book from Royal Road.")]
    Download(DownloadArgs),
    #[clap(name = "update", about = "Update all books in a folder.")]
    Update(UpdateArgs),
}

#[derive(Debug, Parser)]
#[clap(about, version, author)]
pub struct App {
    #[clap(subcommand)]
    command: Command,

    #[clap(flatten)]
    global: GlobalArgs,
}

#[tokio::main]
async fn main() {
    init_tracing();
    if let Err(err) = run().await {
        tracing::error!("{err:?}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run() -> eyre::Result<()> {
    let app = App::parse();
    let command = app.command;
    match command {
        Command::Download(args) => download(app.global, args).await?,
        Command::Update(args) => update(app.global, args).await?,
    }
    Ok(())
}

async fn download(global_args: GlobalArgs, args: DownloadArgs) -> eyre::Result<()> {
    let (id, source_url) = match (&args.book_id, args.book_url.as_deref()) {
        (Some(id), source) => (*id, source),
        (None, Some(url)) => (book_id_from_url(url)?, Some(url)),
        (None, None) => {
            eyre::bail!("One of --book-id or --book-url is required.");
        }
    };
    let query = ChapterQuery {
        from: args.from,
        to: args.to,
        after: args.after,
    };
    let api = RoyalRoadApi::new()?;
    let book = api
        .get_book(id, source_url, &query, global_args.ignore_cache)
        .await?;
    write_epub(api.http(), &book, args.output_file).await?;
    Ok(())
}

async fn update(global_args: GlobalArgs, args: UpdateArgs) -> eyre::Result<()> {
    let api = RoyalRoadApi::new()?;
    let Ok(stat) = std::fs::metadata(&args.folder_or_file) else {
        tracing::error!(
            "Folder or file \"{}\" does not exist, aborting.",
            args.folder_or_file
        );
        return Ok(());
    };
    if stat.is_file() {
        update_file(&api, &args.folder_or_file, global_args.ignore_cache).await?;
    } else {
        let list = std::fs::read_dir(&args.folder_or_file)?;
        for file in list {
            let file = file?;
            let file_name = file
                .file_name()
                .into_string()
                .map_err(|_| eyre::eyre!("Invalid file name: {:?}", file.file_name()))?;
            if !file_name.ends_with(".epub") {
                continue;
            }
            let path = file.path();
            let path = path.to_string_lossy().to_string();
            update_file(&api, &path, global_args.ignore_cache).await?;
        }
    }
    Ok(())
}

async fn update_file(api: &RoyalRoadApi, file_name: &str, ignore_cache: bool) -> eyre::Result<()> {
    if !file_name.ends_with(".epub") {
        tracing::error!(
            "File \"{file_name}\" is not an EPUB file or does not have the .epub extension, aborting."
        );
        return Ok(());
    }
    let path = std::path::Path::new(file_name).canonicalize()?;
    let Some(stored) = StoredBook::read(path)? else {
        tracing::warn!("Found unmanaged book file \"{file_name}\", skipping.");
        return Ok(());
    };
    tracing::info!("Found book file \"{file_name}\", updating.");
    let query = ChapterQuery {
        from: stored.range_from,
        to: stored.range_to,
        after: stored.range_after,
    };
    let book = api
        .get_book(
            stored.id,
            stored.source_url.as_deref(),
            &query,
            ignore_cache,
        )
        .await?;
    write_epub(api.http(), &book, Some(file_name.to_string())).await?;
    Ok(())
}

fn book_id_from_url(input: &str) -> eyre::Result<u32> {
    let url = Url::parse(input).wrap_err_with(|| format!("Invalid book URL: {input}"))?;
    let segments: Vec<&str> = url
        .path_segments()
        .map(|parts| parts.filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();
    segments
        .windows(2)
        .find_map(|pair| {
            if pair[0] == "fiction" {
                pair[1].parse::<u32>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| eyre::eyre!("Invalid book URL: {input}"))
}
