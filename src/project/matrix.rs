use std::collections::BTreeMap;

pub(super) fn base_task_name(task: &str) -> Result<&str, ()> {
    match task.find('[') {
        None => Ok(task),
        Some(index) if task.ends_with(']') && index > 0 => Ok(&task[..index]),
        Some(_) => Err(()),
    }
}

pub(super) fn task_dimensions(task: &str) -> Result<BTreeMap<String, String>, ()> {
    let Some(index) = task.find('[') else {
        return Ok(BTreeMap::new());
    };
    if !task.ends_with(']') {
        return Err(());
    }
    let body = &task[index + 1..task.len() - 1];
    if body.is_empty() {
        return Err(());
    }
    let mut dimensions = BTreeMap::new();
    for pair in body.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(());
        };
        if key.is_empty()
            || value.is_empty()
            || dimensions
                .insert(key.to_owned(), value.to_owned())
                .is_some()
        {
            return Err(());
        }
    }
    Ok(dimensions)
}

pub(super) fn validate_matrix_instance(
    matrix: &BTreeMap<String, Vec<String>>,
    dimensions: &BTreeMap<String, String>,
) -> Result<(), String> {
    if matrix.is_empty() {
        return if dimensions.is_empty() {
            Ok(())
        } else {
            Err("task is not matrix-parameterized".to_owned())
        };
    }
    if matrix.len() != dimensions.len() {
        return Err("matrix task references must specify every dimension".to_owned());
    }
    for (key, values) in matrix {
        let Some(value) = dimensions.get(key) else {
            return Err(format!(
                "matrix task reference is missing dimension '{key}'"
            ));
        };
        if !values.contains(value) {
            return Err(format!("matrix dimension '{key}' has no value '{value}'"));
        }
    }
    Ok(())
}

pub(super) fn format_task_instance(base: &str, dimensions: &BTreeMap<String, String>) -> String {
    if dimensions.is_empty() {
        return base.to_owned();
    }
    let values = dimensions
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{base}[{values}]")
}

pub(super) fn matrix_instances(
    matrix: &BTreeMap<String, Vec<String>>,
    fixed: &BTreeMap<String, String>,
) -> Result<Vec<BTreeMap<String, String>>, String> {
    if matrix.is_empty() {
        return Ok(vec![BTreeMap::new()]);
    }
    let mut instances = vec![BTreeMap::new()];
    for (key, values) in matrix {
        let selected = if let Some(value) = fixed.get(key) {
            if !values.contains(value) {
                return Err(format!("matrix dimension '{key}' has no value '{value}'"));
            }
            vec![value.clone()]
        } else {
            values.clone()
        };
        let mut next = Vec::new();
        for instance in instances {
            for value in &selected {
                let mut expanded = instance.clone();
                expanded.insert(key.clone(), value.clone());
                next.push(expanded);
            }
        }
        instances = next;
    }
    Ok(instances)
}

pub(super) fn interpolate_value(
    value: &str,
    dimensions: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let after = &remaining[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unterminated matrix placeholder in `{value}`"));
        };
        let key = &after[..end];
        let replacement = dimensions
            .get(key)
            .ok_or_else(|| format!("unknown matrix placeholder `${{{key}}}` in `{value}`"))?;
        output.push_str(replacement);
        remaining = &after[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}
