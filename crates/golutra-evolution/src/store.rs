use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::Utc;
use fs2::FileExt;
use golutra_eval::SkillCandidate;
use sha2::{Digest, Sha256};

use crate::{
    EvolutionError, EvolutionPlan, EvolutionState, GeneratedTaskExecution, OpenEndedRun,
    OpenEndedRunStatus, SkillLifecycleRecord, SkillLifecycleStatus, SkillManifest,
};

const MAX_EVOLUTION_STATE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EvolutionStore {
    state_path: PathBuf,
    skills_root: PathBuf,
}

impl EvolutionStore {
    #[must_use]
    pub fn new(state_path: impl Into<PathBuf>, skills_root: impl Into<PathBuf>) -> Self {
        Self {
            state_path: state_path.into(),
            skills_root: skills_root.into(),
        }
    }

    pub fn snapshot(&self) -> Result<EvolutionState, EvolutionError> {
        self.with_state(|state| Ok(state.clone()), false)
    }

    pub fn record_plan(&self, plan: EvolutionPlan) -> Result<EvolutionState, EvolutionError> {
        self.with_state(
            |state| {
                replace_by(&mut state.runs, plan.run, |value| value.run_id.clone());
                for task in plan.generated_tasks {
                    replace_by(&mut state.generated_tasks, task, |value| value.id.clone());
                }
                for item in plan.curriculum {
                    replace_by(&mut state.curriculum, item, |value| value.task_id.clone());
                }
                for record in plan.novelty {
                    replace_by(&mut state.novelty, record, |value| value.task_id.clone());
                }
                for recipe in plan.recipes {
                    replace_by(&mut state.recipes, recipe, |value| value.recipe_id.clone());
                }
                state.frontier = Some(plan.frontier);
                Ok(state.clone())
            },
            true,
        )
    }

    pub fn record_execution(
        &self,
        execution: GeneratedTaskExecution,
    ) -> Result<(), EvolutionError> {
        self.with_state(
            |state| {
                replace_by(&mut state.executions, execution, |value| {
                    value.execution_id.clone()
                });
                Ok(())
            },
            true,
        )
    }

    pub fn start_run(&self, run_id: &str) -> Result<OpenEndedRun, EvolutionError> {
        self.with_state(
            |state| {
                let run = state
                    .runs
                    .iter_mut()
                    .find(|run| run.run_id == run_id)
                    .ok_or_else(|| EvolutionError::RunNotFound(run_id.to_owned()))?;
                if run.status != OpenEndedRunStatus::Planned {
                    return Err(EvolutionError::SkillState(format!(
                        "evolution run {run_id} is {:?}",
                        run.status
                    )));
                }
                run.status = OpenEndedRunStatus::Running;
                run.blocked_reason = None;
                Ok(run.clone())
            },
            true,
        )
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        blocked_reason: Option<String>,
    ) -> Result<OpenEndedRun, EvolutionError> {
        self.with_state(
            |state| {
                let run = state
                    .runs
                    .iter_mut()
                    .find(|run| run.run_id == run_id)
                    .ok_or_else(|| EvolutionError::RunNotFound(run_id.to_owned()))?;
                if run.status != OpenEndedRunStatus::Running {
                    return Err(EvolutionError::SkillState(format!(
                        "evolution run {run_id} is {:?}",
                        run.status
                    )));
                }
                run.status = if blocked_reason.is_some() {
                    OpenEndedRunStatus::Blocked
                } else {
                    OpenEndedRunStatus::Completed
                };
                run.blocked_reason = blocked_reason;
                run.completed_at = Some(Utc::now());
                Ok(run.clone())
            },
            true,
        )
    }

    pub fn stage_skill(
        &self,
        candidate: &SkillCandidate,
    ) -> Result<SkillLifecycleRecord, EvolutionError> {
        validate_skill_id(&candidate.id)?;
        if candidate.evidence_refs.is_empty()
            || candidate.rollback_ref.trim().is_empty()
            || candidate.scope != "project"
        {
            return Err(EvolutionError::SkillGate(
                "skill candidate requires project scope, evidence, and rollback metadata"
                    .to_owned(),
            ));
        }
        let name = skill_name(&candidate.reusable_pattern);
        let manifest = SkillManifest {
            skill_id: candidate.id.clone(),
            name,
            description: candidate.reusable_pattern.clone(),
            source_task_id: candidate.source_task_id,
            source_trajectory: candidate.source_trajectory.clone(),
            prerequisites: vec!["matching workspace objective".to_owned()],
            steps: vec![candidate.reusable_pattern.clone()],
            failure_cases: Vec::new(),
            evidence_refs: candidate.evidence_refs.clone(),
            regression_refs: candidate.regression_refs.clone(),
            scope: candidate.scope.clone(),
            rollback_ref: candidate.rollback_ref.clone(),
        };
        let content = render_skill_markdown(&manifest);
        let candidate_dir = self.skills_root.join("candidates").join(&candidate.id);
        ensure_private_dir(&candidate_dir)?;
        let candidate_path = candidate_dir.join("SKILL.md");
        write_private_atomic(&candidate_path, content.as_bytes())?;
        let checksum = format!("sha256:{:x}", Sha256::digest(content.as_bytes()));
        let record = SkillLifecycleRecord {
            manifest,
            status: SkillLifecycleStatus::Proposed,
            candidate_path: candidate_path.display().to_string(),
            installed_path: None,
            checksum,
            reviewer: None,
            review_reason: None,
            created_at: Utc::now(),
            reviewed_at: None,
            installed_at: None,
            rolled_back_at: None,
            rollback_reason: None,
        };
        self.with_state(
            |state| {
                replace_by(&mut state.skills, record.clone(), |value| {
                    value.manifest.skill_id.clone()
                });
                Ok(record)
            },
            true,
        )
    }

    pub fn review_skill(
        &self,
        skill_id: &str,
        reviewer: &str,
        reason: &str,
        regression_refs: Vec<String>,
        approved: bool,
    ) -> Result<SkillLifecycleRecord, EvolutionError> {
        validate_skill_id(skill_id)?;
        if reviewer.trim().is_empty() || reason.trim().is_empty() {
            return Err(EvolutionError::SkillGate(
                "skill reviewer and reason are required".to_owned(),
            ));
        }
        if approved && regression_refs.is_empty() {
            return Err(EvolutionError::SkillGate(
                "approved skill requires at least one regression result".to_owned(),
            ));
        }
        self.with_state(
            |state| {
                let record = state
                    .skills
                    .iter_mut()
                    .find(|record| record.manifest.skill_id == skill_id)
                    .ok_or_else(|| EvolutionError::SkillNotFound(skill_id.to_owned()))?;
                if record.status != SkillLifecycleStatus::Proposed {
                    return Err(EvolutionError::SkillState(format!(
                        "skill {skill_id} is {:?}",
                        record.status
                    )));
                }
                record.manifest.regression_refs = regression_refs;
                record.reviewer = Some(reviewer.to_owned());
                record.review_reason = Some(reason.to_owned());
                record.reviewed_at = Some(Utc::now());
                record.status = if approved {
                    SkillLifecycleStatus::Reviewed
                } else {
                    SkillLifecycleStatus::Rejected
                };
                Ok(record.clone())
            },
            true,
        )
    }

    pub fn install_skill(&self, skill_id: &str) -> Result<SkillLifecycleRecord, EvolutionError> {
        validate_skill_id(skill_id)?;
        self.with_state(
            |state| {
                let record = state
                    .skills
                    .iter_mut()
                    .find(|record| record.manifest.skill_id == skill_id)
                    .ok_or_else(|| EvolutionError::SkillNotFound(skill_id.to_owned()))?;
                if record.status != SkillLifecycleStatus::Reviewed {
                    return Err(EvolutionError::SkillState(format!(
                        "skill {skill_id} must be reviewed before install"
                    )));
                }
                let expected_source = self
                    .skills_root
                    .join("candidates")
                    .join(skill_id)
                    .join("SKILL.md");
                if Path::new(&record.candidate_path) != expected_source {
                    return Err(EvolutionError::SkillGate(
                        "staged skill path does not match the governed skill store".to_owned(),
                    ));
                }
                let source = fs::read(&expected_source)
                    .map_err(|error| EvolutionError::Io(error.to_string()))?;
                let checksum = format!("sha256:{:x}", Sha256::digest(&source));
                if checksum != record.checksum {
                    return Err(EvolutionError::SkillGate(
                        "staged skill checksum changed after review".to_owned(),
                    ));
                }
                let active_dir = self.skills_root.join("active").join(skill_id);
                ensure_private_dir(&active_dir)?;
                let installed_path = active_dir.join("SKILL.md");
                write_private_atomic(&installed_path, &source)?;
                record.installed_path = Some(installed_path.display().to_string());
                record.installed_at = Some(Utc::now());
                record.status = SkillLifecycleStatus::Installed;
                Ok(record.clone())
            },
            true,
        )
    }

    pub fn rollback_skill(
        &self,
        skill_id: &str,
        reason: &str,
    ) -> Result<SkillLifecycleRecord, EvolutionError> {
        validate_skill_id(skill_id)?;
        if reason.trim().is_empty() {
            return Err(EvolutionError::SkillGate(
                "skill rollback reason is required".to_owned(),
            ));
        }
        self.with_state(
            |state| {
                let record = state
                    .skills
                    .iter_mut()
                    .find(|record| record.manifest.skill_id == skill_id)
                    .ok_or_else(|| EvolutionError::SkillNotFound(skill_id.to_owned()))?;
                if record.status != SkillLifecycleStatus::Installed {
                    return Err(EvolutionError::SkillState(format!(
                        "skill {skill_id} is not installed"
                    )));
                }
                let expected_path = self
                    .skills_root
                    .join("active")
                    .join(skill_id)
                    .join("SKILL.md");
                if record.installed_path.as_deref().map(Path::new) != Some(expected_path.as_path())
                {
                    return Err(EvolutionError::SkillGate(
                        "installed skill path does not match the governed skill store".to_owned(),
                    ));
                }
                if let Some(path) = record.installed_path.as_deref().map(PathBuf::from) {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(EvolutionError::Io(error.to_string())),
                    }
                }
                record.status = SkillLifecycleStatus::RolledBack;
                record.rolled_back_at = Some(Utc::now());
                record.rollback_reason = Some(reason.to_owned());
                Ok(record.clone())
            },
            true,
        )
    }

    pub fn active_skill_context(
        &self,
        objective: &str,
        limit: usize,
    ) -> Result<Vec<SkillManifest>, EvolutionError> {
        let objective_terms = terms(objective);
        let mut matches = self
            .snapshot()?
            .skills
            .into_iter()
            .filter(|record| record.status == SkillLifecycleStatus::Installed)
            .filter_map(|record| {
                let skill_terms = terms(&format!(
                    "{} {}",
                    record.manifest.name, record.manifest.description
                ));
                let score = objective_terms.intersection(&skill_terms).count();
                (score > 0).then_some((score, record.manifest))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.0.cmp(&left.0));
        Ok(matches
            .into_iter()
            .take(limit.max(1))
            .map(|(_, manifest)| manifest)
            .collect())
    }

    fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut EvolutionState) -> Result<T, EvolutionError>,
        save: bool,
    ) -> Result<T, EvolutionError> {
        let parent = self.state_path.parent().ok_or_else(|| {
            EvolutionError::Io(format!("{} has no parent", self.state_path.display()))
        })?;
        ensure_private_dir(parent)?;
        let lock_path = self.state_path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| EvolutionError::Io(error.to_string()))?;
        set_owner_only_file(&lock_path)?;
        lock.lock_exclusive()
            .map_err(|error| EvolutionError::Io(error.to_string()))?;
        let mut state = read_state(&self.state_path)?;
        let result = operation(&mut state)?;
        if save {
            let encoded = serde_json::to_vec_pretty(&state)?;
            if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_EVOLUTION_STATE_BYTES {
                return Err(EvolutionError::Limit(
                    "serialized evolution state is too large".to_owned(),
                ));
            }
            write_private_atomic(&self.state_path, &encoded)?;
        }
        Ok(result)
    }
}

fn render_skill_markdown(manifest: &SkillManifest) -> String {
    let name = serde_json::to_string(&manifest.name).unwrap_or_else(|_| "\"skill\"".to_owned());
    let description = serde_json::to_string(&manifest.description)
        .unwrap_or_else(|_| "\"verified workspace skill\"".to_owned());
    let prerequisites = manifest
        .prerequisites
        .iter()
        .map(|value| format!("- {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let steps = manifest
        .steps
        .iter()
        .enumerate()
        .map(|(index, value)| format!("{}. {value}", index.saturating_add(1)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n# {}\n\n## Prerequisites\n{}\n\n## Workflow\n{}\n",
        name, description, manifest.name, prerequisites, steps
    )
}

fn validate_skill_id(skill_id: &str) -> Result<(), EvolutionError> {
    if skill_id.is_empty()
        || skill_id.len() > 128
        || !skill_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(EvolutionError::SkillGate(
            "skill id must contain only ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }
    Ok(())
}

fn skill_name(value: &str) -> String {
    let name = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .take(6)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-");
    if name.is_empty() {
        "verified-workspace-skill".to_owned()
    } else {
        name
    }
}

fn terms(value: &str) -> std::collections::HashSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= 2)
        .collect()
}

fn read_state(path: &Path) -> Result<EvolutionState, EvolutionError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EvolutionState::default());
        }
        Err(error) => return Err(EvolutionError::Io(error.to_string())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| EvolutionError::Io(error.to_string()))?;
    if metadata.len() > MAX_EVOLUTION_STATE_BYTES {
        return Err(EvolutionError::Limit(
            "evolution state file is too large".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_EVOLUTION_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| EvolutionError::Io(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_EVOLUTION_STATE_BYTES {
        return Err(EvolutionError::Limit(
            "evolution state grew while reading".to_owned(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(EvolutionError::from)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), EvolutionError> {
    let parent = path
        .parent()
        .ok_or_else(|| EvolutionError::Io(format!("{} has no parent", path.display())))?;
    ensure_private_dir(parent)?;
    reject_symlink(path)?;
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| EvolutionError::Io(error.to_string()))?;
    file.write_all(bytes)
        .map_err(|error| EvolutionError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| EvolutionError::Io(error.to_string()))?;
    set_owner_only_file(&temporary)?;
    replace_file(&temporary, path)?;
    set_owner_only_file(path)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| EvolutionError::Io(error.to_string()))
}

fn reject_symlink(path: &Path) -> Result<(), EvolutionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(EvolutionError::Io(format!(
            "state path cannot be a symbolic link: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(EvolutionError::Io(error.to_string())),
    }
}

fn replace_file(source: &Path, target: &Path) -> Result<(), EvolutionError> {
    #[cfg(windows)]
    if target.exists() {
        fs::remove_file(target).map_err(|error| EvolutionError::Io(error.to_string()))?;
    }
    fs::rename(source, target).map_err(|error| EvolutionError::Io(error.to_string()))
}

fn ensure_private_dir(path: &Path) -> Result<(), EvolutionError> {
    fs::create_dir_all(path).map_err(|error| EvolutionError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| EvolutionError::Io(error.to_string()))?;
    }
    Ok(())
}

fn set_owner_only_file(path: &Path) -> Result<(), EvolutionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| EvolutionError::Io(error.to_string()))?;
    }
    Ok(())
}

fn replace_by<T, K: PartialEq>(values: &mut Vec<T>, value: T, key: impl Fn(&T) -> K) {
    let value_key = key(&value);
    if let Some(index) = values
        .iter()
        .position(|existing| key(existing) == value_key)
    {
        values[index] = value;
    } else {
        values.push(value);
    }
}
