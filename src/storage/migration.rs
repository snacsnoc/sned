use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonObjectMigration {
    pub relative_path: PathBuf,
    pub source_keys: Vec<String>,
    pub destination_keys: Vec<String>,
    pub copied_keys: Vec<String>,
    pub skipped_existing_keys: Vec<String>,
    pub conflicting_keys: Vec<String>,
}

impl JsonObjectMigration {
    pub fn is_in_sync(&self) -> bool {
        self.copied_keys.is_empty() && self.conflicting_keys.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskHistoryMigration {
    pub relative_path: PathBuf,
    pub source_count: usize,
    pub destination_count: usize,
    pub copied_ids: Vec<String>,
    pub skipped_existing_ids: Vec<String>,
    pub conflicting_ids: Vec<String>,
}

impl TaskHistoryMigration {
    pub fn merged_count(&self) -> usize {
        self.destination_count + self.copied_ids.len()
    }

    pub fn is_in_sync(&self) -> bool {
        self.copied_ids.is_empty() && self.conflicting_ids.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDirectoryMigration {
    pub task_id: String,
    pub source_file_count: usize,
    pub destination_file_count: usize,
    pub copied_files: Vec<PathBuf>,
    pub skipped_existing_files: Vec<PathBuf>,
    pub conflicting_files: Vec<PathBuf>,
}

impl TaskDirectoryMigration {
    pub fn is_in_sync(&self) -> bool {
        self.copied_files.is_empty() && self.conflicting_files.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunMigrationReport {
    pub source_root: PathBuf,
    pub destination_root: PathBuf,
    pub endpoints: Option<JsonObjectMigration>,
    pub global_settings: Option<JsonObjectMigration>,
    pub secrets: Option<JsonObjectMigration>,
    pub task_history: Option<TaskHistoryMigration>,
    pub tasks: Vec<TaskDirectoryMigration>,
}

impl DryRunMigrationReport {
    pub fn has_changes(&self) -> bool {
        self.endpoints
            .as_ref()
            .is_some_and(|report| !report.is_in_sync())
            || self
                .global_settings
                .as_ref()
                .is_some_and(|report| !report.is_in_sync())
            || self
                .secrets
                .as_ref()
                .is_some_and(|report| !report.is_in_sync())
            || self
                .task_history
                .as_ref()
                .is_some_and(|report| !report.is_in_sync())
            || self.tasks.iter().any(|report| !report.is_in_sync())
    }

    pub fn total_copied_files(&self) -> usize {
        let mut total = 0;

        if let Some(report) = &self.endpoints {
            total += report.copied_keys.len();
        }
        if let Some(report) = &self.global_settings {
            total += report.copied_keys.len();
        }
        if let Some(report) = &self.secrets {
            total += report.copied_keys.len();
        }
        if let Some(report) = &self.task_history {
            total += report.copied_ids.len();
        }
        total
            + self
                .tasks
                .iter()
                .map(|task| task.copied_files.len())
                .sum::<usize>()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported JSON structure in {path}: {message}")]
    UnsupportedJson { path: PathBuf, message: String },
    #[error("migration failed: {message}")]
    MigrationFailed { message: String },
    #[error("rollback failed: {message}")]
    RollbackFailed { message: String },
}

pub fn plan_dry_run_migration(
    source_root: impl AsRef<Path>,
    destination_root: impl AsRef<Path>,
) -> Result<DryRunMigrationReport, MigrationError> {
    let source_root = source_root.as_ref().to_path_buf();
    let destination_root = destination_root.as_ref().to_path_buf();

    let endpoints = compare_json_object_file(
        source_root.join("endpoints.json"),
        destination_root.join("endpoints.json"),
        PathBuf::from("endpoints.json"),
    )?;

    let global_settings = compare_json_object_file(
        source_root.join("data/settings/global_settings.json"),
        destination_root.join("data/settings/global_settings.json"),
        PathBuf::from("data/settings/global_settings.json"),
    )?;

    let secrets = compare_json_object_file(
        source_root.join(".secrets.json"),
        destination_root.join(".secrets.json"),
        PathBuf::from(".secrets.json"),
    )?;

    let task_history = compare_task_history(
        source_root.join("data/state/taskHistory.json"),
        destination_root.join("data/state/taskHistory.json"),
        PathBuf::from("data/state/taskHistory.json"),
    )?;

    let tasks = compare_task_directories(
        source_root.join("data/tasks"),
        destination_root.join("data/tasks"),
    )?;

    Ok(DryRunMigrationReport {
        source_root,
        destination_root,
        endpoints,
        global_settings,
        secrets,
        task_history,
        tasks,
    })
}

#[derive(Debug, Clone)]
pub struct ExecutedOperation {
    pub file_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub operation_type: OperationType,
}

#[derive(Debug, Clone)]
pub enum OperationType {
    CreateFile,
    UpdateFile,
    CopyFile,
    CreateDir,
}

#[derive(Debug)]
pub struct MigrationExecutionReport {
    pub source_root: PathBuf,
    pub destination_root: PathBuf,
    pub endpoints: Option<JsonObjectMigration>,
    pub global_settings: Option<JsonObjectMigration>,
    pub secrets: Option<JsonObjectMigration>,
    pub task_history: Option<TaskHistoryMigration>,
    pub tasks: Vec<TaskDirectoryMigration>,
    pub executed_operations: Vec<ExecutedOperation>,
    pub success: bool,
}

impl MigrationExecutionReport {
    pub fn has_changes(&self) -> bool {
        self.endpoints
            .as_ref()
            .is_some_and(|report| !report.is_in_sync())
            || self
                .global_settings
                .as_ref()
                .is_some_and(|report| !report.is_in_sync())
            || self
                .secrets
                .as_ref()
                .is_some_and(|report| !report.is_in_sync())
            || self
                .task_history
                .as_ref()
                .is_some_and(|report| !report.is_in_sync())
            || self.tasks.iter().any(|report| !report.is_in_sync())
    }
}

pub struct MigrationEngine {
    source_root: PathBuf,
    destination_root: PathBuf,
    executed_operations: Vec<ExecutedOperation>,
}

impl MigrationEngine {
    pub fn new(source_root: impl AsRef<Path>, destination_root: impl AsRef<Path>) -> Self {
        Self {
            source_root: source_root.as_ref().to_path_buf(),
            destination_root: destination_root.as_ref().to_path_buf(),
            executed_operations: Vec::new(),
        }
    }

    pub fn plan(&self) -> Result<DryRunMigrationReport, MigrationError> {
        plan_dry_run_migration(&self.source_root, &self.destination_root)
    }

    pub fn execute(&mut self) -> Result<MigrationExecutionReport, MigrationError> {
        self.executed_operations.clear();

        let endpoints =
            self.execute_json_object_migration("endpoints.json", PathBuf::from("endpoints.json"))?;

        let global_settings = self.execute_json_object_migration(
            "data/settings/global_settings.json",
            PathBuf::from("data/settings/global_settings.json"),
        )?;

        let secrets =
            self.execute_json_object_migration(".secrets.json", PathBuf::from(".secrets.json"))?;

        let task_history = self.execute_task_history_migration(
            "data/state/taskHistory.json",
            PathBuf::from("data/state/taskHistory.json"),
        )?;

        let tasks = self.execute_task_directories_migration()?;

        self.execute_agents_migration()?;

        for op in &self.executed_operations {
            if let Some(ref backup) = op.backup_path {
                let _ = fs::remove_file(backup);
            }
        }

        Ok(MigrationExecutionReport {
            source_root: self.source_root.clone(),
            destination_root: self.destination_root.clone(),
            endpoints,
            global_settings,
            secrets,
            task_history,
            tasks,
            executed_operations: std::mem::take(&mut self.executed_operations),
            success: true,
        })
    }

    pub fn rollback(&mut self) -> Result<(), MigrationError> {
        let mut errors = Vec::new();

        for op in self.executed_operations.iter().rev() {
            match op.operation_type {
                OperationType::CreateFile | OperationType::CopyFile => {
                    if let Some(backup_path) = &op.backup_path {
                        if backup_path.exists() {
                            if let Some(parent) = op.file_path.parent() {
                                let _ = fs::create_dir_all(parent);
                            }
                            if let Err(e) = fs::copy(backup_path, &op.file_path) {
                                errors.push(format!(
                                    "failed to restore {} from backup: {}",
                                    op.file_path.display(),
                                    e
                                ));
                            }
                        }
                    } else if op.file_path.exists()
                        && let Err(e) = fs::remove_file(&op.file_path)
                    {
                        errors.push(format!(
                            "failed to remove created file {}: {}",
                            op.file_path.display(),
                            e
                        ));
                    }
                }
                OperationType::UpdateFile => {
                    if let Some(backup_path) = &op.backup_path
                        && backup_path.exists()
                        && let Err(e) = fs::copy(backup_path, &op.file_path)
                    {
                        errors.push(format!(
                            "failed to restore {} from backup: {}",
                            op.file_path.display(),
                            e
                        ));
                    }
                }
                OperationType::CreateDir => {
                    if op.file_path.exists()
                        && let Err(e) = fs::remove_dir(&op.file_path)
                    {
                        errors.push(format!(
                            "failed to remove created directory {}: {}",
                            op.file_path.display(),
                            e
                        ));
                    }
                }
            }
        }

        if !errors.is_empty() {
            return Err(MigrationError::RollbackFailed {
                message: errors.join("; "),
            });
        }

        self.executed_operations.clear();
        Ok(())
    }

    fn execute_json_object_migration(
        &mut self,
        relative_path: &str,
        _report_path: PathBuf,
    ) -> Result<Option<JsonObjectMigration>, MigrationError> {
        let source_path = self.source_root.join(relative_path);
        let destination_path = self.destination_root.join(relative_path);

        if !source_path.exists() {
            return Ok(None);
        }

        let source = read_json_value(&source_path)?;
        let destination = if destination_path.exists() {
            read_json_value(&destination_path)?
        } else {
            Value::Object(Default::default())
        };

        let source_obj = as_object(&source, &source_path)?;
        let destination_obj = as_object(&destination, &destination_path)?;

        let mut copied_keys = Vec::new();
        let mut skipped_existing_keys = Vec::new();
        let mut conflicting_keys = Vec::new();

        for key in source_obj.keys() {
            match destination_obj.get(key) {
                None => copied_keys.push(key.clone()),
                Some(dest_value) => {
                    skipped_existing_keys.push(key.clone());
                    if source_obj.get(key) != Some(dest_value) {
                        conflicting_keys.push(key.clone());
                    }
                }
            }
        }

        copied_keys.sort();
        skipped_existing_keys.sort();
        conflicting_keys.sort();

        if copied_keys.is_empty() && conflicting_keys.is_empty() {
            return Ok(Some(JsonObjectMigration {
                relative_path: PathBuf::from(relative_path),
                source_keys: source_obj.keys().cloned().collect(),
                destination_keys: destination_obj.keys().cloned().collect(),
                copied_keys: Vec::new(),
                skipped_existing_keys,
                conflicting_keys,
            }));
        }

        let mut merged = destination_obj.clone();

        for key in &copied_keys {
            if let Some(value) = source_obj.get(key) {
                merged.insert(key.clone(), value.clone());
            }
        }

        let backup_path = if destination_path.exists() {
            let backup = destination_path.with_extension("bak");
            fs::copy(&destination_path, &backup).map_err(|source| MigrationError::Io {
                path: destination_path.clone(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                let _ = fs::set_permissions(&backup, perms);
            }
            Some(backup)
        } else {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent).map_err(|source| MigrationError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            None
        };

        let json_str =
            serde_json::to_string_pretty(&Value::Object(merged.clone())).map_err(|source| {
                MigrationError::InvalidJson {
                    path: destination_path.clone(),
                    source,
                }
            })?;

        fs::write(&destination_path, json_str).map_err(|source| MigrationError::Io {
            path: destination_path.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            fs::set_permissions(&destination_path, perms).map_err(|source| MigrationError::Io {
                path: destination_path.clone(),
                source,
            })?;
        }

        let operation_type = if backup_path.is_some() {
            OperationType::UpdateFile
        } else {
            OperationType::CreateFile
        };

        self.executed_operations.push(ExecutedOperation {
            file_path: destination_path.clone(),
            backup_path,
            operation_type,
        });

        Ok(Some(JsonObjectMigration {
            relative_path: PathBuf::from(relative_path),
            source_keys: source_obj.keys().cloned().collect(),
            destination_keys: merged.keys().cloned().collect(),
            copied_keys,
            skipped_existing_keys,
            conflicting_keys,
        }))
    }

    fn execute_task_history_migration(
        &mut self,
        relative_path: &str,
        _report_path: PathBuf,
    ) -> Result<Option<TaskHistoryMigration>, MigrationError> {
        let source_path = self.source_root.join(relative_path);
        let destination_path = self.destination_root.join(relative_path);

        if !source_path.exists() {
            return Ok(None);
        }

        let source = read_json_value(&source_path)?;
        let destination = if destination_path.exists() {
            read_json_value(&destination_path)?
        } else {
            Value::Array(Vec::new())
        };

        let source_items = as_array(&source, &source_path)?;
        let destination_items = as_array(&destination, &destination_path)?;

        let mut destination_by_id: BTreeMap<String, Value> = BTreeMap::new();
        for item in destination_items {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                destination_by_id.insert(id.to_string(), item.clone());
            }
        }

        let mut copied_ids = Vec::new();
        let mut skipped_existing_ids = Vec::new();
        let mut conflicting_ids = Vec::new();

        let mut final_items = destination_items.to_vec();

        for item in source_items {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };

            match destination_by_id.get(id) {
                None => {
                    copied_ids.push(id.to_string());
                    final_items.push(item.clone());
                }
                Some(dest_item) => {
                    skipped_existing_ids.push(id.to_string());
                    if *dest_item != *item {
                        conflicting_ids.push(id.to_string());
                    }
                }
            }
        }

        copied_ids.sort();
        skipped_existing_ids.sort();
        conflicting_ids.sort();

        if copied_ids.is_empty() && conflicting_ids.is_empty() {
            return Ok(Some(TaskHistoryMigration {
                relative_path: PathBuf::from(relative_path),
                source_count: source_items.len(),
                destination_count: destination_items.len(),
                copied_ids: Vec::new(),
                skipped_existing_ids,
                conflicting_ids,
            }));
        }

        let backup_path = if destination_path.exists() {
            let backup = destination_path.with_extension("bak");
            fs::copy(&destination_path, &backup).map_err(|source| MigrationError::Io {
                path: destination_path.clone(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                let _ = fs::set_permissions(&backup, perms);
            }
            Some(backup)
        } else {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent).map_err(|source| MigrationError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            None
        };

        let json_str =
            serde_json::to_string_pretty(&Value::Array(final_items)).map_err(|source| {
                MigrationError::InvalidJson {
                    path: destination_path.clone(),
                    source,
                }
            })?;

        fs::write(&destination_path, json_str).map_err(|source| MigrationError::Io {
            path: destination_path.clone(),
            source,
        })?;

        let operation_type = if backup_path.is_some() {
            OperationType::UpdateFile
        } else {
            OperationType::CreateFile
        };

        self.executed_operations.push(ExecutedOperation {
            file_path: destination_path.clone(),
            backup_path,
            operation_type,
        });

        Ok(Some(TaskHistoryMigration {
            relative_path: PathBuf::from(relative_path),
            source_count: source_items.len(),
            destination_count: destination_items.len(),
            copied_ids,
            skipped_existing_ids,
            conflicting_ids,
        }))
    }

    fn execute_task_directories_migration(
        &mut self,
    ) -> Result<Vec<TaskDirectoryMigration>, MigrationError> {
        let source_tasks_dir = self.source_root.join("data/tasks");
        let destination_tasks_dir = self.destination_root.join("data/tasks");

        if !source_tasks_dir.exists() {
            return Ok(Vec::new());
        }

        let mut task_ids = BTreeSet::new();

        if source_tasks_dir.exists() {
            for entry in fs::read_dir(&source_tasks_dir).map_err(|source| MigrationError::Io {
                path: source_tasks_dir.clone(),
                source,
            })? {
                let entry = entry.map_err(|source| MigrationError::Io {
                    path: source_tasks_dir.clone(),
                    source,
                })?;
                if entry
                    .file_type()
                    .map_err(|source| MigrationError::Io {
                        path: entry.path(),
                        source,
                    })?
                    .is_dir()
                {
                    task_ids.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }

        if destination_tasks_dir.exists() {
            for entry in
                fs::read_dir(&destination_tasks_dir).map_err(|source| MigrationError::Io {
                    path: destination_tasks_dir.clone(),
                    source,
                })?
            {
                let entry = entry.map_err(|source| MigrationError::Io {
                    path: destination_tasks_dir.clone(),
                    source,
                })?;
                if entry
                    .file_type()
                    .map_err(|source| MigrationError::Io {
                        path: entry.path(),
                        source,
                    })?
                    .is_dir()
                {
                    task_ids.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }

        let mut reports = Vec::new();

        for task_id in task_ids {
            let source_task_dir = source_tasks_dir.join(&task_id);
            let destination_task_dir = destination_tasks_dir.join(&task_id);

            let report = self.execute_single_task_directory_migration(
                &task_id,
                &source_task_dir,
                &destination_task_dir,
            )?;
            reports.push(report);
        }

        Ok(reports)
    }

    fn execute_single_task_directory_migration(
        &mut self,
        task_id: &str,
        source_task_dir: &Path,
        destination_task_dir: &Path,
    ) -> Result<TaskDirectoryMigration, MigrationError> {
        let source_files = collect_files(source_task_dir)?;
        let destination_files = collect_files(destination_task_dir)?;

        let source_relative: BTreeSet<PathBuf> = source_files
            .iter()
            .filter_map(|file| file.strip_prefix(source_task_dir).ok().map(PathBuf::from))
            .collect();
        let destination_relative: BTreeSet<PathBuf> = destination_files
            .iter()
            .filter_map(|file| {
                file.strip_prefix(destination_task_dir)
                    .ok()
                    .map(PathBuf::from)
            })
            .collect();

        let mut copied_files = Vec::new();
        let mut skipped_existing_files = Vec::new();
        let mut conflicting_files = Vec::new();

        for relative in &source_relative {
            let source_file = source_task_dir.join(relative);
            let destination_file = destination_task_dir.join(relative);

            match fs::metadata(&destination_file) {
                Ok(metadata) if metadata.is_file() => {
                    skipped_existing_files.push(relative.clone());
                    let source_bytes =
                        fs::read(&source_file).map_err(|source| MigrationError::Io {
                            path: source_file.clone(),
                            source,
                        })?;
                    let destination_bytes =
                        fs::read(&destination_file).map_err(|source| MigrationError::Io {
                            path: destination_file.clone(),
                            source,
                        })?;
                    if source_bytes != destination_bytes {
                        conflicting_files.push(relative.clone());
                    }
                }
                Ok(_) => {
                    copied_files.push(relative.clone());
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    copied_files.push(relative.clone());
                }
                Err(source) => {
                    return Err(MigrationError::Io {
                        path: destination_file,
                        source,
                    });
                }
            }
        }

        for relative in &copied_files {
            let source_file = source_task_dir.join(relative);
            let destination_file = destination_task_dir.join(relative);

            if let Some(parent) = destination_file.parent() {
                fs::create_dir_all(parent).map_err(|source| MigrationError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }

            let backup_path = if destination_file.exists() {
                let backup = destination_file.with_extension("bak");
                fs::copy(&destination_file, &backup).map_err(|source| MigrationError::Io {
                    path: destination_file.clone(),
                    source,
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(0o600);
                    let _ = fs::set_permissions(&backup, perms);
                }
                Some(backup)
            } else {
                None
            };

            fs::copy(&source_file, &destination_file).map_err(|source| MigrationError::Io {
                path: destination_file.clone(),
                source,
            })?;

            let op_type = if backup_path.is_some() {
                OperationType::UpdateFile
            } else {
                OperationType::CreateFile
            };

            self.executed_operations.push(ExecutedOperation {
                file_path: destination_file,
                backup_path,
                operation_type: op_type,
            });
        }

        copied_files.sort();
        skipped_existing_files.sort();
        conflicting_files.sort();

        Ok(TaskDirectoryMigration {
            task_id: task_id.to_string(),
            source_file_count: source_relative.len(),
            destination_file_count: destination_relative.len(),
            copied_files,
            skipped_existing_files,
            conflicting_files,
        })
    }

    fn execute_agents_migration(&mut self) -> Result<(), MigrationError> {
        let source_rules_dir = self.source_root.join(".agents");
        let destination_rules_dir = self.destination_root.join(".agents");

        if !source_rules_dir.exists() {
            return Ok(());
        }

        if !destination_rules_dir.exists() {
            fs::create_dir_all(&destination_rules_dir).map_err(|source| MigrationError::Io {
                path: destination_rules_dir.clone(),
                source,
            })?;

            self.executed_operations.push(ExecutedOperation {
                file_path: destination_rules_dir.clone(),
                backup_path: None,
                operation_type: OperationType::CreateDir,
            });
        }

        for entry in WalkDir::new(&source_rules_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(&source_rules_dir)
                    .ok()
                    .map(PathBuf::from);

                if let Some(relative) = relative {
                    let source_file = entry.path();
                    let destination_file = destination_rules_dir.join(&relative);

                    if let Some(parent) = destination_file.parent() {
                        fs::create_dir_all(parent).map_err(|source| MigrationError::Io {
                            path: parent.to_path_buf(),
                            source,
                        })?;
                    }

                    let backup_path = if destination_file.exists() {
                        let backup = destination_file.with_extension("bak");
                        fs::copy(&destination_file, &backup).map_err(|source| {
                            MigrationError::Io {
                                path: destination_file.clone(),
                                source,
                            }
                        })?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let perms = std::fs::Permissions::from_mode(0o600);
                            let _ = fs::set_permissions(&backup, perms);
                        }
                        Some(backup)
                    } else {
                        None
                    };

                    fs::copy(source_file, &destination_file).map_err(|source| {
                        MigrationError::Io {
                            path: destination_file.clone(),
                            source,
                        }
                    })?;

                    let op_type = if backup_path.is_some() {
                        OperationType::UpdateFile
                    } else {
                        OperationType::CopyFile
                    };

                    self.executed_operations.push(ExecutedOperation {
                        file_path: destination_file,
                        backup_path,
                        operation_type: op_type,
                    });
                }
            }
        }

        Ok(())
    }
}

fn compare_json_object_file(
    source_path: PathBuf,
    destination_path: PathBuf,
    relative_path: PathBuf,
) -> Result<Option<JsonObjectMigration>, MigrationError> {
    let source_exists = source_path.exists();
    let destination_exists = destination_path.exists();

    if !source_exists {
        return Ok(None);
    }

    let source = if source_exists {
        read_json_value(&source_path)?
    } else {
        Value::Object(Default::default())
    };
    let destination = if destination_exists {
        read_json_value(&destination_path)?
    } else {
        Value::Object(Default::default())
    };

    let source_obj = as_object(&source, &source_path)?;
    let destination_obj = as_object(&destination, &destination_path)?;

    let source_keys: BTreeSet<String> = source_obj.keys().cloned().collect();
    let destination_keys: BTreeSet<String> = destination_obj.keys().cloned().collect();

    let mut copied_keys = Vec::new();
    let mut skipped_existing_keys = Vec::new();
    let mut conflicting_keys = Vec::new();

    for key in &source_keys {
        match destination_obj.get(key) {
            None => copied_keys.push(key.clone()),
            Some(dest_value) => {
                skipped_existing_keys.push(key.clone());
                if source_obj.get(key) != Some(dest_value) {
                    conflicting_keys.push(key.clone());
                }
            }
        }
    }

    copied_keys.sort();
    skipped_existing_keys.sort();
    conflicting_keys.sort();

    Ok(Some(JsonObjectMigration {
        relative_path,
        source_keys: source_keys.into_iter().collect(),
        destination_keys: destination_keys.into_iter().collect(),
        copied_keys,
        skipped_existing_keys,
        conflicting_keys,
    }))
}

fn compare_task_history(
    source_path: PathBuf,
    destination_path: PathBuf,
    relative_path: PathBuf,
) -> Result<Option<TaskHistoryMigration>, MigrationError> {
    let source_exists = source_path.exists();
    let destination_exists = destination_path.exists();

    if !source_exists {
        return Ok(None);
    }

    let source = if source_exists {
        read_json_value(&source_path)?
    } else {
        Value::Array(Vec::new())
    };
    let destination = if destination_exists {
        read_json_value(&destination_path)?
    } else {
        Value::Array(Vec::new())
    };

    let source_items = as_array(&source, &source_path)?;
    let destination_items = as_array(&destination, &destination_path)?;

    let mut destination_by_id = BTreeMap::new();
    for item in destination_items {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            destination_by_id.insert(id.to_string(), item);
        }
    }

    let mut copied_ids = Vec::new();
    let mut skipped_existing_ids = Vec::new();
    let mut conflicting_ids = Vec::new();

    for item in source_items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };

        match destination_by_id.get(id) {
            None => copied_ids.push(id.to_string()),
            Some(dest_item) => {
                skipped_existing_ids.push(id.to_string());
                if *dest_item != item {
                    conflicting_ids.push(id.to_string());
                }
            }
        }
    }

    copied_ids.sort();
    skipped_existing_ids.sort();
    conflicting_ids.sort();

    Ok(Some(TaskHistoryMigration {
        relative_path,
        source_count: source_items.len(),
        destination_count: destination_items.len(),
        copied_ids,
        skipped_existing_ids,
        conflicting_ids,
    }))
}

fn compare_task_directories(
    source_dir: PathBuf,
    destination_dir: PathBuf,
) -> Result<Vec<TaskDirectoryMigration>, MigrationError> {
    if !source_dir.exists() {
        return Ok(Vec::new());
    }

    let mut task_ids = BTreeSet::new();

    if source_dir.exists() {
        for entry in fs::read_dir(&source_dir).map_err(|source| MigrationError::Io {
            path: source_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| MigrationError::Io {
                path: source_dir.clone(),
                source,
            })?;
            if entry
                .file_type()
                .map_err(|source| MigrationError::Io {
                    path: entry.path(),
                    source,
                })?
                .is_dir()
            {
                task_ids.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }

    if destination_dir.exists() {
        for entry in fs::read_dir(&destination_dir).map_err(|source| MigrationError::Io {
            path: destination_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| MigrationError::Io {
                path: destination_dir.clone(),
                source,
            })?;
            if entry
                .file_type()
                .map_err(|source| MigrationError::Io {
                    path: entry.path(),
                    source,
                })?
                .is_dir()
            {
                task_ids.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }

    let mut reports = Vec::new();

    for task_id in task_ids {
        let source_task_dir = source_dir.join(&task_id);
        let destination_task_dir = destination_dir.join(&task_id);
        reports.push(compare_single_task_directory(
            &task_id,
            &source_task_dir,
            &destination_task_dir,
        )?);
    }

    Ok(reports)
}

fn compare_single_task_directory(
    task_id: &str,
    source_task_dir: &Path,
    destination_task_dir: &Path,
) -> Result<TaskDirectoryMigration, MigrationError> {
    let source_files = collect_files(source_task_dir)?;
    let destination_files = collect_files(destination_task_dir)?;

    let source_relative: BTreeSet<PathBuf> = source_files
        .iter()
        .filter_map(|file| file.strip_prefix(source_task_dir).ok().map(PathBuf::from))
        .collect();
    let destination_relative: BTreeSet<PathBuf> = destination_files
        .iter()
        .filter_map(|file| {
            file.strip_prefix(destination_task_dir)
                .ok()
                .map(PathBuf::from)
        })
        .collect();

    let mut copied_files = Vec::new();
    let mut skipped_existing_files = Vec::new();
    let mut conflicting_files = Vec::new();

    for relative in &source_relative {
        let source_file = source_task_dir.join(relative);
        let destination_file = destination_task_dir.join(relative);

        match fs::metadata(&destination_file) {
            Ok(metadata) if metadata.is_file() => {
                skipped_existing_files.push(relative.clone());
                let source_bytes = fs::read(&source_file).map_err(|source| MigrationError::Io {
                    path: source_file.clone(),
                    source,
                })?;
                let destination_bytes =
                    fs::read(&destination_file).map_err(|source| MigrationError::Io {
                        path: destination_file.clone(),
                        source,
                    })?;
                if source_bytes != destination_bytes {
                    conflicting_files.push(relative.clone());
                }
            }
            Ok(_) => {
                copied_files.push(relative.clone());
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                copied_files.push(relative.clone());
            }
            Err(source) => {
                return Err(MigrationError::Io {
                    path: destination_file,
                    source,
                });
            }
        }
    }

    copied_files.sort();
    skipped_existing_files.sort();
    conflicting_files.sort();

    Ok(TaskDirectoryMigration {
        task_id: task_id.to_string(),
        source_file_count: source_relative.len(),
        destination_file_count: destination_relative.len(),
        copied_files,
        skipped_existing_files,
        conflicting_files,
    })
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, MigrationError> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn read_json_value(path: &Path) -> Result<Value, MigrationError> {
    let contents = fs::read_to_string(path).map_err(|source| MigrationError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| MigrationError::InvalidJson {
        path: path.to_path_buf(),
        source,
    })
}

fn as_object<'a>(
    value: &'a Value,
    path: &Path,
) -> Result<&'a serde_json::Map<String, Value>, MigrationError> {
    value
        .as_object()
        .ok_or_else(|| MigrationError::UnsupportedJson {
            path: path.to_path_buf(),
            message: "expected a JSON object".to_string(),
        })
}

fn as_array<'a>(value: &'a Value, path: &Path) -> Result<&'a Vec<Value>, MigrationError> {
    value
        .as_array()
        .ok_or_else(|| MigrationError::UnsupportedJson {
            path: path.to_path_buf(),
            message: "expected a JSON array".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn build_source_destination_fixture() -> (TempDir, TempDir) {
        let source = TempDir::new().unwrap();
        let destination = TempDir::new().unwrap();

        write_json(
            &source.path().join("endpoints.json"),
            &serde_json::json!({
                "appBaseUrl": "https://app.example.com",
                "apiBaseUrl": "https://api.example.com"
            }),
        );

        write_json(
            &source.path().join("data/settings/global_settings.json"),
            &serde_json::json!({
                "mode": "act",
                "planActSeparateModelsSetting": false,
                "featureFlag": true
            }),
        );

        write_json(
            &source.path().join(".secrets.json"),
            &serde_json::json!({
                "apiKey": "source-api-key",
                "openAiApiKey": "source-openai"
            }),
        );

        write_json(
            &source.path().join("data/state/taskHistory.json"),
            &serde_json::json!([
                { "id": "task-a", "ts": 2000, "task": "A" },
                { "id": "task-b", "ts": 1000, "task": "B" }
            ]),
        );

        write_json(
            &source.path().join("data/tasks/task-a/settings.json"),
            &serde_json::json!({
                "model": "gpt-4o"
            }),
        );
        write_json(
            &source.path().join("data/tasks/task-a/ui_messages.json"),
            &serde_json::json!([
                { "role": "user", "content": "hello" }
            ]),
        );

        write_json(
            &destination.path().join("endpoints.json"),
            &serde_json::json!({
                "appBaseUrl": "https://app.example.com",
                "apiBaseUrl": "https://api.dest.com"
            }),
        );

        write_json(
            &destination
                .path()
                .join("data/settings/global_settings.json"),
            &serde_json::json!({
                "mode": "plan",
                "planActSeparateModelsSetting": false
            }),
        );

        write_json(
            &destination.path().join(".secrets.json"),
            &serde_json::json!({
                "apiKey": "dest-api-key"
            }),
        );

        write_json(
            &destination.path().join("data/state/taskHistory.json"),
            &serde_json::json!([
                { "id": "task-a", "ts": 2000, "task": "A" }
            ]),
        );

        write_json(
            &destination.path().join("data/tasks/task-a/settings.json"),
            &serde_json::json!({
                "model": "claude-3-5-sonnet"
            }),
        );

        (source, destination)
    }

    #[test]
    fn dry_run_migration_reports_semantic_differences() {
        let (source, destination) = build_source_destination_fixture();
        let report = plan_dry_run_migration(source.path(), destination.path()).unwrap();

        let endpoints = report.endpoints.as_ref().unwrap();
        assert_eq!(endpoints.copied_keys, Vec::<String>::new());
        assert_eq!(endpoints.conflicting_keys, vec!["apiBaseUrl".to_string()]);
        assert!(!endpoints.is_in_sync());

        let global = report.global_settings.as_ref().unwrap();
        assert_eq!(global.copied_keys, vec!["featureFlag".to_string()]);
        assert_eq!(global.conflicting_keys, vec!["mode".to_string()]);
        assert!(!global.is_in_sync());

        let secrets = report.secrets.as_ref().unwrap();
        assert_eq!(secrets.copied_keys, vec!["openAiApiKey".to_string()]);
        assert_eq!(secrets.conflicting_keys, vec!["apiKey".to_string()]);

        let history = report.task_history.as_ref().unwrap();
        assert_eq!(history.copied_ids, vec!["task-b".to_string()]);
        assert!(history.conflicting_ids.is_empty());

        assert_eq!(report.tasks.len(), 1);
        let task = &report.tasks[0];
        assert_eq!(task.task_id, "task-a");
        assert_eq!(task.copied_files, vec![PathBuf::from("ui_messages.json")]);
        assert_eq!(task.conflicting_files, vec![PathBuf::from("settings.json")]);
        assert!(!task.is_in_sync());
    }

    #[test]
    fn dry_run_migration_rejects_invalid_json() {
        let source = TempDir::new().unwrap();
        let destination = TempDir::new().unwrap();

        fs::create_dir_all(source.path().join("data/settings")).unwrap();
        fs::write(
            source.path().join("data/settings/global_settings.json"),
            "{ not valid json",
        )
        .unwrap();

        let err = plan_dry_run_migration(source.path(), destination.path()).unwrap_err();
        assert!(matches!(err, MigrationError::InvalidJson { .. }));
    }

    #[test]
    fn execute_migration_does_not_copy_conflicting_keys() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        let endpoints = report.endpoints.as_ref().unwrap();
        assert!(
            endpoints
                .skipped_existing_keys
                .contains(&"appBaseUrl".to_string())
        );
        assert!(
            endpoints
                .conflicting_keys
                .contains(&"apiBaseUrl".to_string())
        );

        let dest_content = fs::read_to_string(destination.path().join("endpoints.json")).unwrap();
        let dest: serde_json::Value = serde_json::from_str(&dest_content).unwrap();
        assert_eq!(
            dest["appBaseUrl"].as_str().unwrap(),
            "https://app.example.com"
        );
        assert_eq!(dest["apiBaseUrl"].as_str().unwrap(), "https://api.dest.com");
    }

    #[test]
    fn execute_migration_copies_global_settings_keys() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        let global = report.global_settings.as_ref().unwrap();
        assert!(global.copied_keys.contains(&"featureFlag".to_string()));
        assert!(global.conflicting_keys.contains(&"mode".to_string()));

        let dest_content = fs::read_to_string(
            destination
                .path()
                .join("data/settings/global_settings.json"),
        )
        .unwrap();
        let dest: serde_json::Value = serde_json::from_str(&dest_content).unwrap();
        assert_eq!(dest["mode"].as_str().unwrap(), "plan");
        assert!(!dest["planActSeparateModelsSetting"].as_bool().unwrap());
        assert!(dest["featureFlag"].as_bool().unwrap());
    }

    #[test]
    fn execute_migration_copies_secrets() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        let secrets = report.secrets.as_ref().unwrap();
        assert!(secrets.copied_keys.contains(&"openAiApiKey".to_string()));
        assert!(secrets.conflicting_keys.contains(&"apiKey".to_string()));

        let dest_content = fs::read_to_string(destination.path().join(".secrets.json")).unwrap();
        let dest: serde_json::Value = serde_json::from_str(&dest_content).unwrap();
        assert_eq!(dest["apiKey"].as_str().unwrap(), "dest-api-key");
        assert_eq!(dest["openAiApiKey"].as_str().unwrap(), "source-openai");
    }

    #[test]
    fn execute_migration_merges_task_history() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        let history = report.task_history.as_ref().unwrap();
        assert!(history.copied_ids.contains(&"task-b".to_string()));
        assert!(history.skipped_existing_ids.contains(&"task-a".to_string()));

        let dest_content =
            fs::read_to_string(destination.path().join("data/state/taskHistory.json")).unwrap();
        let dest: Vec<serde_json::Value> = serde_json::from_str(&dest_content).unwrap();
        assert_eq!(dest.len(), 2);
        let ids: Vec<&str> = dest.iter().filter_map(|v| v.get("id")?.as_str()).collect();
        assert!(ids.contains(&"task-a"));
        assert!(ids.contains(&"task-b"));
    }

    #[test]
    fn execute_migration_copies_task_directories() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        assert_eq!(report.tasks.len(), 1);
        let task = &report.tasks[0];
        assert_eq!(task.task_id, "task-a");
        assert!(
            task.copied_files
                .contains(&PathBuf::from("ui_messages.json"))
        );
        assert!(
            task.conflicting_files
                .contains(&PathBuf::from("settings.json"))
        );

        let settings_path = destination.path().join("data/tasks/task-a/settings.json");
        let settings_content = fs::read_to_string(&settings_path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&settings_content).unwrap();
        assert_eq!(settings["model"].as_str().unwrap(), "claude-3-5-sonnet");

        let ui_path = destination
            .path()
            .join("data/tasks/task-a/ui_messages.json");
        assert!(ui_path.exists());
    }

    #[test]
    fn execute_migration_success_is_true_on_success() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        assert!(report.success);
        assert!(!report.executed_operations.is_empty());
    }

    #[test]
    fn rollback_does_not_error() {
        let (source, destination) = build_source_destination_fixture();

        let mut engine = MigrationEngine::new(source.path(), destination.path());
        let report = engine.execute().unwrap();

        assert!(report.success);

        let rollback_result = engine.rollback();
        assert!(
            rollback_result.is_ok(),
            "Rollback should succeed without error"
        );
    }
}
