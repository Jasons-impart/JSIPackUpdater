use crate::bkmpw;
use crate::managed::{
    Manifest, Side, adopt_installed, apply_optional_choices, optional_choices,
    optional_conflict_choices, optional_selections, validate_manifest,
};
use crate::planner::{Action, Plan, plan_roots_checked};
use crate::safe_fs::{LocalFile, SafeRoot};
use crate::transaction::{self, Change};
use crate::version::{ReleaseInfo, check_components_roots};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tempfile::TempDir;

const MARKER: &str = "config/createdelight_release_info.json";

#[derive(Serialize, Deserialize)]
struct GeneratedState {
    commit_id: String,
    files: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct OverrideState {
    commit_id: String,
    paths: Vec<String>,
    #[serde(default)]
    missing_artifacts: BTreeMap<String, String>,
    #[serde(default)]
    optional_selections: BTreeMap<String, bool>,
}

#[path = "updater_types.rs"]
mod types;
pub use types::{ConflictPolicy, ConflictResolution, UnresolvedConflicts};

pub struct UpdateRequest<'a> {
    pub base_path: &'a Path,
    pub target_path: &'a Path,
    pub instance_path: &'a Path,
    pub base_commit: &'a str,
    pub target_commit: &'a str,
    pub side: Side,
    pub bkmpw_executable: &'a Path,
    pub policy: ConflictPolicy,
    pub resolutions: BTreeMap<String, ConflictResolution>,
}

#[derive(Debug, Serialize)]
pub struct UpdateReport {
    pub target_commit: String,
    pub changed_files: usize,
    pub managed_files: usize,
    pub local_overrides: Vec<String>,
}

pub fn update(request: UpdateRequest<'_>) -> Result<UpdateReport> {
    update_with_progress(request, |_| {})
}

pub fn update_with_progress(
    request: UpdateRequest<'_>,
    mut progress: impl FnMut(String),
) -> Result<UpdateReport> {
    let UpdateRequest {
        base_path,
        target_path,
        instance_path,
        base_commit,
        target_commit,
        side,
        bkmpw_executable,
        policy,
        resolutions,
    } = request;
    progress("Validating instance and base commit".into());
    let guard = transaction::lock_instance(instance_path)?;
    if transaction::pending(instance_path)? {
        bail!("unfinished update exists; run rollback before another update");
    }
    if target_commit.len() != 40 || !target_commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("target must resolve to a full commit SHA");
    }
    let base = SafeRoot::open(base_path)?;
    let target = SafeRoot::open(target_path)?;
    let instance = SafeRoot::open(instance_path)?;
    if let LocalFile::File(bytes) = instance.read(".jsipack-updater/side.json")? {
        let saved: Side = serde_json::from_slice(&bytes).context("reading saved instance side")?;
        if saved != side {
            bail!("instance side differs from saved selection");
        }
    }
    let old_generated = match instance.read(".jsipack-updater/generated.json")? {
        LocalFile::File(bytes) => {
            let state: GeneratedState = serde_json::from_slice(&bytes)?;
            Some(state)
        }
        LocalFile::Missing => None,
        LocalFile::Obstacle => bail!("generated state is not a regular file"),
    };
    let old_overrides = match instance.read(".jsipack-updater/overrides.json")? {
        LocalFile::File(bytes) => Some(serde_json::from_slice::<OverrideState>(&bytes)?),
        LocalFile::Missing => None,
        LocalFile::Obstacle => bail!("override state is not a regular file"),
    };
    let check = check_components_roots(&base, &target, &instance)?;
    if !check.info.commit_id.eq_ignore_ascii_case(base_commit) {
        bail!("installed commit changed while preparing update");
    }
    if old_generated
        .as_ref()
        .is_some_and(|state| !state.commit_id.eq_ignore_ascii_case(base_commit))
    {
        bail!("generated file state belongs to another installed commit");
    }
    if old_overrides
        .as_ref()
        .is_some_and(|state| !state.commit_id.eq_ignore_ascii_case(base_commit))
    {
        bail!("override state belongs to another installed commit");
    }
    if let Some(state) = &old_overrides {
        crate::planner::validate_source_paths(state.paths.iter().map(String::as_str))?;
        crate::planner::validate_source_paths(state.missing_artifacts.keys().map(String::as_str))?;
        crate::planner::validate_source_paths(
            state.optional_selections.keys().map(String::as_str),
        )?;
    }
    let plan = plan_roots_checked(&base, &target, &instance, &check)?;
    crate::planner::validate_source_paths(resolutions.keys().map(String::as_str))?;
    for path in resolutions.keys() {
        if !matches!(path.as_str(), "pack.toml" | "index.toml")
            && !["mods/", "resourcepacks/", "shaderpacks/"]
                .iter()
                .any(|prefix| path.starts_with(prefix))
            && !old_overrides
                .as_ref()
                .is_some_and(|state| state.paths.contains(path))
            && !plan
                .items
                .iter()
                .any(|item| item.path == *path && item.action == Action::Conflict)
        {
            bail!("resolution does not match a file conflict: {path}");
        }
    }
    let unresolved: Vec<_> = plan
        .items
        .iter()
        .filter(|item| item.action == Action::Conflict && !resolutions.contains_key(&item.path))
        .map(|item| item.path.clone())
        .collect();
    if !unresolved.is_empty() && matches!(policy, ConflictPolicy::Stop) {
        return Err(
            anyhow::Error::new(UnresolvedConflicts::new(unresolved)).context(format!(
                "update has unresolved file conflicts: {}",
                list_conflicts(&plan)
            )),
        );
    }
    let (old_manifest, old_conflicts) = match instance.read("packwiz.json")? {
        LocalFile::File(bytes) => {
            let manifest: Manifest =
                serde_json::from_slice(&bytes).context("parsing packwiz.json")?;
            let conflicts = validate_manifest(&manifest, &instance, side)?;
            (manifest, conflicts)
        }
        LocalFile::Missing => {
            let adoption = adopt_installed(&base, &instance, side)?;
            (adoption.manifest, adoption.conflicts)
        }
        LocalFile::Obstacle => bail!("packwiz.json is not a regular file"),
    };
    for conflict in &old_conflicts {
        if matches!(conflict.reason, "instance_case_collision" | "path_occupied") {
            bail!(
                "resolve managed path obstacle before update: {}",
                conflict.path
            );
        }
    }
    let mut initial_paths: BTreeSet<String> = base
        .file_paths()?
        .into_iter()
        .chain(target.file_paths()?)
        .collect();
    if let Some(state) = &old_overrides {
        initial_paths.extend(state.paths.iter().cloned());
        initial_paths.extend(state.missing_artifacts.keys().cloned());
    }
    initial_paths.extend(
        target
            .file_paths()?
            .into_iter()
            .filter(|path| path.ends_with(".pw.toml")),
    );
    initial_paths.extend(old_manifest.files.iter().map(|file| file.path.clone()));
    initial_paths.extend(old_conflicts.iter().map(|conflict| conflict.path.clone()));
    initial_paths.extend(
        [
            MARKER,
            "packwiz.json",
            "pack.toml",
            "index.toml",
            ".jsipack-updater/side.json",
            ".jsipack-updater/overrides.json",
            ".jsipack-updater/generated.json",
        ]
        .map(str::to_owned),
    );
    let mut initial_hashes = BTreeMap::new();
    for path in initial_paths {
        initial_hashes.insert(path.clone(), transaction::hash(&instance, &path)?);
    }
    bkmpw::verify_protocol(bkmpw_executable)?;
    let work = TempDir::new().context("creating update preparation directory")?;
    let source_path = work.path().join("source");
    let artifacts_path = work.path().join("artifacts");
    let prepared_path = work.path().join("prepared");
    for path in [&source_path, &artifacts_path, &prepared_path] {
        std::fs::create_dir(path)?;
    }
    let source = SafeRoot::open(&source_path)?;
    let artifacts = SafeRoot::open(&artifacts_path)?;
    let prepared = SafeRoot::open(&prepared_path)?;
    let mut effective_resolutions = resolutions.clone();
    if let Some(state) = &old_overrides {
        for path in &state.paths {
            if effective_resolutions.contains_key(path)
                || instance.open_regular(path)?.is_some()
                || target.open_regular(path)?.is_none()
            {
                continue;
            }
            let unchanged_target =
                transaction::hash(&base, path)? == transaction::hash(&target, path)?;
            if unchanged_target || matches!(policy, ConflictPolicy::KeepLocal) {
                effective_resolutions.insert(path.clone(), ConflictResolution::KeepLocal);
            } else if matches!(policy, ConflictPolicy::Stop) {
                return Err(UnresolvedConflicts::new(vec![path.clone()]).into());
            }
        }
    }
    let (mut changes, mut overrides) = stage_ordinary(
        &plan,
        &base,
        &target,
        &instance,
        &prepared,
        policy,
        &effective_resolutions,
    )?;
    if let Some(state) = &old_overrides {
        for path in &state.paths {
            if base.open_regular(path)?.is_none()
                && target.open_regular(path)?.is_none()
                && (instance.open_regular(path)?.is_some()
                    || matches!(resolutions.get(path), Some(ConflictResolution::Manual(_))))
            {
                match resolutions.get(path) {
                    Some(ConflictResolution::UseTarget) => {
                        changes.insert(path.clone(), false);
                    }
                    Some(ConflictResolution::Manual(manual)) => {
                        prepared.write_atomic(
                            path,
                            std::fs::File::open(manual)
                                .with_context(|| format!("reading manual merge for {path}"))?,
                        )?;
                        changes.insert(path.clone(), true);
                        if !overrides.contains(path) {
                            overrides.push(path.clone());
                        }
                    }
                    _ => {
                        if !overrides.contains(path) {
                            overrides.push(path.clone());
                        }
                    }
                }
            }
        }
    }
    for path in target.file_paths()? {
        target.copy_regular_to(&path, &source, &path)?;
        if changes.get(&path) == Some(&true) {
            prepared.copy_regular_to(&path, &source, &path)?;
        } else if overrides.contains(&path) && !instance.copy_regular_to(&path, &source, &path)? {
            source.remove_regular(&path)?;
        }
    }
    for item in &plan.items {
        if target.open_regular(&item.path)?.is_none()
            && overrides.contains(&item.path)
            && (changes.get(&item.path) == Some(&true)
                || instance.open_regular(&item.path)?.is_some())
        {
            if changes.get(&item.path) == Some(&true) {
                prepared.copy_regular_to(&item.path, &source, &item.path)?;
            } else {
                instance.copy_regular_to(&item.path, &source, &item.path)?;
            }
        }
    }
    if let Some(state) = &old_overrides {
        for path in &state.paths {
            if path.ends_with(".pw.toml")
                && target.open_regular(path)?.is_none()
                && (changes.get(path) == Some(&true) || instance.open_regular(path)?.is_some())
            {
                match changes.get(path) {
                    Some(false) => {}
                    Some(true) => {
                        prepared.copy_regular_to(path, &source, path)?;
                    }
                    None => {
                        instance.copy_regular_to(path, &source, path)?;
                    }
                }
            }
        }
    }
    let mut choices = optional_choices(&base, side, &old_manifest)?;
    choices.extend(optional_conflict_choices(&base, side, &old_conflicts)?);
    if let Some(state) = &old_overrides {
        choices.extend(
            state
                .optional_selections
                .iter()
                .map(|(path, choice)| (path.clone(), *choice)),
        );
        let carried = optional_choices(&source, side, &old_manifest)?;
        for path in &state.paths {
            if !choices.contains_key(path)
                && let Some(chosen) = carried.get(path)
            {
                choices.insert(path.clone(), *chosen);
            }
        }
    }
    apply_optional_choices(&source, &choices, side)?;
    let selected_options = optional_selections(&source, side)?;
    for path in source
        .file_paths()?
        .iter()
        .filter(|path| path.ends_with(".pw.toml"))
    {
        let source_hash = transaction::hash(&source, path)?;
        let effective_hash = match changes.get(path) {
            Some(true) => transaction::hash(&prepared, path)?,
            Some(false) => None,
            None => transaction::hash(&instance, path)?,
        };
        if source_hash != effective_hash {
            source.copy_regular_to(path, &prepared, path)?;
            changes.insert(path.clone(), true);
        }
        if source_hash != transaction::hash(&target, path)? {
            if !overrides.contains(path) {
                overrides.push(path.clone());
            }
        } else {
            overrides.retain(|item| item != path);
        }
    }
    check_components_roots(&base, &source, &instance)
        .context("effective pack changes Minecraft or NeoForge components")?;
    progress("Preparing target pack".into());
    bkmpw::prepare_pack(bkmpw_executable, &source_path, |event| {
        if let Some(message) = &event.message {
            progress(message.clone());
        }
    })?;
    progress("Downloading managed artifacts".into());
    bkmpw::sync(
        bkmpw_executable,
        &source_path,
        &artifacts_path,
        side,
        |event| {
            if let Some(message) = &event.message {
                progress(message.clone());
            }
        },
    )?;
    let mut new_manifest: Manifest =
        serde_json::from_slice(&artifacts.required_file("packwiz.json")?)?;
    if !validate_manifest(&new_manifest, &artifacts, side)?.is_empty() {
        bail!("bkmpw output manifest failed verification");
    }
    let mut applicable_resolutions: BTreeSet<String> =
        plan.items.iter().map(|item| item.path.clone()).collect();
    applicable_resolutions.extend(["pack.toml".to_owned(), "index.toml".to_owned()]);
    applicable_resolutions.extend(old_manifest.files.iter().map(|file| file.path.clone()));
    applicable_resolutions.extend(old_conflicts.iter().map(|conflict| conflict.path.clone()));
    applicable_resolutions.extend(new_manifest.files.iter().map(|file| file.path.clone()));
    if let Some(state) = &old_overrides {
        applicable_resolutions.extend(state.paths.iter().cloned());
    }
    for path in resolutions.keys() {
        if !applicable_resolutions.contains(path) {
            bail!("resolution does not match an applicable path: {path}");
        }
    }
    let old_managed: BTreeSet<_> = old_manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let new_managed: BTreeSet<_> = new_manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let new_paths = new_managed.clone();
    let case_collisions = instance.case_collisions(&new_paths)?;
    if !case_collisions.is_empty() {
        bail!(
            "target artifacts collide with instance paths: {}",
            case_collisions.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    let ordinary: BTreeSet<_> = base
        .file_paths()?
        .into_iter()
        .chain(target.file_paths()?)
        .collect();
    if old_managed
        .iter()
        .chain(new_managed.iter())
        .any(|path| ordinary.contains(path.as_str()))
    {
        bail!("managed artifact overlaps a tracked source file");
    }
    let current_plan = plan_roots_checked(&base, &target, &instance, &check)?;
    if serde_json::to_vec(&current_plan)? != serde_json::to_vec(&plan)? {
        bail!("instance changed while preparing update; preview again");
    }
    let mut verify_only = BTreeMap::new();
    let mut unmanaged_targets = BTreeSet::new();
    let mut missing_artifacts = BTreeMap::new();
    for file in &new_manifest.files {
        let local_hash = transaction::hash(&instance, &file.path)?;
        initial_hashes
            .entry(file.path.clone())
            .or_insert(local_hash.clone());
        let old_hash = old_manifest
            .files
            .iter()
            .find(|old| old.path == file.path)
            .map(|old| old.sha256.as_str());
        let previous_absence = old_overrides
            .as_ref()
            .and_then(|state| state.missing_artifacts.get(&file.path));
        if let Some(choice) = resolutions.get(&file.path) {
            match choice {
                ConflictResolution::UseTarget => {}
                ConflictResolution::KeepLocal => {
                    unmanaged_targets.insert(file.path.clone());
                    if local_hash.is_none() {
                        missing_artifacts.insert(file.path.clone(), file.sha256.clone());
                    }
                    verify_only.insert(file.path.clone(), local_hash.clone());
                    continue;
                }
                ConflictResolution::Manual(manual) => {
                    prepared.write_atomic(
                        &file.path,
                        std::fs::File::open(manual).with_context(|| {
                            format!("reading manual artifact for {}", file.path)
                        })?,
                    )?;
                    changes.insert(file.path.clone(), true);
                    unmanaged_targets.insert(file.path.clone());
                    continue;
                }
            }
        }
        if !resolutions.contains_key(&file.path)
            && local_hash.is_none()
            && previous_absence.is_some_and(|hash| hash.eq_ignore_ascii_case(&file.sha256))
        {
            unmanaged_targets.insert(file.path.clone());
            missing_artifacts.insert(file.path.clone(), file.sha256.clone());
            verify_only.insert(file.path.clone(), None);
            continue;
        }
        let conflict = (old_hash.is_some() && local_hash.is_none()
            || previous_absence.is_some() && local_hash.is_none()
            || local_hash.as_deref().is_some_and(|local| {
                old_hash
                    .map(|old| !old.eq_ignore_ascii_case(local))
                    .unwrap_or(true)
            }))
            && !local_hash
                .as_deref()
                .is_some_and(|local| local.eq_ignore_ascii_case(&file.sha256));
        if conflict {
            match resolution_for(&file.path, &resolutions, policy)? {
                ConflictResolution::UseTarget => {}
                ConflictResolution::KeepLocal => {
                    unmanaged_targets.insert(file.path.clone());
                    if local_hash.is_none() {
                        missing_artifacts.insert(file.path.clone(), file.sha256.clone());
                    }
                    verify_only.insert(file.path.clone(), local_hash);
                    continue;
                }
                ConflictResolution::Manual(manual) => {
                    prepared.write_atomic(
                        &file.path,
                        std::fs::File::open(&manual).with_context(|| {
                            format!("reading manual artifact for {}", file.path)
                        })?,
                    )?;
                    changes.insert(file.path.clone(), true);
                    unmanaged_targets.insert(file.path.clone());
                    continue;
                }
            }
        }
        if local_hash
            .as_deref()
            .is_some_and(|hash| hash.eq_ignore_ascii_case(&file.sha256))
        {
            verify_only.insert(file.path.clone(), Some(file.sha256.to_ascii_lowercase()));
            continue;
        }
        artifacts.copy_regular_to(&file.path, &prepared, &file.path)?;
        changes.insert(file.path.clone(), true);
    }
    new_manifest
        .files
        .retain(|file| !unmanaged_targets.contains(&file.path));
    for path in old_managed.difference(&new_managed) {
        let old = old_manifest
            .files
            .iter()
            .find(|file| file.path == *path)
            .expect("old managed path");
        let local = initial_hashes.get(path).and_then(Option::as_deref);
        if local.is_none() && !resolutions.contains_key(path) {
            continue;
        }
        let choice = if let Some(choice) = resolutions.get(path) {
            Some(choice.clone())
        } else if !local.is_some_and(|hash| hash.eq_ignore_ascii_case(&old.sha256)) {
            Some(resolution_for(path, &resolutions, policy)?)
        } else {
            None
        };
        if let Some(choice) = choice {
            match choice {
                ConflictResolution::UseTarget => {}
                ConflictResolution::KeepLocal => continue,
                ConflictResolution::Manual(manual) => {
                    prepared.write_atomic(
                        path,
                        std::fs::File::open(&manual)
                            .with_context(|| format!("reading manual artifact for {path}"))?,
                    )?;
                    changes.insert(path.clone(), true);
                    continue;
                }
            }
        }
        changes.insert(path.clone(), false);
    }
    for conflict in &old_conflicts {
        if new_managed.contains(&conflict.path) {
            continue;
        }
        if initial_hashes
            .get(&conflict.path)
            .is_some_and(Option::is_none)
            && !resolutions.contains_key(&conflict.path)
        {
            continue;
        }
        match resolution_for(&conflict.path, &resolutions, policy)? {
            ConflictResolution::UseTarget => {
                if initial_hashes
                    .get(&conflict.path)
                    .is_some_and(Option::is_some)
                {
                    changes.insert(conflict.path.clone(), false);
                }
            }
            ConflictResolution::KeepLocal => {}
            ConflictResolution::Manual(manual) => {
                prepared.write_atomic(
                    &conflict.path,
                    std::fs::File::open(&manual).with_context(|| {
                        format!("reading manual artifact for {}", conflict.path)
                    })?,
                )?;
                changes.insert(conflict.path.clone(), true);
            }
        }
    }
    let mut reconstructed_base = BTreeMap::new();
    if old_generated.is_none()
        && ["pack.toml", "index.toml"]
            .iter()
            .any(|path| initial_hashes.get(*path).is_some_and(Option::is_some))
    {
        let base_generated_path = work.path().join("base-generated");
        std::fs::create_dir(&base_generated_path)?;
        let base_generated = SafeRoot::open(&base_generated_path)?;
        for path in base.file_paths()? {
            base.copy_regular_to(&path, &base_generated, &path)?;
        }
        bkmpw::prepare_pack(bkmpw_executable, &base_generated_path, |event| {
            if let Some(message) = &event.message {
                progress(message.clone());
            }
        })?;
        for path in ["pack.toml", "index.toml"] {
            if let Some(hash) = transaction::hash(&base_generated, path)? {
                reconstructed_base.insert(path.to_owned(), hash);
            }
        }
    }
    let mut generated_hashes = BTreeMap::new();
    for path in ["pack.toml", "index.toml"] {
        changes.remove(path);
        overrides.retain(|item| item != path);
        let generated = transaction::hash(&source, path)?;
        let Some(generated) = generated else {
            bail!("bkmpw did not generate {path}");
        };
        generated_hashes.insert(path.into(), generated.clone());
        let local = initial_hashes.get(path).cloned().flatten();
        let explicit_target = matches!(resolutions.get(path), Some(ConflictResolution::UseTarget));
        match resolutions.get(path) {
            Some(ConflictResolution::Manual(file)) => {
                prepared.write_atomic(
                    path,
                    std::fs::File::open(file)
                        .with_context(|| format!("reading manual merge for {path}"))?,
                )?;
                changes.insert(path.into(), true);
                overrides.push(path.into());
                continue;
            }
            Some(ConflictResolution::KeepLocal) => {
                overrides.push(path.into());
                verify_only.insert(path.into(), local.clone());
                continue;
            }
            _ => {}
        }
        if local.is_none() && old_generated.is_some() && !explicit_target {
            let unchanged_target = old_generated
                .as_ref()
                .and_then(|state| state.files.get(path))
                == Some(&generated);
            if unchanged_target || matches!(policy, ConflictPolicy::KeepLocal) {
                overrides.push(path.into());
                verify_only.insert(path.into(), None);
                continue;
            }
            if matches!(policy, ConflictPolicy::Stop) {
                return Err(UnresolvedConflicts::new(vec![path.into()]).into());
            }
        }
        if local.as_deref() == Some(generated.as_str()) {
            verify_only.insert(path.into(), Some(generated));
            continue;
        }
        let baseline = old_generated
            .as_ref()
            .and_then(|state| state.files.get(path))
            .or_else(|| reconstructed_base.get(path));
        if !explicit_target
            && let Some(local_hash) = &local
            && baseline.map(String::as_str) != Some(local_hash.as_str())
        {
            match policy {
                ConflictPolicy::Stop => {
                    return Err(UnresolvedConflicts::new(vec![path.into()]).into());
                }
                ConflictPolicy::KeepLocal => {
                    overrides.push(path.into());
                    verify_only.insert(path.into(), Some(local_hash.clone()));
                    continue;
                }
                ConflictPolicy::UseTarget => {}
            }
        }
        source.copy_regular_to(path, &prepared, path)?;
        changes.insert(path.into(), true);
    }
    prepared.write_atomic(
        "packwiz.json",
        serde_json::to_vec_pretty(&new_manifest)?.as_slice(),
    )?;
    changes.insert("packwiz.json".into(), true);
    prepared.write_atomic(
        ".jsipack-updater/side.json",
        serde_json::to_vec(&side)?.as_slice(),
    )?;
    changes.insert(".jsipack-updater/side.json".into(), true);
    let overrides_state = OverrideState {
        commit_id: target_commit.into(),
        paths: overrides.clone(),
        missing_artifacts,
        optional_selections: selected_options,
    };
    prepared.write_atomic(
        ".jsipack-updater/overrides.json",
        serde_json::to_vec_pretty(&overrides_state)?.as_slice(),
    )?;
    changes.insert(".jsipack-updater/overrides.json".into(), true);
    let generated_state = GeneratedState {
        commit_id: target_commit.into(),
        files: generated_hashes,
    };
    prepared.write_atomic(
        ".jsipack-updater/generated.json",
        serde_json::to_vec_pretty(&generated_state)?.as_slice(),
    )?;
    changes.insert(".jsipack-updater/generated.json".into(), true);
    let version: toml::Value = toml::from_str(std::str::from_utf8(
        &target.required_file("pack/pack.toml")?,
    )?)?;
    let target_version = version
        .get("version")
        .and_then(toml::Value::as_str)
        .context("target pack has no version")?;
    let marker = ReleaseInfo {
        schema_version: 1,
        version: target_version.into(),
        commit_id: target_commit.into(),
        minecraft_version: check.info.minecraft_version,
        neoforge_version: check.info.neoforge_version,
    };
    prepared.write_atomic(MARKER, serde_json::to_vec_pretty(&marker)?.as_slice())?;
    changes.remove(MARKER);
    let changed_files = changes.len() + 1;
    let mut transaction_changes = Vec::with_capacity(changed_files);
    for (path, replacement) in changes {
        transaction_changes.push(Change {
            expected: initial_hashes.get(&path).cloned().unwrap_or(None),
            path,
            replacement,
        });
    }
    transaction_changes.push(Change {
        path: MARKER.into(),
        expected: initial_hashes.get(MARKER).cloned().unwrap_or(None),
        replacement: true,
    });
    let changed_paths: BTreeSet<_> = transaction_changes
        .iter()
        .map(|change| change.path.as_str())
        .collect();
    for (path, expected) in &initial_hashes {
        if !path.starts_with(".jsipack-updater/") && !changed_paths.contains(path.as_str()) {
            verify_only
                .entry(path.clone())
                .or_insert_with(|| expected.clone());
        }
    }
    progress("Applying verified transaction".into());
    transaction::apply_locked(
        &guard,
        instance_path,
        &prepared,
        &transaction_changes,
        &verify_only,
    )?;
    progress("Update complete".into());
    Ok(UpdateReport {
        target_commit: target_commit.into(),
        changed_files,
        managed_files: new_manifest.files.len(),
        local_overrides: overrides,
    })
}

#[path = "updater_ordinary.rs"]
mod ordinary;
use ordinary::{list_conflicts, resolution_for, stage_ordinary};

#[cfg(all(test, unix))]
#[path = "updater_tests.rs"]
mod tests;
