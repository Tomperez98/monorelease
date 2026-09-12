use std::path::Path;

use mono::OutputMode;

use crate::app::CommandResult;

pub(crate) enum RenderedCommand {
    Summary(String),
    EventsAlreadyEmitted,
}

pub(crate) fn render(result: CommandResult, output: OutputMode) -> RenderedCommand {
    match result {
        CommandResult::Success { kind, message } => {
            RenderedCommand::Summary(success_document(output, kind, message))
        }
        CommandResult::Check { project_root } => {
            RenderedCommand::Summary(check_document(output, &project_root))
        }
        CommandResult::PreRendered(summary) => RenderedCommand::Summary(summary),
        CommandResult::EventsAlreadyEmitted => RenderedCommand::EventsAlreadyEmitted,
    }
}

#[derive(serde::Serialize)]
struct SuccessDocument {
    schema: u32,
    kind: &'static str,
    status: &'static str,
    message: String,
}

#[derive(serde::Serialize)]
struct CheckDocument {
    schema: u32,
    kind: &'static str,
    status: &'static str,
    project: String,
}

fn serialize(document: &impl serde::Serialize) -> String {
    serde_json::to_string(document).expect("output documents contain only serializable fields")
}

fn success_document(output: OutputMode, kind: &'static str, message: String) -> String {
    if output == OutputMode::Json {
        serialize(&SuccessDocument {
            schema: mono::JSON_OUTPUT_SCHEMA,
            kind,
            status: "ok",
            message,
        })
    } else {
        message
    }
}

fn check_document(output: OutputMode, root: &Path) -> String {
    if output == OutputMode::Json {
        serialize(&CheckDocument {
            schema: mono::JSON_OUTPUT_SCHEMA,
            kind: "check",
            status: "ok",
            project: root.display().to_string(),
        })
    } else {
        format!("checked {}", root.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CommandResult;

    #[test]
    fn renders_success_documents_without_exposing_transport_details() {
        let rendered = render(
            CommandResult::Success {
                kind: "cache_clean",
                message: "removed cache".to_owned(),
            },
            OutputMode::Json,
        );
        let RenderedCommand::Summary(document) = rendered else {
            panic!("success must render a summary")
        };
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");
        assert_eq!(value["kind"], "cache_clean");
        assert_eq!(value["status"], "ok");
    }

    #[test]
    fn preserves_the_event_stream_outcome() {
        assert!(matches!(
            render(CommandResult::EventsAlreadyEmitted, OutputMode::Json),
            RenderedCommand::EventsAlreadyEmitted
        ));
    }
}
