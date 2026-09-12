use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use crate::config::{StdinMode, TaskConfig, validate_process_value};
use crate::project::ProjectError;

pub(crate) fn validate_schema(path: &Path, schema: u32) -> Result<(), ProjectError> {
    if schema != crate::config::SUPPORTED_SCHEMA {
        return Err(ProjectError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: schema,
            supported: crate::config::SUPPORTED_SCHEMA,
        });
    }
    Ok(())
}

pub(super) fn validate_task_config(
    manifest_path: &Path,
    task_name: &str,
    task: &TaskConfig,
) -> Result<(), ProjectError> {
    validate_identifier(manifest_path, "task name", task_name)?;
    if task_name.contains(['[', ']', '=', ',']) {
        return Err(ProjectError::InvalidTaskName {
            task: task_name.to_owned(),
        });
    }
    if task.command.is_empty() || task.command[0].is_empty() {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "command must contain an executable".to_owned(),
        });
    }
    for (index, argument) in task.command.iter().enumerate() {
        validate_process_value(argument, &format!("command argument {index}")).map_err(
            |message| ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message,
            },
        )?;
    }
    for (key, value) in &task.env {
        validate_process_value(key, "environment key")
            .and_then(|_| validate_process_value(value, "environment value"))
            .map_err(|message| ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message,
            })?;
    }
    if task.cache
        && (task.inputs.is_empty() || !task.inputs.iter().any(|pattern| !pattern.starts_with('!')))
    {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "cacheable tasks must declare at least one positive input pattern".to_owned(),
        });
    }
    if task.cache && task.stdin == StdinMode::Inherit {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "cacheable tasks cannot inherit standard input".to_owned(),
        });
    }
    if !task.outputs.is_empty() && !task.outputs.iter().any(|pattern| !pattern.starts_with('!')) {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "outputs must declare at least one positive pattern".to_owned(),
        });
    }
    for pattern in task.inputs.iter().chain(&task.outputs) {
        validate_cache_pattern(pattern).map_err(|message| ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message,
        })?;
    }
    for variable in &task.cache_env {
        if variable.is_empty() || variable.contains('=') || variable.contains('\0') {
            return Err(ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message: "cache_env names must be non-empty and cannot contain '=' or NUL"
                    .to_owned(),
            });
        }
    }
    if task.timeout_seconds == 0 {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "timeout_seconds must be greater than zero".to_owned(),
        });
    }
    if task.max_output_bytes == 0 || usize::try_from(task.max_output_bytes).is_err() {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "max_output_bytes must be positive and fit in platform usize".to_owned(),
        });
    }
    if task.retries > 0 && task.retry_backoff_seconds > 86_400 {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "retry_backoff_seconds must not exceed 86400".to_owned(),
        });
    }
    if let Some(group) = &task.resource_group
        && (group.is_empty()
            || group.contains(':')
            || group.contains('\0')
            || group.chars().any(char::is_control))
    {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "resource_group must be non-empty and cannot contain ':', NUL, or control characters".to_owned(),
        });
    }
    if let Some(cwd) = &task.cwd
        && !valid_relative_path(cwd)
    {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "cwd must be an existing relative directory without '..'".to_owned(),
        });
    }
    for (dimension, values) in &task.matrix {
        validate_identifier(manifest_path, "matrix dimension", dimension)?;
        if values.is_empty() {
            return Err(ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message: format!("matrix dimension '{dimension}' must have at least one value"),
            });
        }
        let mut seen = BTreeSet::new();
        for value in values {
            validate_process_value(value, "matrix value")
                .and_then(|_| {
                    if value.contains(['[', ']', '=', ',']) {
                        Err("matrix values cannot contain '[', ']', '=', or ','".to_owned())
                    } else if value.chars().any(char::is_control) {
                        Err("matrix values cannot contain control characters".to_owned())
                    } else {
                        Ok(())
                    }
                })
                .map_err(|message| ProjectError::InvalidTask {
                    task: task_name.to_owned(),
                    message,
                })?;
            if !seen.insert(value) {
                return Err(ProjectError::InvalidTask {
                    task: task_name.to_owned(),
                    message: format!(
                        "matrix dimension '{dimension}' contains duplicate value '{value}'"
                    ),
                });
            }
        }
    }
    let matrix_size = task
        .matrix
        .values()
        .try_fold(1usize, |size, values| size.checked_mul(values.len()))
        .ok_or_else(|| ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "matrix has too many instances".to_owned(),
        })?;
    if matrix_size > 1024 {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "matrix cannot expand to more than 1024 instances".to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_task_directory(
    root: &Path,
    task_name: &str,
    task: &TaskConfig,
) -> Result<(), ProjectError> {
    let Some(cwd) = &task.cwd else {
        return Ok(());
    };
    if cwd.contains("${") {
        return Ok(());
    }

    let cwd_path = root.join(cwd);
    let canonical = fs::canonicalize(&cwd_path).map_err(|source| ProjectError::TaskDirectory {
        task: task_name.to_owned(),
        path: cwd_path.clone(),
        source,
    })?;
    if !canonical.starts_with(root) || !canonical.is_dir() {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "cwd must resolve to a directory inside the project root".to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_identifier(
    path: &Path,
    label: &str,
    value: &str,
) -> Result<(), ProjectError> {
    if value.is_empty() || value.contains('\0') || value.chars().any(char::is_control) {
        return Err(ProjectError::InvalidManifest {
            path: path.to_path_buf(),
            message: format!(
                "{label} must be non-empty and cannot contain NUL or control characters"
            ),
        });
    }
    Ok(())
}

pub(super) fn validate_task_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty() || reference.contains('\0') || reference.chars().any(char::is_control) {
        Err(
            "task reference must be non-empty and cannot contain NUL or control characters"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

pub(super) fn valid_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

pub(super) fn validate_cache_pattern(pattern: &str) -> Result<(), String> {
    let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
    if pattern.is_empty() || pattern.contains('\0') || Path::new(pattern).is_absolute() {
        return Err("cache patterns must be non-empty and relative".to_owned());
    }
    if pattern
        .split('/')
        .any(|segment| segment.is_empty() || segment == "..")
    {
        return Err("cache patterns cannot contain empty or '..' path segments".to_owned());
    }
    Ok(())
}
