//! 模型只能选择受支持的浏览器动作；会话、Profile、程序路径不接受网页或模型覆写。
use crate::ToolError;
use serde_json::Value;

fn text(args: &Value, key: &str) -> Result<String, ToolError> {
    args[key]
        .as_str()
        .filter(|v| v.len() <= 32768 && !v.contains('\0'))
        .map(str::to_owned)
        .ok_or_else(|| ToolError::InvalidArguments(format!("{key} must be a bounded string")))
}

pub(super) fn arguments(args: &Value) -> Result<Vec<String>, ToolError> {
    let action = text(args, "action")?;
    if args.get("frame_index").is_some() && action != "evaluate" {
        return Err(ToolError::InvalidArguments("frame_index is supported only by evaluate; use observed snapshot refs for other actions".into()));
    }
    let mut options = vec![];
    if let Some(nth) = args.get("nth") {
        if !matches!(action.as_str(), "click" | "hover") {
            return Err(ToolError::InvalidArguments(
                "nth is supported only by click/hover".into(),
            ));
        }
        let nth = nth.as_u64().filter(|n| *n <= 10000).ok_or_else(|| {
            ToolError::InvalidArguments("nth must be an observed index within 0..10000".into())
        })?;
        options.extend(["--nth".into(), nth.to_string()]);
    }
    let (command, values): (&str, Vec<String>) = match action.as_str() {
        "open" | "navigate" => {
            let url = text(args, "url")?;
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return Err(ToolError::InvalidArguments(
                    "Only HTTP/HTTPS task URLs are supported".into(),
                ));
            }
            ("open", vec![url])
        }
        "snapshot" => {
            options.push("--diagnostics".into());
            ("state", vec![])
        }
        "frames" | "screenshot" | "back" => (action.as_str(), vec![]),
        "click" | "hover" => (action.as_str(), vec![text(args, "selector")?]),
        "fill" | "type" => (
            action.as_str(),
            vec![text(args, "selector")?, text(args, "text")?],
        ),
        "select" => ("select", vec![text(args, "selector")?, text(args, "text")?]),
        "press" => ("keys", vec![text(args, "key")?]),
        "evaluate" => {
            if let Some(frame) = args.get("frame_index") {
                let index = frame.as_u64().filter(|v| *v < 1024).ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "frame_index must be an observed nonnegative index".into(),
                    )
                })?;
                options.extend(["--frame".into(), index.to_string()]);
            }
            ("eval", vec![text(args, "expression")?])
        }
        "scroll" => {
            let y = args["y"]
                .as_i64()
                .filter(|v| v.unsigned_abs() <= 100000)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("y must be an integer within +/-100000".into())
                })?;
            options.extend(["--amount".into(), y.unsigned_abs().to_string()]);
            ("scroll", vec![if y < 0 { "up" } else { "down" }.into()])
        }
        "wait" => {
            let kind = text(args, "kind")?;
            if !["selector", "text", "download"].contains(&kind.as_str()) {
                return Err(ToolError::InvalidArguments(
                    "wait kind: selector, text or download".into(),
                ));
            }
            ("wait", vec![kind, text(args, "text")?])
        }
        "upload" => {
            let mut values = vec![text(args, "selector")?];
            let files = args["files"]
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 8)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("files must contain 1..8 absolute paths".into())
                })?;
            for file in files {
                let path = file
                    .as_str()
                    .filter(|v| std::path::Path::new(v).is_absolute() && !v.contains('\0'))
                    .ok_or_else(|| {
                        ToolError::InvalidArguments("upload requires absolute file paths".into())
                    })?;
                values.push(path.into());
            }
            ("upload", values)
        }
        _ => {
            return Err(ToolError::InvalidArguments(format!(
                "Unsupported OpenCLI action: {action}"
            )));
        }
    };
    let mut result = vec![command.into()];
    result.extend(options);
    // -- 阻止文本/CSS/JS 中的 --profile/--session 被解析成路由选项。
    if !values.is_empty() {
        result.push("--".into());
        result.extend(values);
    }
    Ok(result)
}
