//! 原生脚本与固定观察模式分开校验，错误参数不能先创建 Space 或清理标签页。
use crate::ToolError;
use serde_json::{Value, json};

pub(super) fn validate(action: &str, args: &Value) -> Result<(), ToolError> {
    let invalid = |message: &str| ToolError::InvalidArguments(message.into());
    if action != "finish" && (args.get("keep").is_some() || args.get("release").is_some()) {
        return Err(invalid("keep/release are supported only by browser_finish"));
    }
    for name in ["resume_after_user", "release", "new_goal"] {
        if args.get(name).is_some_and(|v| !v.is_boolean()) {
            return Err(invalid(&format!("{name} must be a boolean")));
        }
    }
    if args.get("resume_after_user").is_some() && action != "run" {
        return Err(invalid(
            "Only browser_run may resume after user confirmation",
        ));
    }
    for name in ["new_goal", "observation", "scope", "root", "recovery"] {
        if args.get(name).is_some() && action != "run" {
            return Err(invalid(&format!("{name} is supported only by browser_run")));
        }
    }
    if args.get("recovery").is_some()
        && (args["recovery"] != "reload" || args.get("observation").is_none())
    {
        return Err(invalid(
            "recovery must be reload with a fixed observation; it reloads all managed pages",
        ));
    }
    if args["new_goal"] == true
        && (args.get("observation").is_some() || args["resume_after_user"] == true)
    {
        return Err(invalid(
            "new_goal cannot observe or take over an existing goal",
        ));
    }
    validate_snapshot(args)?;
    if let Some(page) = args.get("page")
        && !valid_label(page)
    {
        return Err(invalid("page must be a nonempty label up to 128 bytes"));
    }
    if action == "run" {
        if let Some(observation) = args.get("observation") {
            if !matches!(observation.as_str(), Some("snapshot" | "screenshot"))
                || args.get("code").is_some()
            {
                return Err(invalid(
                    "observation must be snapshot or screenshot, without code",
                ));
            }
        } else if !args["code"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty() && s.len() <= 65536 && !s.contains('\0'))
        {
            return Err(invalid(
                "code is required unless using observation, and must be a nonempty bounded string",
            ));
        }
    }
    if action == "finish" {
        if args.get("keep").is_some() && args.get("release").is_some() {
            return Err(invalid("Choose keep or release, not both"));
        }
        if let Some(keep) = args.get("keep")
            && keep != "all"
            && !keep
                .as_array()
                .is_some_and(|v| v.len() <= 32 && v.iter().all(valid_label))
        {
            return Err(invalid("keep must be all or up to 32 nonempty page labels"));
        }
    }
    Ok(())
}

fn valid_label(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.len() <= 128 && !s.contains('\0'))
}

fn validate_snapshot(args: &Value) -> Result<(), ToolError> {
    let invalid = |message: &str| ToolError::InvalidArguments(message.into());
    if args.get("scope").is_none() && args.get("root").is_none() {
        return Ok(());
    }
    if args["observation"] != "snapshot" {
        return Err(invalid("scope/root require observation:snapshot"));
    }
    if !matches!(
        args["scope"].as_str(),
        Some("only_within_viewport" | "full_page" | "subtree")
    ) {
        return Err(invalid(
            "scope must be only_within_viewport, full_page or subtree",
        ));
    }
    if args["scope"] == "subtree" {
        if !args["root"].as_str().is_some_and(|root| {
            root.len() <= 32
                && root.strip_prefix('@').is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit())
                })
        }) {
            return Err(invalid("subtree requires a snapshot root ref such as @12"));
        }
        if args.get("recovery").is_some() {
            return Err(invalid(
                "reload invalidates subtree refs; observe viewport or full_page first",
            ));
        }
    } else if args.get("root").is_some() {
        return Err(invalid("root is supported only with subtree"));
    }
    Ok(())
}

pub(super) fn code(args: &Value) -> String {
    match args["observation"].as_str() {
        Some("snapshot") => {
            let mut options = json!({});
            for name in ["scope", "root"] {
                if let Some(value) = args.get(name) {
                    options[name] = value.clone();
                }
            }
            format!("console.log(await page.snapshot({options}));")
        }
        Some("screenshot") => "await browser.screenshot(page);".into(),
        _ => args["code"].as_str().unwrap_or("").into(),
    }
}

pub(super) fn keep(args: &Value) -> Value {
    if args["release"] == true {
        json!([])
    } else {
        args.get("keep").cloned().unwrap_or_else(|| json!("all"))
    }
}
