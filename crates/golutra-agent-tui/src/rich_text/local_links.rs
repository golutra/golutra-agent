use std::path::Path;

pub(super) fn display_target(destination: &str, cwd: Option<&Path>) -> Option<String> {
    if !(destination.starts_with('/')
        || destination.starts_with("./")
        || destination.starts_with("../")
        || destination.starts_with("~/"))
    {
        return None;
    }
    let normalized = if let Some((path, line)) = destination.rsplit_once("#L") {
        if !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()) {
            format!("{path}:{line}")
        } else {
            destination.to_owned()
        }
    } else {
        destination.to_owned()
    };
    Some(
        cwd.and_then(|cwd| Path::new(&normalized).strip_prefix(cwd).ok())
            .filter(|path| !path.as_os_str().is_empty())
            .map_or(normalized.clone(), |path| {
                path.to_string_lossy().into_owned()
            }),
    )
}

pub(super) fn redundant_label(label: &str, destination: &str) -> bool {
    let without_location = |text: &str| {
        let text = text.split("#L").next().unwrap_or(text);
        let end = text
            .rsplit_once(':')
            .filter(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
            .map_or(text.len(), |(path, _)| path.len());
        text[..end].trim_start_matches("./").to_owned()
    };
    let label = without_location(label.trim());
    let target = without_location(destination);
    !label.is_empty()
        && target
            .strip_suffix(&label)
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_labels_collapse_without_losing_location_or_description() {
        assert_eq!(
            display_target("/work/src/main.rs#L12", Some(Path::new("/work"))),
            Some("src/main.rs:12".into())
        );
        assert_eq!(
            display_target("/work-other/main.rs:2", Some(Path::new("/work"))),
            Some("/work-other/main.rs:2".into())
        );
        assert!(redundant_label("main.rs", "/work/src/main.rs:12"));
        assert!(!redundant_label("the entry point", "/work/src/main.rs:12"));
        assert!(!redundant_label("ain.rs", "/work/src/main.rs:12"));
        assert_eq!(display_target("https://example.com", None), None);
    }
}
