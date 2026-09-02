//! 模型常用补丁格式到 `git apply` unified diff 的严格适配。

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use similar::TextDiff;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPatch {
    pub(crate) files: Vec<ModelPatchFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPatchFile {
    pub(crate) path: PathBuf,
    pub(crate) move_path: Option<PathBuf>,
    pub(crate) kind: ModelPatchFileKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelPatchFileKind {
    Update(Vec<ModelPatchHunk>),
    Add {
        lines: Vec<String>,
        no_newline: bool,
    },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPatchHunk {
    pub(crate) header: String,
    pub(crate) context: Option<String>,
    pub(crate) lines: Vec<String>,
    pub(crate) end_of_file: bool,
    pub(crate) new_no_newline: bool,
}

#[must_use]
pub(crate) fn looks_like_model_patch(input: &str) -> bool {
    input.trim_start().starts_with("*** Begin Patch")
}

pub(crate) fn parse(input: &str) -> Result<ModelPatch, String> {
    let mut lines = input
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect::<Vec<_>>();
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return Err("patch must start with *** Begin Patch".to_owned());
    };
    let Some(last) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
        return Err("patch must start with *** Begin Patch".to_owned());
    };
    if first > 0 || last + 1 < lines.len() {
        lines = lines[first..=last].to_vec();
    }
    if lines
        .first()
        .is_some_and(|line| line.trim() == "*** Begin Patch")
    {
        lines[0] = "*** Begin Patch".to_owned();
    }
    if lines.first().map(String::as_str) != Some("*** Begin Patch") {
        return Err("patch must start with *** Begin Patch".to_owned());
    }

    let mut files = Vec::new();
    let mut seen_path_identities = BTreeSet::new();
    let mut index = 1_usize;
    let mut ended = false;
    while index < lines.len() {
        let line = &lines[index];
        if is_end_marker(line) {
            ended = true;
            index = index.saturating_add(1);
            break;
        }
        if line.trim().is_empty() {
            index = index.saturating_add(1);
            continue;
        }
        if line.starts_with("*** Environment ID:") {
            index = index.saturating_add(1);
            continue;
        }

        let (kind, path) = if let Some(path) = line.strip_prefix("*** Update File:") {
            ("update", parse_path(path)?)
        } else if let Some(path) = line.strip_prefix("*** Add File:") {
            ("add", parse_path(path)?)
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            ("delete", parse_path(path)?)
        } else {
            return Err(format!("unsupported patch control line: {line}"));
        };
        if !seen_path_identities.insert(lexical_path_identity(&path)) {
            return Err(format!(
                "patch names the file more than once: {}",
                path.display()
            ));
        }
        index = index.saturating_add(1);

        let (move_path, file_kind) = match kind {
            "update" => {
                let move_path = if lines
                    .get(index)
                    .is_some_and(|line| line.starts_with("*** Move to:"))
                {
                    let move_path = parse_path(
                        lines[index]
                            .strip_prefix("*** Move to:")
                            .expect("move marker was checked"),
                    )?;
                    index = index.saturating_add(1);
                    Some(move_path)
                } else {
                    None
                };
                let hunks = parse_update_hunks(&lines, &mut index)?;
                if let Some(move_path) = &move_path
                    && !seen_path_identities.insert(lexical_path_identity(move_path))
                {
                    return Err(format!(
                        "patch names a source or destination more than once: {}",
                        move_path.display()
                    ));
                }
                (move_path, ModelPatchFileKind::Update(hunks))
            }
            "add" => {
                let (lines, no_newline) = parse_add_lines(&lines, &mut index)?;
                (None, ModelPatchFileKind::Add { lines, no_newline })
            }
            "delete" => {
                if lines
                    .get(index)
                    .is_some_and(|next| !is_file_header(next) && !is_end_marker(next))
                {
                    return Err("delete file entry cannot contain patch body".to_owned());
                }
                (None, ModelPatchFileKind::Delete)
            }
            _ => unreachable!("patch kind is selected above"),
        };
        files.push(ModelPatchFile {
            path,
            move_path,
            kind: file_kind,
        });
    }

    if !ended {
        return Err("patch is missing *** End Patch".to_owned());
    }
    if lines[index..].iter().any(|line| !line.is_empty()) {
        return Err("patch contains data after *** End Patch".to_owned());
    }
    if files.is_empty() {
        return Err("patch does not contain any file entries".to_owned());
    }
    Ok(ModelPatch { files })
}

fn parse_update_hunks(lines: &[String], index: &mut usize) -> Result<Vec<ModelPatchHunk>, String> {
    let mut hunks = Vec::new();
    while let Some(line) = lines.get(*index) {
        if line.trim().is_empty() {
            *index = index.saturating_add(1);
            continue;
        }
        if is_file_header(line) || is_end_marker(line) {
            break;
        }
        let (header, context) = if line.starts_with("@@") {
            let header = line.clone();
            *index = index.saturating_add(1);
            let context = hunk_context(&header);
            (header, context)
        } else if is_hunk_line(line) {
            // 允许模型省略首个 @@，直接从上下文或变更行开始。
            ("@@".to_owned(), None)
        } else {
            return Err(format!("update entry requires a hunk header, got: {line}"));
        };
        let mut body = Vec::new();
        let mut end_of_file = false;
        let mut new_no_newline = false;
        while let Some(line) = lines.get(*index) {
            if line.starts_with("@@") || is_file_header(line) || is_end_marker(line) {
                break;
            }
            if is_end_of_file_marker(line) {
                // EOF 是定位锚点，不代表文件末尾换行符。
                end_of_file = true;
                *index = index.saturating_add(1);
                break;
            } else if is_no_newline_marker(line) {
                new_no_newline = body
                    .iter()
                    .rev()
                    .find_map(|line: &String| line.as_bytes().first().copied())
                    .is_some_and(|kind| kind != b'-');
                *index = index.saturating_add(1);
                continue;
            } else if line.is_empty() {
                // 部分客户端省略空上下文行的前导空格；只在 hunk 内将其视作上下文。
                body.push(" ".to_owned());
            } else if matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-'))
                || is_no_newline_marker(line)
            {
                body.push(line.clone());
            } else {
                return Err(format!("invalid model patch hunk line: {line}"));
            }
            *index = index.saturating_add(1);
        }
        validate_hunk(&header, &body)?;
        hunks.push(ModelPatchHunk {
            header,
            context,
            lines: body,
            end_of_file,
            new_no_newline,
        });
    }
    if hunks.is_empty() {
        return Err("update entry does not contain a hunk".to_owned());
    }
    Ok(hunks)
}

fn parse_add_lines(lines: &[String], index: &mut usize) -> Result<(Vec<String>, bool), String> {
    let mut content = Vec::new();
    let mut no_newline = false;
    while let Some(line) = lines.get(*index) {
        if is_file_header(line) || is_end_marker(line) {
            break;
        }
        if is_end_of_file_marker(line) {
            // Add File 的 EOF 标记只表示定位结束，不改变文件末尾换行语义。
        } else if is_no_newline_marker(line) {
            no_newline = true;
        } else if let Some(value) = line.strip_prefix('+') {
            content.push(value.to_owned());
        } else {
            return Err(format!("added file content must start with '+': {line}"));
        }
        *index = index.saturating_add(1);
    }
    Ok((content, no_newline))
}

fn validate_hunk(header: &str, lines: &[String]) -> Result<(), String> {
    if header != "@@" && !header.starts_with("@@ ") && !header.starts_with("@@-") {
        return Err(format!("invalid model patch hunk header: {header}"));
    }
    for line in lines {
        if is_no_newline_marker(line) {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+' | b'-' | b' ') => {}
            _ => return Err(format!("invalid model patch hunk line: {line}")),
        }
    }
    Ok(())
}

fn is_hunk_line(line: &str) -> bool {
    line.is_empty()
        || matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-'))
        || is_no_newline_marker(line)
}

fn is_end_of_file_marker(line: &str) -> bool {
    line.trim() == "*** End of File"
}

fn is_no_newline_marker(line: &str) -> bool {
    line.trim() == "\\ No newline at end of file"
}

/// 只做词法路径归一，用于补丁内部的身份比较；实际文件访问仍交给 workspace
/// policy 解析，避免把 symlink 的运行时语义提前改写。
fn lexical_path_identity(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let mut has_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                has_root |= matches!(component, Component::RootDir);
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop_normal = normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if can_pop_normal {
                    normalized.pop();
                } else if !has_root {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn hunk_context(header: &str) -> Option<String> {
    let body = header.strip_prefix("@@")?.trim();
    if body.is_empty() {
        return None;
    }
    // 标准 unified header 的第二个 @@ 之后才是上下文锚点；模型格式使用整个后缀。
    if let Some((range, suffix)) = body.split_once("@@")
        && range
            .split_whitespace()
            .all(|token| token.starts_with(['-', '+']) && token.len() > 1)
    {
        return (!suffix.trim().is_empty()).then(|| suffix.trim().to_owned());
    }
    (!body.starts_with(['-', '+']) || body.contains(char::is_whitespace)).then(|| body.to_owned())
}

fn parse_path(path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    if path.is_empty() || path.contains('\0') || path.contains(['\n', '\r']) {
        return Err("patch file path is empty or contains a control character".to_owned());
    }
    Ok(PathBuf::from(path))
}

fn is_file_header(line: &str) -> bool {
    line.starts_with("*** Update File:")
        || line.starts_with("*** Add File:")
        || line.starts_with("*** Delete File:")
}

fn is_end_marker(line: &str) -> bool {
    line.trim() == "*** End Patch"
}

pub(crate) fn render(
    patch: &ModelPatch,
    originals: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
) -> Result<String, String> {
    let mut output = String::new();
    let original_identities = originals
        .keys()
        .map(|path| lexical_path_identity(path))
        .collect::<BTreeSet<_>>();
    for file in &patch.files {
        match &file.kind {
            ModelPatchFileKind::Update(hunks) => {
                let original = originals.get(&file.path).ok_or_else(|| {
                    format!("update target does not exist: {}", file.path.display())
                })?;
                let original = std::str::from_utf8(original).map_err(|_| {
                    format!("update target is not valid UTF-8: {}", file.path.display())
                })?;
                let edited = apply_hunks(original, hunks)?;
                if let Some(move_path) = &file.move_path {
                    if lexical_path_identity(move_path) == lexical_path_identity(&file.path) {
                        return Err("move destination must differ from the source".to_owned());
                    }
                    if original_identities.contains(&lexical_path_identity(move_path)) {
                        return Err(format!(
                            "move destination already exists: {}",
                            move_path.display()
                        ));
                    }
                    append_diff(&mut output, &file.path, original, "", true, false)?;
                    append_diff(&mut output, move_path, "", &edited, false, true)?;
                } else {
                    append_diff(&mut output, &file.path, original, &edited, true, true)?;
                }
            }
            ModelPatchFileKind::Add { lines, no_newline } => {
                if original_identities.contains(&lexical_path_identity(&file.path)) {
                    return Err(format!(
                        "add target already exists: {}",
                        file.path.display()
                    ));
                }
                let edited = join_lines(lines, !no_newline);
                append_diff(&mut output, &file.path, "", &edited, false, true)?;
            }
            ModelPatchFileKind::Delete => {
                let original = originals.get(&file.path).ok_or_else(|| {
                    format!("delete target does not exist: {}", file.path.display())
                })?;
                let original = std::str::from_utf8(original).map_err(|_| {
                    format!("delete target is not valid UTF-8: {}", file.path.display())
                })?;
                append_diff(&mut output, &file.path, original, "", true, false)?;
            }
        }
    }
    if output.is_empty() {
        return Err("patch does not change any file".to_owned());
    }
    Ok(output)
}

fn apply_hunks(original: &str, hunks: &[ModelPatchHunk]) -> Result<String, String> {
    let (original_lines, original_newline) = split_lines(original);
    let mut output_lines = Vec::new();
    let mut cursor = 0_usize;
    let mut output_newline = original_newline;

    for hunk in hunks {
        let (old_lines, new_lines, new_no_newline) = hunk_changes(hunk)?;
        let hint = hunk_old_start(&hunk.header);
        let context_start = if let Some(context) = &hunk.context {
            let context_start = locate_context(&original_lines, context, cursor, hunk.end_of_file)?;
            context_start.saturating_add(1)
        } else {
            cursor
        };
        let start = if old_lines.is_empty() {
            insertion_point(
                original_lines.len(),
                context_start,
                cursor,
                hint,
                hunk.end_of_file,
            )?
        } else {
            locate_hunk(
                &original_lines,
                &old_lines,
                context_start,
                hint,
                hunk.end_of_file,
            )?
        };
        output_lines.extend_from_slice(&original_lines[cursor..start]);
        output_lines.extend(new_lines);
        cursor = start.saturating_add(old_lines.len());
        if new_no_newline {
            output_newline = false;
        }
    }
    output_lines.extend_from_slice(&original_lines[cursor..]);
    Ok(join_lines(&output_lines, output_newline))
}

fn hunk_changes(hunk: &ModelPatchHunk) -> Result<(Vec<String>, Vec<String>, bool), String> {
    let mut old_lines = Vec::new();
    let mut new_lines = Vec::new();
    let mut previous_kind = None;
    let mut new_no_newline = hunk.new_no_newline;
    for line in &hunk.lines {
        if line == "\\ No newline at end of file" {
            if previous_kind != Some(b'-') {
                new_no_newline = true;
            }
            continue;
        }
        let bytes = line.as_bytes();
        let kind = bytes
            .first()
            .copied()
            .ok_or_else(|| "empty hunk line".to_owned())?;
        let value = line
            .get(1..)
            .ok_or_else(|| "hunk line is not valid UTF-8".to_owned())?
            .to_owned();
        match kind {
            b' ' => {
                old_lines.push(value.clone());
                new_lines.push(value);
            }
            b'-' => old_lines.push(value),
            b'+' => new_lines.push(value),
            _ => return Err(format!("invalid hunk line: {line}")),
        }
        previous_kind = Some(kind);
    }
    Ok((old_lines, new_lines, new_no_newline))
}

fn locate_hunk(
    original: &[String],
    needle: &[String],
    cursor: usize,
    hint: Option<usize>,
    end_of_file: bool,
) -> Result<usize, String> {
    if needle.is_empty() {
        let start = hint.map_or(cursor, |line| line.saturating_sub(1));
        if start < cursor || start > original.len() {
            return Err("model patch insertion point is outside the file".to_owned());
        }
        return Ok(start);
    }
    let hinted = hint.map(|line| line.saturating_sub(1));
    if let Some(start) = hinted.filter(|start| {
        *start >= cursor
            && start.saturating_add(needle.len()) <= original.len()
            && (!end_of_file || start.saturating_add(needle.len()) == original.len())
            && original[*start..start + needle.len()] == *needle
    }) {
        return Ok(start);
    }

    let mut matches = Vec::new();
    for start in cursor..=original.len().saturating_sub(needle.len()) {
        if end_of_file && start.saturating_add(needle.len()) != original.len() {
            continue;
        }
        if original[start..start + needle.len()] == *needle {
            matches.push(start);
            if matches.len() > 1 {
                break;
            }
        }
    }
    match matches.as_slice() {
        [start] => Ok(*start),
        [] => Err("model patch context does not match the current file".to_owned()),
        _ => {
            Err("model patch context is ambiguous; include more context or line numbers".to_owned())
        }
    }
}

fn locate_context(
    original: &[String],
    context: &str,
    cursor: usize,
    end_of_file: bool,
) -> Result<usize, String> {
    let mut matches = Vec::new();
    for index in cursor..original.len() {
        if end_of_file && index + 1 != original.len() {
            continue;
        }
        if original[index] == context
            || original[index].trim() == context.trim()
            || normalize_context(&original[index]) == normalize_context(context)
        {
            matches.push(index);
            if matches.len() > 1 {
                break;
            }
        }
    }
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => Err(format!("model patch context does not match: {context}")),
        _ => Err(format!("model patch context is ambiguous: {context}")),
    }
}

fn normalize_context(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn insertion_point(
    file_len: usize,
    context_start: usize,
    cursor: usize,
    hint: Option<usize>,
    end_of_file: bool,
) -> Result<usize, String> {
    if end_of_file {
        return Ok(file_len);
    }
    if context_start > cursor {
        return Ok(context_start.min(file_len));
    }
    if let Some(line) = hint {
        let start = line.saturating_sub(1);
        if start >= cursor && start <= file_len {
            return Ok(start);
        }
        return Err("model patch insertion point is outside the file".to_owned());
    }
    Ok(file_len.max(cursor))
}

fn hunk_old_start(header: &str) -> Option<usize> {
    let range = header.strip_prefix("@@")?.split("@@").next()?;
    range.split_whitespace().find_map(|token| {
        let value = token.strip_prefix('-')?.split(',').next()?;
        value.parse::<usize>().ok()
    })
}

fn split_lines(value: &str) -> (Vec<String>, bool) {
    let final_newline = value.ends_with('\n');
    let mut lines = value.split('\n').map(str::to_owned).collect::<Vec<_>>();
    if final_newline {
        lines.pop();
    }
    (lines, final_newline)
}

fn join_lines(lines: &[String], final_newline: bool) -> String {
    let mut value = lines.join("\n");
    if final_newline && !value.is_empty() {
        value.push('\n');
    }
    value
}

fn append_diff(
    output: &mut String,
    path: &Path,
    original: &str,
    edited: &str,
    original_exists: bool,
    edited_exists: bool,
) -> Result<(), String> {
    let path = path.to_string_lossy();
    let old_name = if original_exists {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let new_name = if edited_exists {
        format!("b/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let diff = TextDiff::from_lines(original, edited);
    let body = diff
        .unified_diff()
        .context_radius(3)
        .header(&old_name, &new_name)
        .to_string();
    if body.is_empty() && original_exists == edited_exists {
        return Err(format!("patch does not change file: {path}"));
    }
    output.push_str(&format!("diff --git a/{path} b/{path}\n"));
    if !original_exists {
        output.push_str("new file mode 100644\n");
    } else if !edited_exists {
        output.push_str("deleted file mode 100644\n");
    }
    if body.is_empty() {
        output.push_str(&format!("--- {old_name}\n+++ {new_name}\n"));
    } else {
        output.push_str(&body);
    }
    Ok(())
}
