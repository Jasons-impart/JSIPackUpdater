use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

pub fn portable_case_key(name: &str) -> String {
    name.nfkc()
        .flat_map(char::to_uppercase)
        .collect::<String>()
        .nfkc()
        .collect()
}

const MAX_PREVIEW_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub struct ArtifactHashes {
    pub sha1: String,
    pub sha256: String,
    pub sha512: String,
}

pub enum LocalArtifact {
    Missing,
    File(ArtifactHashes),
    Obstacle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalFile {
    Missing,
    File(Vec<u8>),
    Obstacle,
}

pub struct SafeRoot {
    dir: Dir,
}

impl SafeRoot {
    pub fn file_paths(&self) -> Result<BTreeSet<String>> {
        let mut paths = BTreeSet::new();
        self.collect_paths(&self.dir, "", &mut paths)?;
        Ok(paths)
    }

    fn collect_paths(&self, dir: &Dir, prefix: &str, paths: &mut BTreeSet<String>) -> Result<()> {
        for entry in dir.read_dir(".")? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("non-UTF-8 snapshot path")?;
            validate_relative(name)?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.file_type()?.is_dir() {
                let child = open_directory(dir, name)?;
                self.collect_paths(&child, &path, paths)?;
            } else {
                let file = dir.open_with(name, &file_options())?;
                if !file.metadata()?.is_file() {
                    bail!("snapshot contains unsupported file: {path}");
                }
                paths.insert(path);
            }
        }
        Ok(())
    }
    pub fn hash_artifact(&self, relative: &str) -> Result<LocalArtifact> {
        let parts = validate_relative(relative)?;
        let mut dir = self.dir.try_clone()?;
        for part in &parts[..parts.len() - 1] {
            dir = match open_directory(&dir, part) {
                Ok(child) => child,
                Err(error) => return classify_artifact_error(&dir, part, relative, error, true),
            };
        }
        let name = parts[parts.len() - 1];
        let mut file = match dir.open_with(name, &file_options()) {
            Ok(file) => file,
            Err(error) => {
                return classify_artifact_error(&dir, name, relative, error.into(), false);
            }
        };
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() {
            bail!("path contains symlink: {relative}");
        }
        if !metadata.is_file() {
            return Ok(LocalArtifact::Obstacle);
        }
        if metadata.len() > MAX_ARTIFACT_BYTES {
            bail!("artifact exceeds 2 GiB: {relative}");
        }
        let mut sha1 = Sha1::new();
        let mut sha256 = Sha256::new();
        let mut sha512 = sha2::Sha512::new();
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_ARTIFACT_BYTES {
                bail!("artifact grew beyond 2 GiB: {relative}");
            }
            sha1.update(&buffer[..count]);
            sha256.update(&buffer[..count]);
            sha512.update(&buffer[..count]);
        }
        Ok(LocalArtifact::File(ArtifactHashes {
            sha1: format!("{:x}", sha1.finalize()),
            sha256: format!("{:x}", sha256.finalize()),
            sha512: format!("{:x}", sha512.finalize()),
        }))
    }
    pub fn open(path: &Path) -> Result<Self> {
        let mut start = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => start.push(prefix.as_os_str()),
                Component::RootDir => start.push(component.as_os_str()),
                _ => break,
            }
        }
        if start.as_os_str().is_empty() {
            start.push(".");
        }
        let mut dir = Dir::open_ambient_dir(&start, ambient_authority())
            .with_context(|| format!("opening root of {}", path.display()))?;
        for component in path.components() {
            match component {
                Component::ParentDir => bail!("root path cannot contain '..': {}", path.display()),
                Component::Normal(name) => {
                    dir = open_directory(&dir, name.to_str().context("non-UTF-8 root name")?)
                        .with_context(|| format!("opening root {}", path.display()))?;
                }
                _ => {}
            }
        }
        Ok(Self { dir })
    }

    pub fn read(&self, relative: &str) -> Result<LocalFile> {
        let parts = validate_relative(relative)?;
        let mut dir = self.dir.try_clone()?;
        for part in &parts[..parts.len() - 1] {
            dir = match open_directory(&dir, part) {
                Ok(child) => child,
                Err(error) => return classify_open_error(&dir, part, relative, error, true),
            };
        }
        let name = parts[parts.len() - 1];
        let mut file = match dir.open_with(name, &file_options()) {
            Ok(file) => file,
            Err(error) => return classify_open_error(&dir, name, relative, error.into(), false),
        };
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() {
            bail!("path contains symlink: {relative}");
        }
        if !metadata.is_file() {
            return Ok(LocalFile::Obstacle);
        }
        if metadata.len() > MAX_PREVIEW_FILE_BYTES {
            bail!("file exceeds 64 MiB preview limit: {relative}");
        }
        let bytes = read_bounded(&mut file, metadata.len(), relative)?;
        Ok(LocalFile::File(bytes))
    }

    pub fn required_file(&self, relative: &str) -> Result<Vec<u8>> {
        match self.read(relative)? {
            LocalFile::File(bytes) => Ok(bytes),
            LocalFile::Missing => bail!("required file is missing: {relative}"),
            LocalFile::Obstacle => bail!("required file is not regular: {relative}"),
        }
    }

    pub fn hashes(&self) -> Result<BTreeMap<String, [u8; 32]>> {
        let mut hashes = BTreeMap::new();
        self.collect_hashes(&self.dir, "", &mut hashes)?;
        Ok(hashes)
    }

    fn collect_hashes(
        &self,
        dir: &Dir,
        prefix: &str,
        hashes: &mut BTreeMap<String, [u8; 32]>,
    ) -> Result<()> {
        for entry in dir.read_dir(".")? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("non-UTF-8 snapshot path")?;
            validate_relative(name)?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.file_type()?.is_dir() {
                let child = open_directory(dir, name)
                    .with_context(|| format!("opening snapshot directory {path}"))?;
                self.collect_hashes(&child, &path, hashes)?;
            } else {
                let mut file = dir
                    .open_with(name, &file_options())
                    .with_context(|| format!("opening snapshot file {path}"))?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!("snapshot contains unsupported file: {path}");
                }
                hashes.insert(
                    path.clone(),
                    hash_bounded(&mut file, metadata.len(), &path)?,
                );
            }
        }
        Ok(())
    }

    pub fn case_collisions(&self, paths: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let mut cache = BTreeMap::<String, BTreeMap<String, Vec<String>>>::new();
        let mut collisions = BTreeSet::new();
        for path in paths {
            let mut parent = String::new();
            for part in validate_relative(path)? {
                if !cache.contains_key(&parent) {
                    cache.insert(parent.clone(), self.directory_names(&parent)?);
                }
                let names = &cache[&parent];
                let Some(matches) = names.get(&portable_case_key(part)) else {
                    break;
                };
                if matches.iter().any(|name| name != part) {
                    collisions.insert(path.clone());
                    break;
                }
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(part);
            }
        }
        Ok(collisions)
    }

    fn directory_names(&self, relative: &str) -> Result<BTreeMap<String, Vec<String>>> {
        let mut dir = self.dir.try_clone()?;
        if !relative.is_empty() {
            for part in validate_relative(relative)? {
                dir = match open_directory(&dir, part) {
                    Ok(child) => child,
                    Err(_) => return Ok(BTreeMap::new()),
                };
            }
        }
        let mut names = BTreeMap::<String, Vec<String>>::new();
        for entry in dir.read_dir(".")? {
            let name = entry?.file_name();
            if let Some(name) = name.to_str() {
                names
                    .entry(portable_case_key(name))
                    .or_default()
                    .push(name.to_owned());
            }
        }
        Ok(names)
    }
}

fn classify_artifact_error(
    dir: &Dir,
    part: &str,
    relative: &str,
    error: anyhow::Error,
    parent: bool,
) -> Result<LocalArtifact> {
    match dir.symlink_metadata(part) {
        Err(probe) if probe.kind() == ErrorKind::NotFound => Ok(LocalArtifact::Missing),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("path contains symlink: {relative}")
        }
        Ok(metadata) if (parent && !metadata.is_dir()) || (!parent && !metadata.is_file()) => {
            Ok(LocalArtifact::Obstacle)
        }
        _ => Err(error).with_context(|| format!("opening {relative}")),
    }
}

fn read_bounded(reader: &mut impl Read, declared_size: u64, relative: &str) -> Result<Vec<u8>> {
    if declared_size > MAX_PREVIEW_FILE_BYTES {
        bail!("file exceeds 64 MiB preview limit: {relative}");
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(declared_size as usize)?;
    reader
        .take(MAX_PREVIEW_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {relative}"))?;
    if bytes.len() as u64 > MAX_PREVIEW_FILE_BYTES {
        bail!("file grew beyond 64 MiB preview limit: {relative}");
    }
    Ok(bytes)
}

fn hash_bounded(reader: &mut impl Read, declared_size: u64, relative: &str) -> Result<[u8; 32]> {
    if declared_size > MAX_PREVIEW_FILE_BYTES {
        bail!("snapshot file exceeds 64 MiB preview limit: {relative}");
    }
    let mut reader = reader.take(MAX_PREVIEW_FILE_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_PREVIEW_FILE_BYTES {
            bail!("snapshot file grew beyond 64 MiB preview limit: {relative}");
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn validate_relative(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<_> = path.split('/').collect();
    if parts.iter().any(|part| {
        part.is_empty()
            || part.contains('\\')
            || !matches!(
                Path::new(part).components().next(),
                Some(Component::Normal(_))
            )
            || Path::new(part).components().count() != 1
    }) {
        bail!("unsafe relative path: {path}");
    }
    Ok(parts)
}

fn classify_open_error(
    dir: &Dir,
    part: &str,
    relative: &str,
    error: anyhow::Error,
    parent: bool,
) -> Result<LocalFile> {
    match dir.symlink_metadata(part) {
        Err(probe) if probe.kind() == ErrorKind::NotFound => Ok(LocalFile::Missing),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("path contains symlink: {relative}")
        }
        Ok(metadata) if (parent && !metadata.is_dir()) || (!parent && !metadata.is_file()) => {
            Ok(LocalFile::Obstacle)
        }
        _ => Err(error).with_context(|| format!("opening {relative}")),
    }
}

fn open_directory(dir: &Dir, name: &str) -> Result<Dir> {
    let file = dir.open_with(name, &directory_options())?;
    let metadata = file.metadata()?;
    if metadata.file_type().is_symlink() {
        bail!("path contains symlink: {name}");
    }
    if !metadata.is_dir() {
        bail!("path is not a directory: {name}");
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn directory_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(windows_directory_flags());
    }
    options
}

fn file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
}

#[cfg(windows)]
fn windows_directory_flags() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_rejects_understated_size() {
        let mut contents = std::io::repeat(0).take(MAX_PREVIEW_FILE_BYTES + 1);
        assert!(
            read_bounded(&mut contents, 0, "growing.bin")
                .unwrap_err()
                .to_string()
                .contains("grew beyond")
        );
    }

    #[test]
    fn bounded_hasher_rejects_understated_size() {
        let mut contents = std::io::repeat(0).take(MAX_PREVIEW_FILE_BYTES + 1);
        assert!(
            hash_bounded(&mut contents, 0, "growing.bin")
                .unwrap_err()
                .to_string()
                .contains("grew beyond")
        );
    }

    #[test]
    fn artifact_hashing_streams_past_preview_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = std::fs::File::create(dir.path().join("large.jar")).unwrap();
        file.set_len(MAX_PREVIEW_FILE_BYTES + 1).unwrap();
        let root = SafeRoot::open(dir.path()).unwrap();
        assert!(matches!(
            root.hash_artifact("large.jar").unwrap(),
            LocalArtifact::File(_)
        ));
    }
}
