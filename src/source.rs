use crate::planner::validate_source_paths;
use crate::safe_fs::SafeRoot;
use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const MAX_BLOB_SIZE: u64 = 64 * 1024 * 1024;
const MAX_API_JSON_SIZE: u64 = 32 * 1024 * 1024;
const MAX_TREE_REQUESTS: usize = 2048;
const MAX_TREE_DEPTH: usize = 64;
const MAX_TAR_METADATA: u64 = 1024 * 1024;
const MAX_BLOB_FALLBACKS: usize = 512;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(15 * 60);
const DISK_RESERVE: u64 = 128 * 1024 * 1024;

struct FetchBudget {
    started: Instant,
    fallbacks: usize,
}

impl FetchBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            fallbacks: 0,
        }
    }

    fn check(&self) -> Result<()> {
        if self.started.elapsed() >= SNAPSHOT_DEADLINE {
            bail!("source snapshot exceeded 15-minute deadline");
        }
        Ok(())
    }

    fn remaining(&self) -> Result<Duration> {
        self.check()?;
        Ok(SNAPSHOT_DEADLINE
            .saturating_sub(self.started.elapsed())
            .min(Duration::from_secs(180)))
    }

    fn fallback(&mut self) -> Result<()> {
        self.check()?;
        if self.fallbacks >= MAX_BLOB_FALLBACKS {
            bail!("source snapshot exceeds Git blob fallback request limit");
        }
        self.fallbacks += 1;
        Ok(())
    }
}

#[derive(Default)]
struct TreeBudget {
    bytes: u64,
    requests: usize,
}

struct CappedReader<R> {
    inner: R,
    remaining: u64,
}

struct TarMetadataGuard<R> {
    inner: R,
    block: [u8; 512],
    offset: usize,
    available: usize,
    payload_blocks: u64,
    pax_data: Option<Vec<u8>>,
    pax_remaining: u64,
    pax_global: bool,
    local_size: Option<u64>,
    global_size: Option<u64>,
}

impl<R: Read> TarMetadataGuard<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            block: [0; 512],
            offset: 0,
            available: 0,
            payload_blocks: 0,
            pax_data: None,
            pax_remaining: 0,
            pax_global: false,
            local_size: None,
            global_size: None,
        }
    }
}

impl<R: Read> Read for TarMetadataGuard<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.offset == self.available {
            let first = self.inner.read(&mut self.block[..1])?;
            if first == 0 {
                return Ok(0);
            }
            self.inner.read_exact(&mut self.block[1..])?;
            self.offset = 0;
            self.available = 512;
            if self.payload_blocks == 0 && self.block.iter().any(|byte| *byte != 0) {
                let header = tar::Header::from_byte_slice(&self.block);
                let size = header.size()?;
                let kind = header.entry_type();
                if (kind.is_pax_global_extensions()
                    || kind.is_pax_local_extensions()
                    || kind.is_gnu_longname()
                    || kind.is_gnu_longlink())
                    && size > MAX_TAR_METADATA
                {
                    return Err(io::Error::other(
                        "source archive metadata exceeds 1 MiB limit",
                    ));
                }
                if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
                    self.pax_data = Some(Vec::with_capacity(size as usize));
                    self.pax_remaining = size;
                    self.pax_global = kind.is_pax_global_extensions();
                    if size == 0 {
                        self.pax_data = None;
                    }
                }
                let extension = kind.is_pax_global_extensions()
                    || kind.is_pax_local_extensions()
                    || kind.is_gnu_longname()
                    || kind.is_gnu_longlink();
                let effective_size = if !extension {
                    self.local_size.take().or(self.global_size).unwrap_or(size)
                } else {
                    size
                };
                self.payload_blocks = effective_size.div_ceil(512);
            } else if self.payload_blocks > 0 {
                if let Some(data) = &mut self.pax_data {
                    let count = self.pax_remaining.min(512) as usize;
                    data.extend_from_slice(&self.block[..count]);
                    self.pax_remaining -= count as u64;
                    if self.pax_remaining == 0 {
                        let size = pax_size(data)?;
                        if self.pax_global {
                            if size.is_some() {
                                self.global_size = size;
                            }
                        } else {
                            self.local_size = size;
                        }
                        self.pax_data = None;
                    }
                }
                self.payload_blocks -= 1;
            }
        }
        let count = output.len().min(self.available - self.offset);
        output[..count].copy_from_slice(&self.block[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

fn pax_size(data: &[u8]) -> io::Result<Option<u64>> {
    let mut size = None;
    for extension in tar::PaxExtensions::new(data) {
        let extension = extension?;
        if extension.key().map_err(io::Error::other)? == "size" {
            size = Some(
                extension
                    .value()
                    .map_err(io::Error::other)?
                    .parse::<u64>()
                    .map_err(io::Error::other)?,
            );
        }
    }
    Ok(size)
}

impl<R: Read> Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return match self.inner.read(&mut [0u8; 1])? {
                0 => Ok(0),
                _ => Err(io::Error::other("source archive exceeds stream limit")),
            };
        }
        let limit = (self.remaining.min(buf.len() as u64)) as usize;
        let count = self.inner.read(&mut buf[..limit])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[derive(Clone, Copy)]
pub struct SourceLimits {
    pub max_snapshot_bytes: u64,
    pub max_files: usize,
}

impl Default for SourceLimits {
    fn default() -> Self {
        Self {
            max_snapshot_bytes: 1024 * 1024 * 1024,
            max_files: 100_000,
        }
    }
}

pub struct GitHub {
    client: Client,
    api: reqwest::Url,
    raw: reqwest::Url,
    owner: String,
    repo: String,
    token: Option<String>,
    force_blob_api: bool,
    limits: SourceLimits,
}

pub struct Snapshot {
    pub commit_id: String,
    pub root: TempDir,
    pub file_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct TreeEntry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct TreeResponse {
    truncated: bool,
    tree: Vec<TreeEntry>,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
    commit: CommitData,
}

#[derive(Deserialize)]
struct ReleaseTag {
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize)]
struct CommitData {
    tree: TreeSha,
}

#[derive(Deserialize)]
struct TreeSha {
    sha: String,
}

#[derive(Deserialize)]
struct BlobResponse {
    sha: String,
    size: u64,
    encoding: String,
    content: String,
}

impl GitHub {
    /// Reuse a persisted snapshot only after checking every cached file against
    /// the authoritative Git tree for the resolved full commit SHA.
    pub fn fetch_snapshot_cached(&self, reference: &str, cache_path: &Path) -> Result<Snapshot> {
        if reference.is_empty() || reference.chars().any(char::is_control) {
            bail!("invalid target reference");
        }
        let budget = FetchBudget::new();
        let commit: CommitResponse = self.api_json(
            &["repos", &self.owner, &self.repo, "commits", reference],
            &[],
            &budget,
        )?;
        validate_sha(&commit.sha)?;
        validate_sha(&commit.commit.tree.sha)?;
        let files = self.fetch_tree(&commit.commit.tree.sha, &budget)?;
        self.check_budget(&files)?;
        let cache_root = SafeRoot::open(cache_path)?;
        let cached_path = cache_path.join(&commit.sha);
        if let Ok(cached) = SafeRoot::open(&cached_path)
            && let Ok(paths) = cached.file_paths()
            && paths.len() == files.len()
            && paths.iter().all(|path| files.contains_key(path))
        {
            let output = TempDir::new()?;
            let output_root = SafeRoot::open(output.path())?;
            let mut valid = true;
            for (path, entry) in &files {
                let bytes = match cached.required_file(path) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        valid = false;
                        break;
                    }
                };
                if entry.size.is_some_and(|size| size != bytes.len() as u64)
                    || git_blob_sha(&bytes) != entry.sha
                {
                    valid = false;
                    break;
                }
                output_root.write_atomic(path, bytes.as_slice())?;
                #[cfg(unix)]
                output_root
                    .set_unix_mode(path, if entry.mode == "100755" { 0o755 } else { 0o644 })?;
            }
            if valid {
                return Ok(Snapshot {
                    commit_id: commit.sha,
                    root: output,
                    file_count: files.len(),
                });
            }
        }
        let snapshot = self.fetch_snapshot(&commit.sha)?;
        if cache_root.ensure_dir(&commit.sha).is_ok()
            && let Ok(cached) = SafeRoot::open(&cached_path)
            && let Ok(paths) = cached.file_paths()
        {
            for path in paths {
                if !files.contains_key(&path) {
                    let _ = cached.remove_regular(&path);
                }
            }
            if let Ok(snapshot_root) = SafeRoot::open(snapshot.root.path()) {
                for path in files.keys() {
                    if snapshot_root.copy_regular_to(path, &cached, path).is_err() {
                        break;
                    }
                }
            }
        }
        Ok(snapshot)
    }
    pub fn new(repository: &str, token: Option<String>) -> Result<Self> {
        Self::with_api(repository, token, "https://api.github.com/")
    }

    pub fn with_api(repository: &str, token: Option<String>, api: &str) -> Result<Self> {
        let mut source =
            Self::with_endpoints(repository, token, api, "https://raw.githubusercontent.com/")?;
        source.force_blob_api = api != "https://api.github.com/";
        Ok(source)
    }

    fn with_endpoints(
        repository: &str,
        token: Option<String>,
        api: &str,
        raw: &str,
    ) -> Result<Self> {
        let (owner, repo) = repository
            .split_once('/')
            .context("repository must be owner/name")?;
        if !valid_repo_part(owner) || !valid_repo_part(repo) {
            bail!("repository must be a GitHub owner/name without extra path components");
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            client,
            api: reqwest::Url::parse(api)?,
            raw: reqwest::Url::parse(raw)?,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            token,
            force_blob_api: false,
            limits: SourceLimits::default(),
        })
    }

    pub fn set_limits(&mut self, limits: SourceLimits) -> Result<()> {
        if limits.max_snapshot_bytes == 0 || limits.max_files == 0 {
            bail!("source limits must be positive");
        }
        self.limits = limits;
        Ok(())
    }

    /// List recent published releases for interactive target selection.
    pub fn release_tags(&self) -> Result<Vec<String>> {
        let releases: Vec<ReleaseTag> = self.api_json(
            &["repos", &self.owner, &self.repo, "releases"],
            &[("per_page", "30")],
            &FetchBudget::new(),
        )?;
        let mut tags = Vec::new();
        for release in releases {
            if !release.draft
                && !release.tag_name.is_empty()
                && !release.tag_name.chars().any(char::is_control)
                && !tags.contains(&release.tag_name)
            {
                tags.push(release.tag_name);
            }
        }
        Ok(tags)
    }

    pub fn fetch_snapshot(&self, reference: &str) -> Result<Snapshot> {
        if reference.is_empty() || reference.chars().any(char::is_control) {
            bail!("invalid target reference");
        }
        let mut budget = FetchBudget::new();
        let commit: CommitResponse = self.api_json(
            &["repos", &self.owner, &self.repo, "commits", reference],
            &[],
            &budget,
        )?;
        validate_sha(&commit.sha)?;
        validate_sha(&commit.commit.tree.sha)?;
        budget.check()?;
        let files = self.fetch_tree(&commit.commit.tree.sha, &budget)?;
        let expected_bytes = self.check_budget(&files)?;
        let root = TempDir::new().context("creating source snapshot directory")?;
        let required_space = required_disk_space(expected_bytes, files.keys().map(String::as_str))?;
        let available = fs2::available_space(root.path())?;
        if required_space > available {
            bail!(
                "insufficient temporary disk space: need {required_space} bytes, available {available}"
            );
        }
        self.download_archive(&commit.sha, &files, root.path(), &mut budget)?;
        budget.check()?;
        Ok(Snapshot {
            commit_id: commit.sha,
            root,
            file_count: files.len(),
        })
    }

    fn check_budget(&self, files: &BTreeMap<String, TreeEntry>) -> Result<u64> {
        if files.len() > self.limits.max_files {
            bail!(
                "source tree has {} files, exceeding configured limit of {}",
                files.len(),
                self.limits.max_files
            );
        }
        let mut total = 0u64;
        for (path, entry) in files {
            let size = entry
                .size
                .with_context(|| format!("Git tree omits file size: {path}"))?;
            if size > MAX_BLOB_SIZE {
                bail!("source file exceeds 64 MiB: {path}");
            }
            total = total
                .checked_add(size)
                .context("source tree byte count overflow")?;
            if total > self.limits.max_snapshot_bytes {
                bail!("source tree exceeds configured snapshot byte limit");
            }
        }
        Ok(total)
    }

    fn fetch_tree(
        &self,
        root_sha: &str,
        fetch: &FetchBudget,
    ) -> Result<BTreeMap<String, TreeEntry>> {
        fetch.check()?;
        let response: TreeResponse = self.api_json(
            &["repos", &self.owner, &self.repo, "git", "trees", root_sha],
            &[("recursive", "1")],
            fetch,
        )?;
        let mut files = BTreeMap::new();
        let mut budget = TreeBudget {
            requests: 1,
            ..TreeBudget::default()
        };
        if response.truncated {
            self.walk_tree(root_sha, "", 0, &mut files, &mut budget, fetch)?;
        } else {
            for entry in response.tree {
                if entry.kind == "tree" {
                    continue;
                }
                insert_file(&mut files, entry, &self.limits, &mut budget)?;
            }
        }
        validate_source_paths(files.keys().map(String::as_str))?;
        Ok(files)
    }

    fn walk_tree(
        &self,
        tree_sha: &str,
        prefix: &str,
        depth: usize,
        files: &mut BTreeMap<String, TreeEntry>,
        budget: &mut TreeBudget,
        fetch: &FetchBudget,
    ) -> Result<()> {
        fetch.check()?;
        if depth > MAX_TREE_DEPTH || budget.requests >= MAX_TREE_REQUESTS {
            bail!("Git tree traversal exceeds depth or request limit");
        }
        budget.requests += 1;
        let response: TreeResponse = self.api_json(
            &["repos", &self.owner, &self.repo, "git", "trees", tree_sha],
            &[],
            fetch,
        )?;
        if response.truncated {
            bail!("GitHub non-recursive tree is truncated: {tree_sha}");
        }
        for mut entry in response.tree {
            entry.path = if prefix.is_empty() {
                entry.path
            } else {
                format!("{prefix}/{}", entry.path)
            };
            if entry.kind == "tree" {
                validate_sha(&entry.sha)?;
                validate_source_paths([entry.path.as_str()])?;
                self.walk_tree(&entry.sha, &entry.path, depth + 1, files, budget, fetch)?;
            } else {
                insert_file(files, entry, &self.limits, budget)?;
            }
        }
        Ok(())
    }

    fn download_archive(
        &self,
        commit: &str,
        files: &BTreeMap<String, TreeEntry>,
        destination: &Path,
        fetch: &mut FetchBudget,
    ) -> Result<()> {
        fetch.check()?;
        let url = self.api_url(&["repos", &self.owner, &self.repo, "tarball", commit], &[])?;
        let response = self.send(url, fetch)?;
        let archive_limit = archive_stream_limit(
            self.limits.max_snapshot_bytes,
            files.keys().map(String::as_str),
        )?;
        let compressed = CappedReader {
            inner: response,
            remaining: archive_limit,
        };
        let decoder = GzDecoder::new(compressed);
        let expanded = CappedReader {
            inner: decoder,
            remaining: archive_limit,
        };
        let mut archive = tar::Archive::new(TarMetadataGuard::new(expanded));
        let mut seen = BTreeSet::new();
        let mut pending_blobs = BTreeSet::new();
        let mut written = 0u64;
        let mut archive_prefix: Option<PathBuf> = None;
        for entry in archive.entries()? {
            fetch.check()?;
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            if kind.is_pax_global_extensions()
                || kind.is_pax_local_extensions()
                || kind.is_gnu_longname()
                || kind.is_gnu_longlink()
            {
                continue;
            }
            let raw_path = entry.path()?.into_owned();
            let mut components = raw_path.components();
            let first = match components.next() {
                Some(Component::Normal(first)) => PathBuf::from(first),
                _ => bail!("unsafe archive root: {}", raw_path.display()),
            };
            let original_prefix = archive_prefix.get_or_insert_with(|| first.clone());
            if original_prefix != &first {
                bail!(
                    "archive contains multiple roots: {} and {} at {}",
                    original_prefix.display(),
                    first.display(),
                    raw_path.display()
                );
            }
            let relative: PathBuf = components.collect();
            if relative.as_os_str().is_empty() {
                continue;
            }
            let path = relative
                .to_str()
                .context("non-UTF-8 archive path")?
                .replace('\\', "/");
            validate_source_paths([path.as_str()])?;
            if entry.header().entry_type().is_dir() {
                continue;
            }
            if !entry.header().entry_type().is_file() {
                bail!("archive contains unsupported object: {path}");
            }
            let expected = files
                .get(&path)
                .with_context(|| format!("archive has untracked file: {path}"))?;
            if !seen.insert(path.clone()) {
                bail!("duplicate archive file: {path}");
            }
            let archive_size = entry.size();
            if archive_size > MAX_BLOB_SIZE
                || expected
                    .size
                    .is_some_and(|size| archive_size > size.saturating_add(1024 * 1024))
            {
                // GitHub may expand an LFS pointer in the archive. Stop reading
                // this representation and retrieve the original Git blob by SHA.
                pending_blobs.insert(path);
                let drained = match io::copy(&mut entry, &mut io::sink()) {
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error).context("draining expanded archive entry"),
                };
                if drained != archive_size {
                    // GitHub sometimes ends a transformed representation early.
                    // Fall back to each original Git blob after the archive is dropped.
                    break;
                }
                continue;
            }
            let mut bytes = Vec::new();
            entry
                .by_ref()
                .take(MAX_BLOB_SIZE + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BLOB_SIZE {
                bail!("source file exceeds 64 MiB: {path}");
            }
            if expected.size.is_some_and(|size| size != bytes.len() as u64)
                || git_blob_sha(&bytes) != expected.sha
            {
                pending_blobs.insert(path);
                continue;
            }
            written = written
                .checked_add(bytes.len() as u64)
                .context("snapshot byte count overflow")?;
            if written > self.limits.max_snapshot_bytes {
                bail!("downloaded source exceeds configured snapshot byte limit");
            }
            ensure_disk_space(destination, &path, bytes.len() as u64)?;
            write_snapshot_file(destination, &path, expected, &bytes)?;
            ensure_disk_reserve(destination)?;
        }
        drop(archive);
        if seen.len() != files.len() || !pending_blobs.is_empty() {
            for (path, expected) in files {
                if !seen.contains(path) || pending_blobs.contains(path) {
                    let bytes = self.fetch_raw_blob(commit, path, expected, fetch)?;
                    written = written
                        .checked_add(bytes.len() as u64)
                        .context("snapshot byte count overflow")?;
                    if written > self.limits.max_snapshot_bytes {
                        bail!("downloaded source exceeds configured snapshot byte limit");
                    }
                    ensure_disk_space(destination, path, bytes.len() as u64)?;
                    write_snapshot_file(destination, path, expected, &bytes)?;
                    ensure_disk_reserve(destination)?;
                }
            }
        }
        Ok(())
    }

    fn fetch_raw_blob(
        &self,
        commit: &str,
        path: &str,
        expected: &TreeEntry,
        fetch: &mut FetchBudget,
    ) -> Result<Vec<u8>> {
        fetch.fallback()?;
        let expected_size = expected.size.context("Git tree omits blob size")?;
        if expected_size > MAX_BLOB_SIZE {
            bail!("source file exceeds 64 MiB: {path}");
        }
        if self.token.is_some() || self.force_blob_api {
            let url = self.api_url(
                &[
                    "repos",
                    &self.owner,
                    &self.repo,
                    "git",
                    "blobs",
                    &expected.sha,
                ],
                &[],
            )?;
            let encoded_size = expected_size
                .div_ceil(3)
                .checked_mul(4)
                .context("blob size overflow")?;
            let response_limit = encoded_size
                .checked_add(encoded_size / 30)
                .and_then(|size| size.checked_add(65_536))
                .context("blob response size overflow")?;
            let body = read_response_limited(self.send(url, fetch)?, response_limit)?;
            let mut blob: BlobResponse =
                serde_json::from_slice(&body).context("parsing Git blob API response")?;
            if blob.sha != expected.sha || blob.size > MAX_BLOB_SIZE || blob.encoding != "base64" {
                bail!("Git blob API returned invalid metadata for {path}");
            }
            blob.content.retain(|ch| !ch.is_ascii_whitespace());
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&blob.content)
                .with_context(|| format!("decoding Git blob for {path}"))?;
            if bytes.len() as u64 != blob.size
                || expected.size.is_some_and(|size| size != blob.size)
                || git_blob_sha(&bytes) != expected.sha
            {
                bail!("Git blob verification failed: {path}");
            }
            return Ok(bytes);
        }
        let mut url = self.raw.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid raw URL"))?
            .pop_if_empty()
            .extend([self.owner.as_str(), self.repo.as_str(), commit])
            .extend(path.split('/'));
        let mut response = self
            .client
            .get(url)
            .header(USER_AGENT, "minecraft-modpack-updater/0.1")
            .timeout(fetch.remaining()?)
            .send()
            .with_context(|| format!("fetching Git blob for {path}"))?;
        if !response.status().is_success() {
            bail!(
                "fetching Git blob for {path} failed with HTTP {}",
                response.status()
            );
        }
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(MAX_BLOB_SIZE + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BLOB_SIZE
            || expected.size.is_some_and(|size| size != bytes.len() as u64)
            || git_blob_sha(&bytes) != expected.sha
        {
            bail!("Git blob verification failed: {path}");
        }
        Ok(bytes)
    }

    fn api_json<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
        fetch: &FetchBudget,
    ) -> Result<T> {
        let url = self.api_url(segments, query)?;
        let body = read_response_limited(self.send(url, fetch)?, MAX_API_JSON_SIZE)?;
        serde_json::from_slice(&body).context("parsing GitHub API response")
    }

    fn api_url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<reqwest::Url> {
        let mut url = self.api.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid API URL"))?
            .pop_if_empty()
            .extend(segments);
        url.query_pairs_mut().extend_pairs(query);
        Ok(url)
    }

    fn send(&self, url: reqwest::Url, fetch: &FetchBudget) -> Result<Response> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("minecraft-modpack-updater/0.1"),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        if let Some(token) = &self.token {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
        }
        let response = self
            .client
            .get(url)
            .headers(headers)
            .timeout(fetch.remaining()?)
            .send()
            .context("requesting GitHub")?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        if matches!(status.as_u16(), 403 | 429)
            && let Some(reset) = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
        {
            let retry = DateTime::<Utc>::from_timestamp(reset, 0)
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| reset.to_string());
            bail!(
                "GitHub API rate limited; retry after {retry} or provide GITHUB_TOKEN / --token-stdin"
            );
        }
        bail!("GitHub request failed with HTTP {status}");
    }
}

fn read_response_limited(mut response: Response, limit: u64) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("GitHub response exceeds configured size limit");
    }
    let mut bytes = Vec::new();
    response.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("GitHub response exceeds configured size limit");
    }
    Ok(bytes)
}

fn ensure_disk_reserve(path: &Path) -> Result<()> {
    let available = fs2::available_space(path)?;
    if available < DISK_RESERVE {
        bail!("temporary disk space fell below safety reserve");
    }
    Ok(())
}

fn ensure_disk_space(root: &Path, relative: &str, bytes: u64) -> Result<()> {
    let overhead = (relative.split('/').count() as u64)
        .checked_mul(8192)
        .context("temporary disk estimate overflow")?;
    let required = bytes
        .checked_add(overhead)
        .and_then(|total| total.checked_add(DISK_RESERVE))
        .context("temporary disk estimate overflow")?;
    let available = fs2::available_space(root)?;
    if available < required {
        bail!(
            "insufficient temporary disk space for {relative}: need {required}, available {available}"
        );
    }
    Ok(())
}

fn required_disk_space<'a>(bytes: u64, paths: impl IntoIterator<Item = &'a str>) -> Result<u64> {
    let mut files = 0u64;
    let mut directories = BTreeSet::new();
    for path in paths {
        files += 1;
        let mut prefix = String::new();
        for part in path
            .split('/')
            .take(path.split('/').count().saturating_sub(1))
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            directories.insert(prefix.clone());
        }
    }
    bytes
        .checked_add(files.saturating_mul(8192))
        .and_then(|total| total.checked_add((directories.len() as u64).saturating_mul(8192)))
        .and_then(|total| total.checked_add(DISK_RESERVE))
        .context("temporary disk space estimate overflow")
}

fn archive_stream_limit<'a>(bytes: u64, paths: impl IntoIterator<Item = &'a str>) -> Result<u64> {
    let mut files = 0u64;
    let mut directories = BTreeSet::new();
    for path in paths {
        files += 1;
        for (index, _) in path.match_indices('/') {
            directories.insert(&path[..index]);
        }
    }
    bytes
        .checked_add(files.saturating_mul(4096))
        .and_then(|total| total.checked_add((directories.len() as u64).saturating_mul(4096)))
        .and_then(|total| total.checked_add(16 * 1024 * 1024))
        .context("source archive limit overflow")
}

fn write_snapshot_file(
    destination: &Path,
    path: &str,
    _expected: &TreeEntry,
    bytes: &[u8],
) -> Result<()> {
    let output = destination.join(path);
    fs::create_dir_all(output.parent().unwrap())?;
    fs::write(&output, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if _expected.mode == "100755" {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(&output, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn insert_file(
    files: &mut BTreeMap<String, TreeEntry>,
    entry: TreeEntry,
    limits: &SourceLimits,
    budget: &mut TreeBudget,
) -> Result<()> {
    if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
        bail!(
            "unsupported Git object at {} ({} {})",
            entry.path,
            entry.kind,
            entry.mode
        );
    }
    validate_sha(&entry.sha)?;
    validate_source_paths([entry.path.as_str()])?;
    if entry.path.split('/').count() > MAX_TREE_DEPTH + 1 {
        bail!(
            "Git source path exceeds directory depth limit: {}",
            entry.path
        );
    }
    if files.len() >= limits.max_files {
        bail!("source tree exceeds configured file count limit");
    }
    let size = entry.size.context("Git tree omits blob size")?;
    if size > MAX_BLOB_SIZE {
        bail!("source file exceeds 64 MiB: {}", entry.path);
    }
    budget.bytes = budget
        .bytes
        .checked_add(size)
        .context("source tree byte count overflow")?;
    if budget.bytes > limits.max_snapshot_bytes {
        bail!("source tree exceeds configured snapshot byte limit");
    }
    if files.insert(entry.path.clone(), entry).is_some() {
        bail!("duplicate Git tree path");
    }
    Ok(())
}

fn validate_sha(sha: &str) -> Result<()> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("GitHub returned invalid Git SHA");
    }
    Ok(())
}

fn valid_repo_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub fn git_blob_sha(bytes: &[u8]) -> String {
    let mut sha = Sha1::new();
    sha.update(format!("blob {}\0", bytes.len()).as_bytes());
    sha.update(bytes);
    format!("{:x}", sha.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve(responses: Vec<(&'static str, Vec<u8>)>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for (expected, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let count = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..count])
                        .to_ascii_lowercase()
                        .contains(&expected.to_ascii_lowercase())
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (base, handle)
    }

    fn archive(path: &str, content: &[u8]) -> Vec<u8> {
        archive_many(&[(path, content)])
    }

    fn archive_many(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gzip = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(gzip);
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(content.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, path, *content).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn release_tags_keep_published_unique_targets() {
        let releases = serde_json::json!([
            {"tag_name":"v2-test2","draft":false},
            {"tag_name":"v2-test1","draft":false},
            {"tag_name":"v2-test2","draft":false},
            {"tag_name":"draft","draft":true},
            {"tag_name":"bad\nname","draft":false}
        ]);
        let (base, server) = serve(vec![(
            "/releases?per_page=30",
            serde_json::to_vec(&releases).unwrap(),
        )]);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        assert_eq!(github.release_tags().unwrap(), ["v2-test2", "v2-test1"]);
        server.join().unwrap();
    }

    #[test]
    fn git_blob_hash_and_repo_validation() {
        assert_eq!(
            git_blob_sha(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert!(GitHub::new("owner/repo", None).is_ok());
        assert!(GitHub::new("owner/repo/extra", None).is_err());
        for base in ["https://host/api/v3/", "https://host/api/v3"] {
            let github = GitHub::with_api("owner/repo", None, base).unwrap();
            assert_eq!(
                github
                    .api_url(&["repos", "owner", "repo"], &[])
                    .unwrap()
                    .path(),
                "/api/v3/repos/owner/repo"
            );
        }
    }

    #[test]
    fn cached_snapshot_is_reused_only_after_git_blob_verification() {
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        let content = b"trusted";
        let commit_body = serde_json::json!({"sha":commit,"commit":{"tree":{"sha":tree}}})
            .to_string()
            .into_bytes();
        let tree_body = serde_json::json!({"truncated":false,"tree":[{"path":"file.txt","mode":"100755","type":"blob","sha":git_blob_sha(content),"size":content.len()}]}).to_string().into_bytes();
        let archive_body = archive("prefix/file.txt", content);
        let mut responses = Vec::new();
        for fetch in 0..3 {
            responses.push(("/commits/", commit_body.clone()));
            responses.push(("/git/trees/", tree_body.clone()));
            if fetch != 1 {
                responses.push(("/commits/", commit_body.clone()));
                responses.push(("/git/trees/", tree_body.clone()));
                responses.push(("/tarball/", archive_body.clone()));
            }
        }
        let (base, server) = serve(responses);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        let cache = TempDir::new().unwrap();
        let first = github.fetch_snapshot_cached(&commit, cache.path()).unwrap();
        assert_eq!(
            fs::read(first.root.path().join("file.txt")).unwrap(),
            content
        );
        let second = github.fetch_snapshot_cached(&commit, cache.path()).unwrap();
        assert_eq!(
            fs::read(second.root.path().join("file.txt")).unwrap(),
            content
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(second.root.path().join("file.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
        }
        fs::write(cache.path().join(&commit).join("file.txt"), b"tampered").unwrap();
        let third = github.fetch_snapshot_cached(&commit, cache.path()).unwrap();
        assert_eq!(
            fs::read(third.root.path().join("file.txt")).unwrap(),
            content
        );
        server.join().unwrap();
    }

    #[test]
    fn truncated_tree_falls_back_to_child_trees() {
        let root = "a".repeat(40);
        let child = "b".repeat(40);
        let blob = git_blob_sha(b"hello\n");
        let recursive = serde_json::json!({"truncated":true,"tree":[]});
        let top = serde_json::json!({"truncated":false,"tree":[{"path":"dir","mode":"040000","type":"tree","sha":child}]});
        let nested = serde_json::json!({"truncated":false,"tree":[{"path":"file.txt","mode":"100644","type":"blob","sha":blob,"size":6}]});
        let (base, server) = serve(vec![
            ("recursive=1", recursive.to_string().into_bytes()),
            ("/git/trees/", top.to_string().into_bytes()),
            ("/git/trees/", nested.to_string().into_bytes()),
        ]);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        let files = github.fetch_tree(&root, &FetchBudget::new()).unwrap();
        assert!(files.contains_key("dir/file.txt"));
        server.join().unwrap();
    }

    #[test]
    fn archive_export_transform_uses_verified_raw_blob() {
        let commit = "a".repeat(40);
        let original = b"echo hello\n";
        let transformed = b"echo hello\r\n";
        let mut files = BTreeMap::new();
        files.insert(
            "devtool.bat".into(),
            TreeEntry {
                path: "devtool.bat".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(original),
                size: Some(original.len() as u64),
            },
        );
        let (base, server) = serve(vec![
            ("/tarball/", archive("prefix/devtool.bat", transformed)),
            ("/devtool.bat", original.to_vec()),
        ]);
        let github = GitHub::with_endpoints("owner/repo", None, &base, &base).unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(&commit, &files, output.path(), &mut FetchBudget::new())
            .unwrap();
        assert_eq!(
            fs::read(output.path().join("devtool.bat")).unwrap(),
            original
        );
        server.join().unwrap();
    }

    #[test]
    fn private_blob_fallback_sends_token_to_git_api() {
        let content = b"private\n";
        let sha = git_blob_sha(content);
        let entry = TreeEntry {
            path: "file.txt".into(),
            mode: "100644".into(),
            kind: "blob".into(),
            sha: sha.clone(),
            size: Some(content.len() as u64),
        };
        let body = serde_json::json!({
            "sha":sha,"size":content.len(),"encoding":"base64",
            "content":base64::engine::general_purpose::STANDARD.encode(content),
        });
        let (base, server) = serve(vec![(
            "authorization: bearer test-secret",
            body.to_string().into_bytes(),
        )]);
        let github =
            GitHub::with_endpoints("owner/repo", Some("test-secret".into()), &base, &base).unwrap();
        assert_eq!(
            github
                .fetch_raw_blob(&"a".repeat(40), "file.txt", &entry, &mut FetchBudget::new())
                .unwrap(),
            content
        );
        server.join().unwrap();
    }

    #[test]
    fn custom_api_uses_its_blob_endpoint_without_token() {
        let content = b"custom";
        let sha = git_blob_sha(content);
        let body = serde_json::json!({"sha":sha,"size":content.len(),"encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(content)});
        let (base, server) = serve(vec![("/git/blobs/", body.to_string().into_bytes())]);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        let entry = TreeEntry {
            path: "custom".into(),
            mode: "100644".into(),
            kind: "blob".into(),
            sha,
            size: Some(content.len() as u64),
        };
        assert_eq!(
            github
                .fetch_raw_blob(&"a".repeat(40), "custom", &entry, &mut FetchBudget::new())
                .unwrap(),
            content
        );
        server.join().unwrap();
    }

    #[test]
    fn tree_budget_rejects_file_count_and_bytes() {
        let mut github = GitHub::new("owner/repo", None).unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            "a".into(),
            TreeEntry {
                path: "a".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(b"a"),
                size: Some(1),
            },
        );
        files.insert(
            "b".into(),
            TreeEntry {
                path: "b".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(b"b"),
                size: Some(1),
            },
        );
        github
            .set_limits(SourceLimits {
                max_snapshot_bytes: 10,
                max_files: 1,
            })
            .unwrap();
        assert!(
            github
                .check_budget(&files)
                .unwrap_err()
                .to_string()
                .contains("files")
        );
        github
            .set_limits(SourceLimits {
                max_snapshot_bytes: 1,
                max_files: 2,
            })
            .unwrap();
        assert!(
            github
                .check_budget(&files)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
    }

    #[test]
    fn traversal_budget_rejects_before_collecting_more_files() {
        let mut files = BTreeMap::new();
        let mut budget = TreeBudget::default();
        let limits = SourceLimits {
            max_snapshot_bytes: 1,
            max_files: 1,
        };
        let first = TreeEntry {
            path: "first".into(),
            mode: "100644".into(),
            kind: "blob".into(),
            sha: git_blob_sha(b"a"),
            size: Some(1),
        };
        let second = TreeEntry {
            path: "second".into(),
            mode: "100644".into(),
            kind: "blob".into(),
            sha: git_blob_sha(b"a"),
            size: Some(1),
        };
        insert_file(&mut files, first, &limits, &mut budget).unwrap();
        assert!(insert_file(&mut files, second, &limits, &mut budget).is_err());
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn nested_file_and_small_files_require_disk_overhead() {
        let estimate = required_disk_space(2, ["a/one", "b/two"]).unwrap();
        assert!(estimate >= DISK_RESERVE + 4 * 8192 + 2);
        let mut files = BTreeMap::new();
        let mut budget = TreeBudget::default();
        let path = format!("{}file", "d/".repeat(MAX_TREE_DEPTH + 1));
        let entry = TreeEntry {
            path,
            mode: "100644".into(),
            kind: "blob".into(),
            sha: git_blob_sha(b""),
            size: Some(0),
        };
        assert!(
            insert_file(&mut files, entry, &SourceLimits::default(), &mut budget)
                .unwrap_err()
                .to_string()
                .contains("depth")
        );
        let nested: Vec<String> = (0..1000)
            .map(|index| format!("group{index}/{}/file", "nested/".repeat(32)))
            .collect();
        let limit = archive_stream_limit(1024, nested.iter().map(String::as_str)).unwrap();
        assert!(limit > 1024 + 1000 * 32 * 4096);
    }

    #[test]
    fn missing_archive_files_have_shared_fallback_limit() {
        let mut fetch = FetchBudget::new();
        for _ in 0..MAX_BLOB_FALLBACKS {
            fetch.fallback().unwrap();
        }
        assert!(
            fetch
                .fallback()
                .unwrap_err()
                .to_string()
                .contains("fallback request limit")
        );
    }

    #[test]
    fn archive_stream_limit_rejects_skipped_payload() {
        let payload = vec![0u8; 17 * 1024 * 1024];
        let gzip = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(payload.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "prefix/ignored/", payload.as_slice())
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_cksum();
        tar.append_data(&mut header, "prefix/one", b"one".as_slice())
            .unwrap();
        let compressed = tar.into_inner().unwrap().finish().unwrap();
        let (base, server) = serve(vec![("/tarball/", compressed)]);
        let mut github = GitHub::with_api("owner/repo", None, &base).unwrap();
        github
            .set_limits(SourceLimits {
                max_snapshot_bytes: 3,
                max_files: 1,
            })
            .unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            "one".into(),
            TreeEntry {
                path: "one".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(b"one"),
                size: Some(3),
            },
        );
        let output = TempDir::new().unwrap();
        let error = github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("stream limit"));
        server.join().unwrap();
    }

    #[test]
    fn expanded_tracked_payload_cannot_bypass_archive_stream_limit() {
        let original = b"ptr";
        let expanded = vec![0u8; 17 * 1024 * 1024];
        let compressed = archive("prefix/pointer", &expanded);
        let (base, server) = serve(vec![("/tarball/", compressed)]);
        let mut github = GitHub::with_api("owner/repo", None, &base).unwrap();
        github
            .set_limits(SourceLimits {
                max_snapshot_bytes: 3,
                max_files: 1,
            })
            .unwrap();
        let files = BTreeMap::from([(
            "pointer".into(),
            TreeEntry {
                path: "pointer".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(original),
                size: Some(3),
            },
        )]);
        let output = TempDir::new().unwrap();
        let error = github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("stream limit"), "{error:#}");
        server.join().unwrap();
    }

    #[test]
    fn oversized_pax_metadata_is_rejected_before_tar_buffers_it() {
        let payload = vec![b'x'; (MAX_TAR_METADATA + 1) as usize];
        let gzip = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::XHeader);
        header.set_size(payload.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "pax-header", payload.as_slice())
            .unwrap();
        let bytes = builder.into_inner().unwrap().finish().unwrap();
        let (base, server) = serve(vec![("/tarball/", bytes)]);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        let output = TempDir::new().unwrap();
        let error = github
            .download_archive(
                &"a".repeat(40),
                &BTreeMap::new(),
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("metadata exceeds"),
            "{error:#}"
        );
        server.join().unwrap();
    }

    #[test]
    fn oversized_archive_representation_falls_back_to_original_blob() {
        let original = b"ptr";
        let commit = "a".repeat(40);
        let mut files = BTreeMap::new();
        files.insert(
            "pointer".into(),
            TreeEntry {
                path: "pointer".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(original),
                size: Some(3),
            },
        );
        let mut header = tar::Header::new_gnu();
        header.set_path("prefix/pointer").unwrap();
        header.set_size(32 * 1024 * 1024);
        header.set_cksum();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(header.as_bytes()).unwrap();
        gzip.write_all(&[0u8; 1024]).unwrap();
        let (base, server) = serve(vec![
            ("/tarball/", gzip.finish().unwrap()),
            ("/pointer", original.to_vec()),
        ]);
        let github = GitHub::with_endpoints("owner/repo", None, &base, &base).unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(&commit, &files, output.path(), &mut FetchBudget::new())
            .unwrap();
        assert_eq!(fs::read(output.path().join("pointer")).unwrap(), original);
        server.join().unwrap();
    }

    #[test]
    fn non_block_aligned_truncated_expansion_uses_verified_blob() {
        let original = b"ptr";
        let mut files = BTreeMap::new();
        files.insert(
            "pointer".into(),
            TreeEntry {
                path: "pointer".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(original),
                size: Some(3),
            },
        );
        let mut header = tar::Header::new_gnu();
        header.set_path("prefix/pointer").unwrap();
        header.set_size(32 * 1024 * 1024);
        header.set_cksum();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(header.as_bytes()).unwrap();
        gzip.write_all(&[0u8; 1025]).unwrap();
        let (base, server) = serve(vec![
            ("/tarball/", gzip.finish().unwrap()),
            ("/pointer", original.to_vec()),
        ]);
        let github = GitHub::with_endpoints("owner/repo", None, &base, &base).unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap();
        assert_eq!(fs::read(output.path().join("pointer")).unwrap(), original);
        server.join().unwrap();
    }

    #[test]
    fn early_expanded_blob_does_not_consume_fallback_budget_for_later_files() {
        let original = b"ptr";
        let expanded = vec![0u8; 12 * 1024 * 1024];
        let gzip = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let mut files = BTreeMap::new();
        files.insert(
            "pointer".into(),
            TreeEntry {
                path: "pointer".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(original),
                size: Some(3),
            },
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(expanded.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "prefix/pointer", expanded.as_slice())
            .unwrap();
        for index in 0..=MAX_BLOB_FALLBACKS {
            let path = format!("file-{index}");
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_cksum();
            tar.append_data(&mut header, format!("prefix/{path}"), b"x".as_slice())
                .unwrap();
            files.insert(
                path.clone(),
                TreeEntry {
                    path,
                    mode: "100644".into(),
                    kind: "blob".into(),
                    sha: git_blob_sha(b"x"),
                    size: Some(1),
                },
            );
        }
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        let (base, server) = serve(vec![("/tarball/", bytes), ("/pointer", original.to_vec())]);
        let mut github = GitHub::with_endpoints("owner/repo", None, &base, &base).unwrap();
        github
            .set_limits(SourceLimits {
                max_snapshot_bytes: 5 * 1024 * 1024,
                max_files: 1000,
            })
            .unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap();
        assert_eq!(fs::read(output.path().join("file-512")).unwrap(), b"x");
        server.join().unwrap();
    }

    fn pax_size_archive(size: u64, payload: &[u8], following: bool) -> Vec<u8> {
        let value = format!("size={size}\n");
        let mut length = value.len() + 2;
        loop {
            let record = format!("{length} {value}");
            if record.len() == length {
                break;
            }
            length = record.len();
        }
        let record = format!("{length} {value}");
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::XHeader);
        header.set_path("pax").unwrap();
        header.set_size(record.len() as u64);
        header.set_cksum();
        gzip.write_all(header.as_bytes()).unwrap();
        gzip.write_all(record.as_bytes()).unwrap();
        gzip.write_all(&vec![0; (512 - record.len() % 512) % 512])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_path("prefix/a").unwrap();
        header.set_size(1);
        header.set_cksum();
        gzip.write_all(header.as_bytes()).unwrap();
        gzip.write_all(payload).unwrap();
        if following {
            gzip.write_all(&vec![0; (512 - payload.len() % 512) % 512])
                .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("prefix/b").unwrap();
            header.set_size(1);
            header.set_cksum();
            gzip.write_all(header.as_bytes()).unwrap();
            gzip.write_all(b"b").unwrap();
            gzip.write_all(&[0; 511]).unwrap();
        }
        gzip.write_all(&[0; 1024]).unwrap();
        gzip.finish().unwrap()
    }

    #[test]
    fn pax_size_override_keeps_following_headers_aligned() {
        let mut files = BTreeMap::new();
        for (path, bytes) in [("a", b"abc".as_slice()), ("b", b"b".as_slice())] {
            files.insert(
                path.into(),
                TreeEntry {
                    path: path.into(),
                    mode: "100644".into(),
                    kind: "blob".into(),
                    sha: git_blob_sha(bytes),
                    size: Some(bytes.len() as u64),
                },
            );
        }
        let (base, server) = serve(vec![("/tarball/", pax_size_archive(3, b"abc", true))]);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap();
        assert_eq!(fs::read(output.path().join("b")).unwrap(), b"b");
        server.join().unwrap();
    }

    #[test]
    fn local_pax_size_is_consumed_by_directory_before_next_file() {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        let mut pax = tar::Header::new_gnu();
        pax.set_entry_type(tar::EntryType::XHeader);
        pax.set_path("pax").unwrap();
        pax.set_size(10);
        pax.set_cksum();
        gzip.write_all(pax.as_bytes()).unwrap();
        gzip.write_all(b"10 size=0\n").unwrap();
        gzip.write_all(&[0; 502]).unwrap();
        let mut directory = tar::Header::new_gnu();
        directory.set_entry_type(tar::EntryType::Directory);
        directory.set_path("prefix/dir/").unwrap();
        directory.set_size(0);
        directory.set_cksum();
        gzip.write_all(directory.as_bytes()).unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_path("prefix/a").unwrap();
        file.set_size(1);
        file.set_cksum();
        gzip.write_all(file.as_bytes()).unwrap();
        gzip.write_all(b"a").unwrap();
        gzip.write_all(&[0; 511]).unwrap();
        gzip.write_all(&[0; 1024]).unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            "a".into(),
            TreeEntry {
                path: "a".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(b"a"),
                size: Some(1),
            },
        );
        let (base, server) = serve(vec![("/tarball/", gzip.finish().unwrap())]);
        let github = GitHub::with_api("owner/repo", None, &base).unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap();
        assert_eq!(fs::read(output.path().join("a")).unwrap(), b"a");
        server.join().unwrap();
    }

    #[test]
    fn oversized_pax_size_uses_verified_blob_fallback() {
        let original = b"ptr";
        let mut files = BTreeMap::new();
        files.insert(
            "a".into(),
            TreeEntry {
                path: "a".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: git_blob_sha(original),
                size: Some(3),
            },
        );
        let (base, server) = serve(vec![
            ("/tarball/", pax_size_archive(32 * 1024 * 1024, b"", false)),
            ("/a", original.to_vec()),
        ]);
        let github = GitHub::with_endpoints("owner/repo", None, &base, &base).unwrap();
        let output = TempDir::new().unwrap();
        github
            .download_archive(
                &"a".repeat(40),
                &files,
                output.path(),
                &mut FetchBudget::new(),
            )
            .unwrap();
        assert_eq!(fs::read(output.path().join("a")).unwrap(), original);
        server.join().unwrap();
    }

    #[test]
    fn extraction_budget_failure_drops_partial_snapshot() {
        let commit = "a".repeat(40);
        let mut files = BTreeMap::new();
        for name in ["a", "b"] {
            files.insert(
                name.into(),
                TreeEntry {
                    path: name.into(),
                    mode: "100644".into(),
                    kind: "blob".into(),
                    sha: git_blob_sha(b"abc"),
                    size: Some(3),
                },
            );
        }
        let bytes = archive_many(&[("prefix/a", b"abc"), ("prefix/b", b"abc")]);
        let (base, server) = serve(vec![("/tarball/", bytes)]);
        let mut github = GitHub::with_api("owner/repo", None, &base).unwrap();
        github
            .set_limits(SourceLimits {
                max_snapshot_bytes: 5,
                max_files: 2,
            })
            .unwrap();
        let parent = TempDir::new().unwrap();
        let snapshot = TempDir::new_in(parent.path()).unwrap();
        let path = snapshot.path().to_owned();
        assert!(
            github
                .download_archive(&commit, &files, &path, &mut FetchBudget::new())
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert!(path.join("a").exists());
        drop(snapshot);
        assert!(!path.exists());
        server.join().unwrap();
    }

    #[test]
    fn authenticated_blob_body_without_content_length_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.write_all(&vec![b'x'; 70_000]).unwrap();
        });
        let entry = TreeEntry {
            path: "empty".into(),
            mode: "100644".into(),
            kind: "blob".into(),
            sha: git_blob_sha(b""),
            size: Some(0),
        };
        let github =
            GitHub::with_endpoints("owner/repo", Some("secret".into()), &base, &base).unwrap();
        assert!(
            github
                .fetch_raw_blob(&"a".repeat(40), "empty", &entry, &mut FetchBudget::new())
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        server.join().unwrap();
    }
}
