use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{CrawlError, Result};

/// Private execution contract. Redacted public copies are never executable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceCandidate {
    pub schema_version: u8,
    pub visibility: String,
    pub resource_id: String,
    pub source_page: Url,
    pub url: Url,
    pub kind: String,
    pub discovery: String,
    pub credential_scope: String,
}

pub(crate) fn discover(source: &Url, body: &[u8], limit: usize) -> Vec<ResourceCandidate> {
    let text = String::from_utf8_lossy(body);
    let document = Html::parse_document(&text);
    let selector = Selector::parse("video[src],audio[src],source[src],track[src],iframe[src],embed[src],object[data],enclosure[url],link[rel='enclosure'][href],meta[property='og:video'][content],meta[property='og:audio'][content],a[href]").expect("static selector");
    let base_selector = Selector::parse("base[href]").expect("static selector");
    let base = document
        .select(&base_selector)
        .next()
        .and_then(|node| source.join(node.attr("href")?).ok())
        .unwrap_or_else(|| source.clone());
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for element in document.select(&selector).take(limit.saturating_mul(4)) {
        let Some(raw) = ["src", "data", "url", "href", "content"]
            .into_iter()
            .find_map(|name| element.attr(name))
        else {
            continue;
        };
        let Ok(mut url) = base.join(raw) else {
            continue;
        };
        url.set_fragment(None);
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.as_str().len() > 8192
        {
            continue;
        }
        let tag = element.value().name();
        let path = url.path().to_ascii_lowercase();
        let extension = std::path::Path::new(&path)
            .extension()
            .and_then(|value| value.to_str());
        let mime = element.attr("type").unwrap_or_default();
        let kind = if extension == Some("m3u8") || mime.contains("mpegurl") {
            "hls"
        } else if extension == Some("mpd") || mime.contains("dash+xml") {
            "dash"
        } else if tag == "track" {
            "subtitle"
        } else if matches!(tag, "iframe" | "embed" | "object") {
            "media_page"
        } else if matches!(tag, "video" | "audio")
            || (tag == "source"
                && element
                    .parent()
                    .and_then(scraper::ElementRef::wrap)
                    .is_some_and(|parent| matches!(parent.value().name(), "video" | "audio")))
            || mime.starts_with("video/")
            || mime.starts_with("audio/")
            || [
                ".mp4", ".webm", ".mkv", ".mov", ".mp3", ".m4a", ".ogg", ".opus", ".wav", ".flac",
            ]
            .iter()
            .any(|ext| path.ends_with(ext))
        {
            "direct_media"
        } else if tag == "a" {
            continue;
        } else {
            "unknown"
        };
        let resource_id = identity(&url, kind);
        if !seen.insert(resource_id.clone()) {
            continue;
        }
        found.push(ResourceCandidate {
            schema_version: 1,
            visibility: "execution".into(),
            resource_id,
            source_page: source.clone(),
            url,
            kind: kind.into(),
            discovery: format!("html:{tag}"),
            credential_scope: "public".into(),
        });
        if found.len() >= limit {
            break;
        }
    }
    found
}

pub(crate) fn from_response(
    source: &Url,
    url: &Url,
    content_type: Option<&str>,
) -> Vec<ResourceCandidate> {
    let mime = content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let kind = if mime.contains("mpegurl") {
        "hls"
    } else if mime == "application/dash+xml" {
        "dash"
    } else if mime.starts_with("audio/") || mime.starts_with("video/") {
        "direct_media"
    } else {
        return Vec::new();
    };
    vec![ResourceCandidate {
        schema_version: 1,
        visibility: "execution".into(),
        resource_id: identity(url, kind),
        source_page: source.clone(),
        url: url.clone(),
        kind: kind.into(),
        discovery: "http:content-type".into(),
        credential_scope: "public".into(),
    }]
}

pub(crate) fn identity(url: &Url, kind: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{}\n{kind}\npublic", url.as_str()).as_bytes())
    )
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(output_error)?;
    let metadata = std::fs::symlink_metadata(path).map_err(output_error)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(output_error(
            "private output requires a real 0700 directory",
        ));
    }
    Ok(())
}

pub(crate) fn lock_output(path: &Path) -> Result<std::fs::File> {
    use fs2::FileExt;
    use rustix::fs::{Mode, OFlags, open};
    private_directory(path)?;
    let lock_path = path.join("output.lock");
    if std::fs::symlink_metadata(&lock_path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(output_error("private output lock must be a regular file"));
    }
    let file = std::fs::File::from(
        open(
            lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(output_error)?,
    );
    if !file.metadata().map_err(output_error)?.is_file() {
        return Err(output_error("private output lock is not regular"));
    }
    file.try_lock_exclusive()
        .map_err(|_| output_error("private output is in use by another crawl"))?;
    Ok(file)
}

pub(crate) fn atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags, open, openat, renameat, unlinkat};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| output_error("missing output parent"))?;
    private_directory(parent)?;
    let directory = std::fs::File::from(
        open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(output_error)?,
    );
    let temp = format!(
        ".pending-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(output_error)?
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut file = std::fs::File::from(
        openat(
            &directory,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(output_error)?,
    );
    let result = (|| {
        file.write_all(bytes).map_err(output_error)?;
        file.sync_all().map_err(output_error)?;
        renameat(
            &directory,
            &temp,
            &directory,
            path.file_name()
                .ok_or_else(|| output_error("missing filename"))?,
        )
        .map_err(output_error)?;
        directory.sync_all().map_err(output_error)
    })();
    if result.is_err() {
        let _ = unlinkat(&directory, &temp, AtFlags::empty());
    }
    result
}

/// Preserve the first provenance record so replay has an identical request hash.
pub(crate) fn persist_candidate(path: &Path, candidate: &ResourceCandidate) -> Result<()> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.permissions().mode() & 0o077 != 0
                || metadata.len() > 32768
            {
                return Err(output_error("invalid private outbox entry"));
            }
            let mut bytes = Vec::new();
            std::fs::File::from(
                rustix::fs::open(
                    path,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map_err(output_error)?,
            )
            .take(32769)
            .read_to_end(&mut bytes)
            .map_err(output_error)?;
            let old: ResourceCandidate = serde_json::from_slice(&bytes).map_err(output_error)?;
            if old.schema_version != 1
                || old.visibility != "execution"
                || old.credential_scope != candidate.credential_scope
                || old.resource_id != candidate.resource_id
                || old.url != candidate.url
                || old.kind != candidate.kind
            {
                return Err(output_error("outbox identity conflict"));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_private(path, &serde_json::to_vec(candidate).map_err(output_error)?)
        }
        Err(error) => Err(output_error(error)),
    }
}

fn output_error(error: impl std::fmt::Display) -> CrawlError {
    CrawlError::Output(error.to_string())
}

#[cfg(test)]
#[path = "resource_tests.rs"]
mod tests;
