use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind, Read, Write};
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

/// Create an owned temporary directory under the physical system temp directory.
/// macOS exposes its default temp parent through the `/var` system alias.
pub fn temp_dir() -> io::Result<tempfile::TempDir> {
    temp_dir_in(&std::env::temp_dir())
}

fn temp_dir_in(parent: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::TempDir::new_in(parent.canonicalize()?)
}

#[cfg(windows)]
fn open_attributes(dir: &Dir, name: &str) -> Result<cap_std::fs::File> {
    use cap_std::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES,
    };
    let mut options = OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES);
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = dir.open_with(name, &options)?;
    if !file.metadata()?.is_file() {
        bail!("path is not a regular file: {name}");
    }
    Ok(file)
}

#[cfg(windows)]
fn set_readonly(dir: &Dir, name: &str, readonly: bool) -> Result<()> {
    let file = open_attributes(dir, name)?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(readonly);
    file.set_permissions(permissions)?;
    Ok(())
}

impl SafeRoot {
    #[cfg(unix)]
    pub fn ensure_private_dir(&self, relative: &str) -> Result<()> {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let parts = validate_relative(relative)?;
        if parts.len() != 1 {
            bail!("private directory must be a single component");
        }
        let name = std::ffi::CString::new(relative)?;
        if unsafe { libc::mkdirat(self.dir.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        let directory = open_directory(&self.dir, relative)?;
        let metadata = directory.into_std_file().metadata()?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            bail!("tool lock directory must be owned by this user and private (0700)");
        }
        Ok(())
    }
    #[cfg(windows)]
    pub fn set_readonly(&self, relative: &str, readonly: bool) -> Result<()> {
        let (dir, name) = self.parent(relative, false)?;
        let dir = dir.with_context(|| format!("missing parent for {relative}"))?;
        set_readonly(&dir, &name, readonly)
    }

    pub fn open_dir(&self, relative: &str) -> Result<Self> {
        let mut dir = self.dir.try_clone()?;
        for part in validate_relative(relative)? {
            dir = open_directory(&dir, part)?;
        }
        Ok(Self { dir })
    }

    pub fn open_lock_file(&self, relative: &str) -> Result<std::fs::File> {
        let (dir, name) = self.parent(relative, true)?;
        let dir = dir.expect("creating parent always returns a directory");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let file = dir.open_with(&name, &options)?;
        if !file.metadata()?.is_file() {
            bail!("lock path is not a regular file: {relative}");
        }
        Ok(file.into_std())
    }
    /// Open a regular file without following a symlink in any path component.
    pub fn open_regular(&self, relative: &str) -> Result<Option<cap_std::fs::File>> {
        let (dir, name) = self.parent(relative, false)?;
        let Some(dir) = dir else { return Ok(None) };
        match dir.open_with(&name, &file_options()) {
            Ok(file) => {
                if !file.metadata()?.is_file() {
                    bail!("path is not a regular file: {relative}");
                }
                Ok(Some(file))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("opening {relative}")),
        }
    }

    /// Create directories through checked handles, rejecting symlink ancestors.
    pub fn ensure_dir(&self, relative: &str) -> Result<()> {
        let mut dir = self.dir.try_clone()?;
        for part in validate_relative(relative)? {
            match dir.create_dir(part) {
                Ok(()) => sync_directory(&dir)?,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).with_context(|| format!("creating {relative}")),
            }
            dir = open_directory(&dir, part)?;
        }
        Ok(())
    }

    fn parent(&self, relative: &str, create: bool) -> Result<(Option<Dir>, String)> {
        let parts = validate_relative(relative)?;
        let mut dir = self.dir.try_clone()?;
        for part in &parts[..parts.len() - 1] {
            if create {
                match dir.create_dir(part) {
                    Ok(()) => sync_directory(&dir)?,
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| format!("creating {relative}"));
                    }
                }
            }
            dir = match open_directory(&dir, part) {
                Ok(child) => child,
                Err(error)
                    if !create
                        && error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|cause| cause.kind() == ErrorKind::NotFound) =>
                {
                    return Ok((None, parts[parts.len() - 1].to_owned()));
                }
                Err(error) => return Err(error).with_context(|| format!("opening {relative}")),
            };
        }
        Ok((Some(dir), parts[parts.len() - 1].to_owned()))
    }

    pub fn remove_regular(&self, relative: &str) -> Result<()> {
        let (dir, name) = self.parent(relative, false)?;
        let Some(dir) = dir else { return Ok(()) };
        let metadata = match dir.symlink_metadata(&name) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
            Ok(_) => bail!("path is not a regular file: {relative}"),
            Err(error) => return Err(error.into()),
        };
        #[cfg(windows)]
        if metadata.permissions().readonly() {
            set_readonly(&dir, &name, false)?;
        }
        #[cfg(not(windows))]
        let _ = metadata;
        dir.remove_file(&name)
            .with_context(|| format!("removing {relative}"))?;
        sync_directory(&dir)
    }

    pub fn remove_empty_dir(&self, relative: &str) -> Result<()> {
        let (dir, name) = self.parent(relative, false)?;
        let Some(dir) = dir else { return Ok(()) };
        match dir.symlink_metadata(&name) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!("path is not a directory: {relative}"),
            Err(error) => return Err(error.into()),
        }
        dir.remove_dir(&name)?;
        sync_directory(&dir)
    }

    pub fn write_atomic(&self, relative: &str, input: impl Read) -> Result<()> {
        self.write_atomic_with_permissions(relative, input, None)
    }

    fn write_atomic_with_permissions(
        &self,
        relative: &str,
        input: impl Read,
        preferred_permissions: Option<cap_std::fs::Permissions>,
    ) -> Result<()> {
        self.write_atomic_with_permissions_checked(
            relative,
            input,
            preferred_permissions,
            || Ok(()),
        )
    }

    fn write_atomic_with_permissions_checked(
        &self,
        relative: &str,
        mut input: impl Read,
        preferred_permissions: Option<cap_std::fs::Permissions>,
        before_replace: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (dir, name) = self.parent(relative, true)?;
        let dir = dir.expect("creating parent always returns a directory");
        let permissions = match dir.symlink_metadata(&name) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                Some(metadata.permissions())
            }
            Ok(_) => bail!("path is not a regular file: {relative}"),
            Err(error) if error.kind() == ErrorKind::NotFound => preferred_permissions,
            Err(error) => return Err(error.into()),
        };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let (temp, mut file) = loop {
            let temp = format!(
                ".jsipack-tmp-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            if temp == name {
                continue;
            }
            match dir.open_with(&temp, &options) {
                Ok(file) => break (temp, file),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("creating temporary file for {relative}"));
                }
            }
        };
        let result = (|| -> Result<()> {
            io::copy(&mut input, &mut file)?;
            #[cfg(not(windows))]
            if let Some(permissions) = permissions.clone() {
                file.set_permissions(permissions)?;
            }
            file.flush()?;
            file.sync_all()?;
            drop(file);
            before_replace()?;
            #[cfg(windows)]
            let was_readonly = match dir.symlink_metadata(&name) {
                Ok(metadata) => metadata.permissions().readonly(),
                Err(error) if error.kind() == ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            #[cfg(windows)]
            if was_readonly {
                set_readonly(&dir, &name, false)?;
            }
            if let Err(error) = dir.rename(&temp, &dir, &name) {
                #[cfg(windows)]
                if was_readonly {
                    let _ = set_readonly(&dir, &name, true);
                }
                return Err(error.into());
            }
            #[cfg(windows)]
            if let Some(permissions) = permissions {
                let installed = open_attributes(&dir, &name)?;
                installed.set_permissions(permissions)?;
            }
            sync_directory(&dir)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = dir.remove_file(&temp);
        }
        result.with_context(|| format!("writing {relative}"))
    }

    pub fn copy_regular_to(
        &self,
        relative: &str,
        target: &SafeRoot,
        target_path: &str,
    ) -> Result<bool> {
        let Some(file) = self.open_regular(relative)? else {
            return Ok(false);
        };
        let permissions = file.metadata()?.permissions();
        target.write_atomic_with_permissions(target_path, file, Some(permissions))?;
        Ok(true)
    }

    pub fn copy_regular_to_checked(
        &self,
        relative: &str,
        target: &SafeRoot,
        target_path: &str,
        before_replace: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        let Some(file) = self.open_regular(relative)? else {
            return Ok(false);
        };
        let permissions = file.metadata()?.permissions();
        target.write_atomic_with_permissions_checked(
            target_path,
            file,
            Some(permissions),
            before_replace,
        )?;
        Ok(true)
    }

    #[cfg(unix)]
    pub fn set_unix_mode(&self, relative: &str, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let file = self
            .open_regular(relative)?
            .with_context(|| format!("missing file: {relative}"))?;
        file.set_permissions(cap_std::fs::Permissions::from_std(
            std::fs::Permissions::from_mode(mode),
        ))?;
        file.sync_all()?;
        Ok(())
    }

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

fn sync_directory(dir: &Dir) -> Result<()> {
    #[cfg(unix)]
    dir.try_clone()?.into_std_file().sync_all()?;
    #[cfg(windows)]
    {
        // Directory flush support varies by filesystem. The file data and
        // journal are flushed regardless; try the directory handle as well.
        let _ = dir.try_clone()?.into_std_file().sync_all();
    }
    Ok(())
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
    fn chosen_root_rejects_parent_components_before_resolution() {
        let dir = crate::safe_fs::temp_dir().unwrap();
        // Joining onto a Windows verbatim path would normalize away `..`.
        let path = PathBuf::from(format!(
            "{}{}..",
            dir.path().display(),
            std::path::MAIN_SEPARATOR
        ));
        assert!(path.components().any(|part| part == Component::ParentDir));
        assert!(SafeRoot::open(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_root_resolves_parent_alias_and_internal_symlinks_stay_blocked() {
        use std::os::unix::fs::symlink;
        let base = crate::safe_fs::temp_dir().unwrap();
        let physical = base.path().join("physical");
        let other = base.path().join("other");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("private.txt"), b"keep").unwrap();
        let alias = base.path().join("alias");
        symlink(&physical, &alias).unwrap();
        let directory = temp_dir_in(&alias).unwrap();
        assert!(
            directory
                .path()
                .starts_with(physical.canonicalize().unwrap())
        );
        let root = SafeRoot::open(directory.path()).unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        root.write_atomic("managed.txt", &b"new"[..]).unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("managed.txt")).unwrap(),
            b"new"
        );
        assert!(!other.join("managed.txt").exists());
        symlink(&other, directory.path().join("escape")).unwrap();
        assert!(root.read("escape/private.txt").is_err());
        assert!(
            root.write_atomic("escape/private.txt", &b"changed"[..])
                .is_err()
        );
        assert_eq!(std::fs::read(other.join("private.txt")).unwrap(), b"keep");
    }

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
        let dir = crate::safe_fs::temp_dir().unwrap();
        let file = std::fs::File::create(dir.path().join("large.jar")).unwrap();
        file.set_len(MAX_PREVIEW_FILE_BYTES + 1).unwrap();
        let root = SafeRoot::open(dir.path()).unwrap();
        assert!(matches!(
            root.hash_artifact("large.jar").unwrap(),
            LocalArtifact::File(_)
        ));
    }

    #[test]
    fn checked_copy_preserves_external_edit_before_replace() {
        let source = crate::safe_fs::temp_dir().unwrap();
        let target = crate::safe_fs::temp_dir().unwrap();
        std::fs::write(source.path().join("file.txt"), b"new").unwrap();
        std::fs::write(target.path().join("file.txt"), b"old").unwrap();
        let source_root = SafeRoot::open(source.path()).unwrap();
        let target_root = SafeRoot::open(target.path()).unwrap();
        let result =
            source_root.copy_regular_to_checked("file.txt", &target_root, "file.txt", || {
                std::fs::write(target.path().join("file.txt"), b"external")?;
                if target_root.required_file("file.txt")?.as_slice() != b"old" {
                    bail!("destination changed while staging replacement");
                }
                Ok(())
            });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(target.path().join("file.txt")).unwrap(),
            b"external"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_copy_preserves_existing_and_new_file_modes() {
        use std::os::unix::fs::PermissionsExt;
        let source = crate::safe_fs::temp_dir().unwrap();
        let target = crate::safe_fs::temp_dir().unwrap();
        std::fs::write(source.path().join("script.sh"), b"new").unwrap();
        std::fs::set_permissions(
            source.path().join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(target.path().join("private.sh"), b"old").unwrap();
        std::fs::set_permissions(
            target.path().join("private.sh"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let source = SafeRoot::open(source.path()).unwrap();
        let target_root = SafeRoot::open(target.path()).unwrap();
        source
            .copy_regular_to("script.sh", &target_root, "script.sh")
            .unwrap();
        source
            .copy_regular_to("script.sh", &target_root, "private.sh")
            .unwrap();
        assert_eq!(
            std::fs::metadata(target.path().join("script.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(target.path().join("private.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
