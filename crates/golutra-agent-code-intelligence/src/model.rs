use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CODE_GRAPH_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeLanguage {
    Rust,
    TypeScript,
    JavaScript,
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    Module,
    Constant,
    TypeAlias,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceLocation {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeSymbol {
    pub symbol_id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub language: CodeLanguage,
    pub path: String,
    pub location: SourceLocation,
    pub container: Option<String>,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeReference {
    pub symbol_name: String,
    pub path: String,
    pub location: SourceLocation,
    pub context: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeEdgeKind {
    Contains,
    References,
    Imports,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeEdge {
    pub from: String,
    pub to: String,
    pub kind: CodeEdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeGraph {
    pub version: u32,
    pub workspace_root: String,
    pub generated_at: DateTime<Utc>,
    pub source_digest: String,
    pub files_indexed: u32,
    pub symbols: Vec<CodeSymbol>,
    pub references: Vec<CodeReference>,
    pub edges: Vec<CodeEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolQueryResult {
    pub query: String,
    pub matches: Vec<CodeSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReferenceQueryResult {
    pub symbol_name: String,
    pub references: Vec<CodeReference>,
}
