# rr-to-epub

> [!NOTE]  
> All stories are the property of their respective authors. This application is not affiliated with Royal Road in any way and makes no attempt to claim credit for any stories downloaded. **If anyone working at Royal Road wants this tool taken down, please reach out to me at the email linked to my Github and I will immediately comply.**

A small application to convert a [Royal Road](https://www.royalroad.com/) story to an `.epub` file, compatible with readers such as Kindle or Calibre. Motivated by me wanting to read some stories on my Kindle when I'm without access to the Royal Road web service.

## Installation

### From Source

This tool is written in Rust. To install it from source, first install [Rust](https://www.rust-lang.org/tools/install), then run the following command:

```sh
cargo install --locked --git https://github.com/gabcoyne/rr-to-epub
```

Prebuilt binaries are not currently available. The first run downloads [curl-impersonate](https://github.com/lexiforest/curl-impersonate) into `~/.cache/rr-to-epub` (about 11 MB) so chapter requests can use a Chrome 152 TLS fingerprint.

## Usage

Download a whole book:

```sh
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith
```

Download a slice. A bare number matches a chapter title such as `Chapter 672`, ignoring a leading space in the table of contents. Interludes and side chapters between those bounds are included in TOC order, and only chapters in the range are downloaded. Leave `--to` off to continue through the latest chapter. `--after` starts on the next chapter.

```sh
# Chapter 672 through the latest chapter
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith --from 672

# Inclusive span. The EPUB title is "The Runesmith (Ch 672-723)"
rr-to-epub download runesmith-672-723.epub -u https://www.royalroad.com/fiction/33844/the-runesmith --from 672 --to 723

# Start after chapter 671
rr-to-epub download -u https://www.royalroad.com/fiction/33844/the-runesmith --after 671

# 1-based TOC index, Royal Road chapter id, or a chapter URL
rr-to-epub download -u <URL> --from index:10 --to index:40
rr-to-epub download -u <URL> --from id:3402749 --to latest
rr-to-epub download -u <URL> --from https://www.royalroad.com/fiction/33844/the-runesmith/chapter/3402749/chapter-672-first-apprentice-acquired
```

If no title looks like `Chapter N`, the number is a 1-based TOC index instead.

Each chapter is its own spine item and TOC entry. The fiction cover is embedded (the largest size Royal Road serves) and set as the EPUB cover. Paragraphs, italics, and bold are kept. Author notes stay in the chapter and are marked as notes. Royal Road's hidden anti-theft lines are removed.

Updating a folder, or one EPUB, redownloads that book. A ranged EPUB keeps the same range:

```sh
rr-to-epub update <directory-or-file>
```

`--ignore-cache` redownloads chapters even when the cached copy is current. Progress is still checkpointed every 100 chapters under `~/.cache/rr-to-epub`.

Royal Road's Cloudflare check rejects a normal Rust HTTP client with HTTP 403, including when the request only changes its User-Agent. Older browser impersonation profiles were challenged too. Fetches use curl-impersonate's Chrome 152 profile, retry with backoff, and wait between chapter requests. Set `RR_CURL_IMPERSONATE` if you already have that binary.

Full help can be found by running `rr-to-epub --help`.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
