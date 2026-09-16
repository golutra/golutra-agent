use std::path::Path;

use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use crate::{
    CodeEdge, CodeEdgeKind, CodeLanguage, CodeReference, CodeSymbol, SourceLocation, SymbolKind,
};

pub(crate) struct ParsedFile {
    pub symbols: Vec<CodeSymbol>,
    pub references: Vec<CodeReference>,
    pub dependencies: Vec<String>,
}

pub(crate) fn parse_source(
    relative_path: &Path,
    source: &str,
) -> Result<Option<ParsedFile>, tree_sitter::LanguageError> {
    let Some(language) = language_for_path(relative_path) else {
        return Ok(None);
    };
    let mut parser = Parser::new();
    match language {
        CodeLanguage::Rust => parser.set_language(&tree_sitter_rust::LANGUAGE.into())?,
        CodeLanguage::Python => parser.set_language(&tree_sitter_python::LANGUAGE.into())?,
        CodeLanguage::TypeScript => {
            parser.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())?
        }
        CodeLanguage::JavaScript => {
            parser.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())?
        }
    }
    let Some(tree) = parser.parse(source, None) else {
        return Ok(None);
    };
    let path = relative_path.to_string_lossy().replace('\\', "/");
    let nodes = descendants(tree.root_node());
    let mut symbols = nodes
        .iter()
        .filter_map(|node| symbol_from_node(*node, language, &path, source))
        .collect::<Vec<_>>();
    symbols.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.location.start_line.cmp(&right.location.start_line))
            .then(left.name.cmp(&right.name))
    });
    let definition_ranges = symbols
        .iter()
        .map(|symbol| {
            (
                symbol.location.start_line,
                symbol.location.start_column,
                symbol.name.clone(),
            )
        })
        .collect::<Vec<_>>();
    let symbol_names = symbols
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let references = nodes
        .iter()
        .filter(|node| is_identifier(node.kind()))
        .filter_map(|node| {
            let name = node.utf8_text(source.as_bytes()).ok()?;
            if !symbol_names.contains(name) {
                return None;
            }
            let location = source_location(*node);
            if definition_ranges.iter().any(|(line, column, definition)| {
                *line == location.start_line
                    && *column == location.start_column
                    && definition == name
            }) {
                return None;
            }
            Some(CodeReference {
                symbol_name: name.to_owned(),
                path: path.clone(),
                location,
                context: source_line(source, node.start_position().row),
            })
        })
        .collect();
    let dependencies = nodes
        .iter()
        .filter(|node| is_dependency_node(language, node.kind()))
        .filter_map(|node| node.utf8_text(source.as_bytes()).ok())
        .map(compact_text)
        .collect();
    Ok(Some(ParsedFile {
        symbols,
        references,
        dependencies,
    }))
}

pub(crate) fn graph_edges(
    symbols: &[CodeSymbol],
    references: &[CodeReference],
    dependencies: &[(String, String)],
) -> Vec<CodeEdge> {
    let mut edges = symbols
        .iter()
        .map(|symbol| CodeEdge {
            from: symbol.path.clone(),
            to: symbol.symbol_id.clone(),
            kind: CodeEdgeKind::Contains,
        })
        .collect::<Vec<_>>();
    for reference in references {
        if let Some(target) = symbols
            .iter()
            .find(|symbol| symbol.path == reference.path && symbol.name == reference.symbol_name)
            .or_else(|| {
                symbols
                    .iter()
                    .find(|symbol| symbol.name == reference.symbol_name)
            })
        {
            edges.push(CodeEdge {
                from: reference.path.clone(),
                to: target.symbol_id.clone(),
                kind: CodeEdgeKind::References,
            });
        }
    }
    edges.extend(dependencies.iter().map(|(path, dependency)| CodeEdge {
        from: path.clone(),
        to: dependency.clone(),
        kind: CodeEdgeKind::Imports,
    }));
    edges.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then(left.to.cmp(&right.to))
            .then((left.kind as u8).cmp(&(right.kind as u8)))
    });
    edges.dedup();
    edges
}

fn language_for_path(path: &Path) -> Option<CodeLanguage> {
    match path.extension().and_then(|extension| extension.to_str())? {
        "rs" => Some(CodeLanguage::Rust),
        "ts" | "tsx" => Some(CodeLanguage::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(CodeLanguage::JavaScript),
        "py" | "pyi" => Some(CodeLanguage::Python),
        _ => None,
    }
}

fn descendants(root: Node<'_>) -> Vec<Node<'_>> {
    let mut nodes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        nodes.push(node);
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    nodes
}

fn symbol_from_node(
    node: Node<'_>,
    language: CodeLanguage,
    path: &str,
    source: &str,
) -> Option<CodeSymbol> {
    let kind = symbol_kind(language, node.kind())?;
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("declarator"))?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.trim();
    if name.is_empty() || name.len() > 256 {
        return None;
    }
    let location = source_location(name_node);
    let signature = compact_text(
        node.utf8_text(source.as_bytes())
            .ok()?
            .lines()
            .next()
            .unwrap_or_default(),
    );
    let container = containing_symbol(node, source);
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(location.start_line.to_be_bytes());
    hasher.update(location.start_column.to_be_bytes());
    hasher.update(name.as_bytes());
    Some(CodeSymbol {
        symbol_id: format!("sha256:{:x}", hasher.finalize()),
        name: name.to_owned(),
        kind,
        language,
        path: path.to_owned(),
        location,
        container,
        signature: signature.chars().take(320).collect(),
    })
}

fn containing_symbol(node: Node<'_>, source: &str) -> Option<String> {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if symbol_kind(CodeLanguage::Rust, candidate.kind()).is_some()
            || symbol_kind(CodeLanguage::TypeScript, candidate.kind()).is_some()
            || symbol_kind(CodeLanguage::Python, candidate.kind()).is_some()
        {
            return candidate
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                .map(ToOwned::to_owned);
        }
        parent = candidate.parent();
    }
    None
}

fn symbol_kind(language: CodeLanguage, kind: &str) -> Option<SymbolKind> {
    match (language, kind) {
        (CodeLanguage::Rust, "function_item") => Some(SymbolKind::Function),
        (CodeLanguage::Rust, "struct_item") => Some(SymbolKind::Struct),
        (CodeLanguage::Rust, "enum_item") => Some(SymbolKind::Enum),
        (CodeLanguage::Rust, "trait_item") => Some(SymbolKind::Trait),
        (CodeLanguage::Rust, "mod_item") => Some(SymbolKind::Module),
        (CodeLanguage::Rust, "const_item" | "static_item") => Some(SymbolKind::Constant),
        (CodeLanguage::Rust, "type_item") => Some(SymbolKind::TypeAlias),
        (CodeLanguage::TypeScript | CodeLanguage::JavaScript, "function_declaration") => {
            Some(SymbolKind::Function)
        }
        (CodeLanguage::TypeScript | CodeLanguage::JavaScript, "method_definition") => {
            Some(SymbolKind::Method)
        }
        (CodeLanguage::TypeScript | CodeLanguage::JavaScript, "class_declaration") => {
            Some(SymbolKind::Class)
        }
        (CodeLanguage::TypeScript, "interface_declaration") => Some(SymbolKind::Interface),
        (CodeLanguage::TypeScript, "type_alias_declaration") => Some(SymbolKind::TypeAlias),
        (CodeLanguage::Python, "function_definition") => Some(SymbolKind::Function),
        (CodeLanguage::Python, "class_definition") => Some(SymbolKind::Class),
        _ => None,
    }
}

fn is_identifier(kind: &str) -> bool {
    matches!(
        kind,
        "identifier" | "type_identifier" | "property_identifier"
    )
}

fn is_dependency_node(language: CodeLanguage, kind: &str) -> bool {
    match language {
        CodeLanguage::Rust => kind == "use_declaration",
        CodeLanguage::TypeScript | CodeLanguage::JavaScript => kind == "import_statement",
        CodeLanguage::Python => matches!(kind, "import_statement" | "import_from_statement"),
    }
}

fn source_location(node: Node<'_>) -> SourceLocation {
    let start = node.start_position();
    let end = node.end_position();
    SourceLocation {
        start_line: u32::try_from(start.row.saturating_add(1)).unwrap_or(u32::MAX),
        start_column: u32::try_from(start.column.saturating_add(1)).unwrap_or(u32::MAX),
        end_line: u32::try_from(end.row.saturating_add(1)).unwrap_or(u32::MAX),
        end_column: u32::try_from(end.column.saturating_add(1)).unwrap_or(u32::MAX),
    }
}

fn source_line(source: &str, row: usize) -> String {
    source
        .lines()
        .nth(row)
        .map(compact_text)
        .unwrap_or_default()
        .chars()
        .take(320)
        .collect()
}

fn compact_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
