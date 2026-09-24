use super::*;

pub(super) fn list_conflicts(plan: &Plan) -> String {
    plan.items
        .iter()
        .filter(|item| item.action == Action::Conflict)
        .map(|item| format!("{} ({})", item.path, item.reason))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn resolution_for(
    path: &str,
    resolutions: &BTreeMap<String, ConflictResolution>,
    policy: ConflictPolicy,
) -> Result<ConflictResolution> {
    if let Some(choice) = resolutions.get(path) {
        return Ok(choice.clone());
    }
    match policy {
        ConflictPolicy::Stop => Err(UnresolvedConflicts::new(vec![path.into()]).into()),
        ConflictPolicy::UseTarget => Ok(ConflictResolution::UseTarget),
        ConflictPolicy::KeepLocal => Ok(ConflictResolution::KeepLocal),
    }
}

pub(super) fn stage_ordinary(
    plan: &Plan,
    base: &SafeRoot,
    target: &SafeRoot,
    instance: &SafeRoot,
    prepared: &SafeRoot,
    policy: ConflictPolicy,
    resolutions: &BTreeMap<String, ConflictResolution>,
) -> Result<(BTreeMap<String, bool>, Vec<String>)> {
    let mut changes = BTreeMap::<String, bool>::new();
    let mut overrides = Vec::new();
    for item in &plan.items {
        if item.action != Action::Conflict
            && let Some(choice) = resolutions.get(&item.path)
        {
            match choice {
                ConflictResolution::UseTarget => {
                    changes.insert(
                        item.path.clone(),
                        target.copy_regular_to(&item.path, prepared, &item.path)?,
                    );
                }
                ConflictResolution::KeepLocal => overrides.push(item.path.clone()),
                ConflictResolution::Manual(file) => {
                    prepared.write_atomic(
                        &item.path,
                        std::fs::File::open(file)
                            .with_context(|| format!("reading manual merge for {}", item.path))?,
                    )?;
                    changes.insert(item.path.clone(), true);
                    overrides.push(item.path.clone());
                }
            }
            continue;
        }
        match item.action {
            Action::AlreadyTarget => {}
            Action::KeepLocal => overrides.push(item.path.clone()),
            Action::Delete => {
                changes.insert(item.path.clone(), false);
            }
            Action::WriteTarget => {
                target.copy_regular_to(&item.path, prepared, &item.path)?;
                changes.insert(item.path.clone(), true);
            }
            Action::Merge => {
                let a = String::from_utf8(base.required_file(&item.path)?)?;
                let l = String::from_utf8(instance.required_file(&item.path)?)?;
                let b = String::from_utf8(target.required_file(&item.path)?)?;
                let merged = diffy::merge(&a, &l, &b).map_err(|_| {
                    anyhow::anyhow!("merge changed during preparation: {}", item.path)
                })?;
                prepared.write_atomic(&item.path, merged.as_bytes())?;
                changes.insert(item.path.clone(), true);
                overrides.push(item.path.clone());
            }
            Action::Conflict => {
                if matches!(
                    item.reason,
                    "case_only_rename" | "instance_case_collision" | "path_occupied_by_directory"
                ) {
                    bail!("resolve path obstacle before update: {}", item.path);
                }
                let choice = resolutions.get(&item.path);
                if let Some(ConflictResolution::Manual(file)) = choice {
                    prepared.write_atomic(
                        &item.path,
                        std::fs::File::open(file)
                            .with_context(|| format!("reading manual merge for {}", item.path))?,
                    )?;
                    changes.insert(item.path.clone(), true);
                    overrides.push(item.path.clone());
                    continue;
                }
                let use_target = match choice {
                    Some(ConflictResolution::UseTarget) => true,
                    Some(ConflictResolution::KeepLocal) => false,
                    None => match policy {
                        ConflictPolicy::Stop => {
                            return Err(UnresolvedConflicts::new(vec![item.path.clone()]).into());
                        }
                        ConflictPolicy::UseTarget => true,
                        ConflictPolicy::KeepLocal => false,
                    },
                    Some(ConflictResolution::Manual(_)) => unreachable!(),
                };
                if use_target {
                    if target.copy_regular_to(&item.path, prepared, &item.path)? {
                        changes.insert(item.path.clone(), true);
                    } else {
                        changes.insert(item.path.clone(), false);
                    }
                } else {
                    overrides.push(item.path.clone());
                }
            }
        }
    }
    Ok((changes, overrides))
}
