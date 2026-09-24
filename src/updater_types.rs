#[derive(Clone, Copy)]
pub enum ConflictPolicy {
    Stop,
    UseTarget,
    KeepLocal,
}

#[derive(Debug, Clone)]
pub enum ConflictResolution {
    UseTarget,
    KeepLocal,
    Manual(std::path::PathBuf),
}

#[derive(Debug)]
pub struct UnresolvedConflicts {
    pub paths: Vec<String>,
    pub target_commit: Option<String>,
}

impl UnresolvedConflicts {
    pub fn new(paths: Vec<String>) -> Self {
        Self {
            paths,
            target_commit: None,
        }
    }
}

impl std::fmt::Display for UnresolvedConflicts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "unresolved conflicts: {}", self.paths.join(", "))
    }
}

impl std::error::Error for UnresolvedConflicts {}
