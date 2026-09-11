use super::*;
use crate::config::config_path;
use crate::project::suggest::edit_distance;
use crate::project::validate::{valid_relative_path, validate_cache_pattern};
use crate::testing::TempDir;

// --- Task 1: pure helper tests ---

#[test]
fn a_name_without_dimensions_is_its_own_base() {
    assert_eq!(base_task_name("build"), Ok("build"));
}

#[test]
fn a_matrix_instance_base_is_the_name_before_the_bracket() {
    assert_eq!(base_task_name("build[os=linux]"), Ok("build"));
    assert_eq!(base_task_name("build[os=linux,arch=arm]"), Ok("build"));
}

#[test]
fn an_unterminated_or_leading_bracket_is_not_a_task_name() {
    assert_eq!(base_task_name("build[os=linux"), Err(()));
    assert_eq!(base_task_name("[os=linux]"), Err(()));
}

#[test]
fn a_plain_task_name_has_no_dimensions() {
    assert!(task_dimensions("build").unwrap().is_empty());
}

#[test]
fn dimensions_parse_into_a_sorted_map() {
    let dimensions = task_dimensions("build[os=linux,arch=arm]").unwrap();

    assert_eq!(dimensions.get("os").map(String::as_str), Some("linux"));
    assert_eq!(dimensions.get("arch").map(String::as_str), Some("arm"));
    assert_eq!(dimensions.len(), 2);
}

#[test]
fn malformed_dimensions_are_rejected() {
    for reference in [
        "build[",
        "build[]",
        "build[os]",
        "build[os=]",
        "build[=linux]",
        "build[os=linux,os=mac]",
    ] {
        assert_eq!(task_dimensions(reference), Err(()), "{reference}");
    }
}

#[test]
fn an_instance_is_a_format_parse_round_trip() {
    let dimensions = task_dimensions("build[arch=x64,os=linux]").unwrap();

    assert_eq!(
        format_task_instance("build", &dimensions),
        "build[arch=x64,os=linux]"
    );
    assert_eq!(
        task_dimensions(&format_task_instance("build", &dimensions)).unwrap(),
        dimensions
    );
}

#[test]
fn matrix_instances_are_the_cartesian_product_in_key_order() {
    let matrix = BTreeMap::from([
        ("os".to_owned(), vec!["linux".to_owned(), "mac".to_owned()]),
        ("arch".to_owned(), vec!["x64".to_owned()]),
    ]);

    let instances = matrix_instances(&matrix, &BTreeMap::new()).unwrap();

    assert_eq!(
        instances
            .iter()
            .map(|instance| format_task_instance("build", instance))
            .collect::<Vec<_>>(),
        vec!["build[arch=x64,os=linux]", "build[arch=x64,os=mac]"]
    );
}

#[test]
fn matrix_instances_pin_a_fixed_dimension() {
    let matrix = BTreeMap::from([("os".to_owned(), vec!["linux".to_owned(), "mac".to_owned()])]);
    let fixed = BTreeMap::from([("os".to_owned(), "mac".to_owned())]);

    let instances = matrix_instances(&matrix, &fixed).unwrap();

    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["os"], "mac");
}

#[test]
fn a_fixed_matrix_value_must_exist_in_the_dimension() {
    let matrix = BTreeMap::from([("os".to_owned(), vec!["linux".to_owned(), "mac".to_owned()])]);
    let fixed = BTreeMap::from([("os".to_owned(), "windows".to_owned())]);

    assert_eq!(
        matrix_instances(&matrix, &fixed),
        Err("matrix dimension 'os' has no value 'windows'".to_owned())
    );
}

#[test]
fn a_matrix_reference_must_name_every_dimension() {
    let matrix = BTreeMap::from([("os".to_owned(), vec!["linux".to_owned()])]);

    assert_eq!(
        validate_matrix_instance(&matrix, &BTreeMap::new()),
        Err("matrix task references must specify every dimension".to_owned())
    );
}

#[test]
fn a_non_matrix_task_cannot_be_referenced_with_dimensions() {
    let dimensions = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);

    assert_eq!(
        validate_matrix_instance(&BTreeMap::new(), &dimensions),
        Err("task is not matrix-parameterized".to_owned())
    );
}

#[test]
fn placeholders_are_replaced_from_the_instance() {
    let dimensions = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);

    assert_eq!(
        interpolate_value("bin/${os}/app", &dimensions).unwrap(),
        "bin/linux/app"
    );
    assert_eq!(interpolate_value("plain", &dimensions).unwrap(), "plain");
}

#[test]
fn an_unknown_or_unterminated_placeholder_is_rejected() {
    let dimensions = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);

    assert!(interpolate_value("${windows}", &dimensions).is_err());
    assert!(interpolate_value("${os", &dimensions).is_err());
}

#[test]
fn edit_distance_counts_substitutions_insertions_and_deletions() {
    assert_eq!(edit_distance("", ""), 0);
    assert_eq!(edit_distance("abc", "abc"), 0);
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("build", "builds"), 1);
}

#[test]
fn a_close_name_is_suggested_only_within_the_distance_bound() {
    let candidates = ["test".to_owned(), "build".to_owned(), "release".to_owned()];

    assert_eq!(closest_name("tests", &candidates), Some("test".to_owned()));
    assert_eq!(closest_name("test", &candidates), None);
    assert_eq!(closest_name("completely-different", &candidates), None);
}

#[test]
fn relative_paths_reject_parent_and_absolute_components() {
    assert!(valid_relative_path("services/api"));
    assert!(valid_relative_path("./api"));
    assert!(!valid_relative_path(""));
    assert!(!valid_relative_path(".."));
    assert!(!valid_relative_path("../api"));
    assert!(!valid_relative_path("/api"));
}

#[test]
fn cache_patterns_are_relative_and_free_of_empty_segments() {
    assert!(validate_cache_pattern("src/**").is_ok());
    assert!(validate_cache_pattern("!src/**").is_ok());
    assert!(validate_cache_pattern("").is_err());
    assert!(validate_cache_pattern("!").is_err());
    assert!(validate_cache_pattern("/src/**").is_err());
    assert!(validate_cache_pattern("src//app").is_err());
    assert!(validate_cache_pattern("src/../app").is_err());
}

// --- Task 2: Project::load / plan integration tests ---

fn load(manifest: &str) -> (TempDir, Project) {
    let temp = TempDir::new();
    fs::write(config_path(temp.path()), manifest).expect("write manifest");
    let project = Project::load(temp.path()).expect("project loads");
    (temp, project)
}

fn reject(manifest: &str) -> ProjectError {
    let temp = TempDir::new();
    fs::write(config_path(temp.path()), manifest).expect("write manifest");
    Project::load(temp.path()).expect_err("manifest must be rejected")
}

const FINALIZER_MANIFEST: &str = "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n";

#[test]
fn load_orders_dependencies_before_dependents() {
    let (_temp, project) = load(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
    );

    let plan = project.plan(None, &[]).expect("plan succeeds");

    assert_eq!(
        plan.iter().map(PlannedTask::id).collect::<Vec<_>>(),
        vec!["base", "app"]
    );
}

#[test]
fn a_dependency_cycle_is_reported_with_its_path() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\ndepends_on = [\"app\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
    );

    assert!(matches!(error, ProjectError::TaskCycle { .. }), "{error}");
    assert!(error.to_string().contains("app -> base -> app"), "{error}");
}

#[test]
fn an_unknown_default_pipeline_is_rejected() {
    let error = reject(
        "[project]\nname = \"fixture\"\ndefault_pipeline = \"missing\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\n",
    );

    assert!(
        matches!(error, ProjectError::UnknownPipeline { .. }),
        "{error}"
    );
}

#[test]
fn a_pipeline_reference_to_an_unknown_task_suggests_the_closest_name() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"tests\"]\n\n[tasks.test]\ncommand = [\"echo\", \"test\"]\n",
    );

    assert!(
        matches!(
            error,
            ProjectError::MissingTask {
                suggestion: Some(_),
                ..
            }
        ),
        "{error}"
    );
    assert!(
        error.to_string().contains("Did you mean 'test'?"),
        "{error}"
    );
}

#[test]
fn a_matrix_task_expands_to_one_instance_per_combination() {
    let (_temp, project) = load(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"${os}\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
    );

    let plan = project.plan(None, &[]).expect("plan succeeds");

    assert_eq!(
        plan.iter().map(PlannedTask::id).collect::<Vec<_>>(),
        vec!["build[os=linux]", "build[os=mac]"]
    );
    assert_eq!(plan[0].command(), ["echo".to_owned(), "linux".to_owned()]);
}

#[test]
fn an_explicit_matrix_instance_selects_one_combination() {
    let (_temp, project) = load(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build[os=linux]\"]\n\n[tasks.build]\ncommand = [\"echo\", \"${os}\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
    );

    let plan = project.plan(None, &[]).expect("plan succeeds");

    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].id(), "build[os=linux]");
}

#[test]
fn an_explicit_matrix_value_outside_the_dimension_is_rejected() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build[os=windows]\"]\n\n[tasks.build]\ncommand = [\"echo\", \"${os}\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
    );

    assert!(matches!(error, ProjectError::InvalidTask { .. }), "{error}");
}

#[test]
fn a_matrix_dependent_inherits_the_current_dimensions() {
    let (_temp, project) = load(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"package\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nmatrix.os = [\"linux\", \"mac\"]\n\n[tasks.package]\ncommand = [\"echo\", \"package\"]\ndepends_on = [\"build\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
    );

    let plan = project.plan(None, &[]).expect("plan succeeds");
    let package = plan
        .iter()
        .find(|task| task.id() == "package[os=linux]")
        .expect("package instance exists");

    assert_eq!(
        package
            .depends_on()
            .iter()
            .map(TaskNode::id)
            .collect::<Vec<_>>(),
        vec!["build[os=linux]"]
    );
}

#[test]
fn pipeline_finalizers_are_marked_on_the_planned_tasks() {
    let (_temp, project) = load(FINALIZER_MANIFEST);

    let plan = project.plan(None, &[]).expect("plan succeeds");

    assert!(!plan[0].is_finalizer());
    assert!(plan[1].is_finalizer());
}

#[test]
fn a_cacheable_finalizer_is_rejected() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\ncache = true\ninputs = [\"src/**\"]\n",
    );

    assert!(
        error
            .to_string()
            .contains("finalizer tasks cannot be cached"),
        "{error}"
    );
}

#[test]
fn a_cacheable_task_must_declare_a_positive_input_pattern() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"!vendor/**\"]\n",
    );

    assert!(
        error
            .to_string()
            .contains("at least one positive input pattern"),
        "{error}"
    );
}

#[test]
fn a_cwd_escaping_the_project_root_is_rejected() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncwd = \"../escape\"\n",
    );

    assert!(matches!(error, ProjectError::InvalidTask { .. }), "{error}");
}

#[test]
fn a_missing_cwd_directory_is_a_task_directory_failure() {
    let error = reject(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncwd = \"missing\"\n",
    );

    assert!(
        matches!(error, ProjectError::TaskDirectory { ref task, .. } if task == "build"),
        "{error}"
    );
}

#[test]
fn zero_timeout_and_zero_output_limit_are_rejected() {
    for (field, message) in [
        (
            "timeout_seconds = 0",
            "timeout_seconds must be greater than zero",
        ),
        ("max_output_bytes = 0", "max_output_bytes must be positive"),
    ] {
        let error = reject(&format!(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n{field}\n"
        ));
        assert!(error.to_string().contains(message), "{error}");
    }
}

#[test]
fn an_unsupported_schema_is_rejected() {
    let error = reject(
        "schema = 2\n\n[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
    );

    assert!(
        matches!(error, ProjectError::UnsupportedSchema { found: 2, .. }),
        "{error}"
    );
}

#[test]
fn a_matrix_larger_than_the_bound_is_rejected() {
    let values = (0..1025)
        .map(|index| format!("\"v{index}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let error = reject(&format!(
        "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nmatrix.os = [{values}]\n"
    ));

    assert!(
        error.to_string().contains("more than 1024 instances"),
        "{error}"
    );
}

// --- Fault model: which failures wrap a cause ---

#[test]
fn project_errors_expose_a_source_exactly_when_they_wrap_one() {
    let io = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");

    let with_source = [
        ProjectError::Io {
            path: PathBuf::from("mono.toml"),
            source: io(),
        },
        ProjectError::TaskDirectory {
            task: "build".to_owned(),
            path: PathBuf::from("missing"),
            source: io(),
        },
    ];
    for error in &with_source {
        assert!(error.source().is_some(), "{error}");
        assert!(!error.to_string().is_empty(), "{error}");
    }

    let bare = [
        ProjectError::InvalidProject {
            message: "no pipelines".to_owned(),
        },
        ProjectError::UnknownPipeline {
            name: "missing".to_owned(),
            suggestion: Some("ci".to_owned()),
        },
        ProjectError::InvalidTaskName {
            task: "bad[".to_owned(),
        },
        ProjectError::InvalidTask {
            task: "build".to_owned(),
            message: "empty command".to_owned(),
        },
        ProjectError::MissingTask {
            task: "missing".to_owned(),
            suggestion: None,
        },
        ProjectError::InvalidTaskReference {
            reference: "bad[".to_owned(),
            from: TaskNode::new("build"),
        },
        ProjectError::TaskCycle {
            path: vec![TaskNode::new("a"), TaskNode::new("b"), TaskNode::new("a")],
        },
        ProjectError::MissingRoot {
            start: PathBuf::from("."),
        },
    ];
    for error in &bare {
        assert!(error.source().is_none(), "{error}");
        assert!(!error.to_string().is_empty(), "{error}");
    }

    let parse_error = ProjectError::Parse {
        path: PathBuf::from("mono.toml"),
        source: toml::from_str::<crate::config::MonoConfig>("= =").unwrap_err(),
    };
    assert!(parse_error.source().is_some(), "{parse_error}");

    let schema_error = ProjectError::UnsupportedSchema {
        path: PathBuf::from("mono.toml"),
        found: 2,
        supported: 1,
    };
    assert!(schema_error.source().is_none(), "{schema_error}");
}
