//! Typed query AST.
//!
//! The AST is the public semantic contract (ARCHITECTURE invariant 11). The
//! web UI syntax, the CLI syntax, saved queries, MCP tools, and the future NLQ
//! adapter all compile to this structure. Nothing executes a query that has
//! not been expressed as a [`Query`].
//!
//! Serialization uses an internally tagged `op` discriminator so that JSON
//! forms are self-describing and stable:
//!
//! ```json
//! {"op":"and","clauses":[{"op":"extension","values":["md"]},
//!                        {"op":"text","field":"content","mode":"exact","value":"Qz"}]}
//! ```

use crate::ids::{HostId, ObjectId, SourceId};
use crate::state::{ContentState, ObjectKind};
use crate::time::UnixNanos;
use serde::{Deserialize, Serialize};

/// Query tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Query {
    /// Matches every object in scope.
    All,
    And {
        clauses: Vec<Query>,
    },
    Or {
        clauses: Vec<Query>,
    },
    Not {
        clause: Box<Query>,
    },
    /// Text clause against a name, path, or content field.
    Text {
        field: TextField,
        mode: TextMode,
        value: String,
        /// For `exact`, `substring`, and `regex`: whether case must match.
        /// Ignored by `ranked`/`phrase`/`proximity`, which are case-folded.
        #[serde(default)]
        case_sensitive: bool,
        /// Maximum token distance for `proximity`; ignored otherwise.
        #[serde(default)]
        slop: u32,
    },
    Host {
        ids: Vec<HostId>,
    },
    /// Restrict to sources by id and/or by configured name (names are
    /// resolved by the executor against the catalog).
    Source {
        #[serde(default)]
        ids: Vec<SourceId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        names: Vec<String>,
    },
    Object {
        ids: Vec<ObjectId>,
    },
    /// Path predicate against the rendered current path.
    Path {
        mode: PathMode,
        value: String,
        #[serde(default)]
        case_sensitive: bool,
    },
    /// Objects whose ancestor chain contains `directory`.
    DescendantOf {
        directory: ObjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_depth: Option<u32>,
    },
    /// Extension match. Values are compared case-insensitively without the
    /// leading dot. The empty string matches extensionless objects.
    Extension {
        values: Vec<String>,
    },
    Kind {
        values: Vec<ObjectKind>,
    },
    Size {
        field: SizeField,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u64>,
    },
    Time {
        field: TimeField,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<UnixNanos>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<UnixNanos>,
    },
    /// Windows attribute bit filter.
    Attributes {
        #[serde(default)]
        all_of: u32,
        #[serde(default)]
        none_of: u32,
    },
    /// Content-processing state filter.
    ContentState {
        states: Vec<ContentState>,
    },
    /// Directory predicate: descendant extension count within the subtree.
    DescendantExtension {
        extension: String,
        #[serde(default = "one")]
        min_count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_count: Option<u64>,
    },
    /// Directory predicate: subtree size.
    SubtreeSize {
        field: SizeField,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u64>,
    },
    /// Directory predicate: descendant file count.
    DescendantCount {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u64>,
    },
    /// Archive clause.
    Archive {
        /// `Some(true)`: only virtual members; `Some(false)`: only physical objects.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_archive: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        container: Option<ObjectId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_depth: Option<u32>,
    },
}

fn one() -> u64 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextField {
    /// Entry display name.
    Name,
    /// Full rendered path.
    Path,
    /// Extracted literal content.
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMode {
    /// Tokenised, case-folded, scored terms (all terms required).
    Ranked,
    /// Exact literal occurrence verified against original text.
    Exact,
    /// Tokenised phrase in order.
    Phrase,
    /// Tokenised terms within `slop` positions.
    Proximity,
    /// Raw substring (folded trigram candidates + verification).
    Substring,
    /// Regular expression (folded trigram candidates + verification).
    Regex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathMode {
    Exact,
    Prefix,
    Glob,
    Regex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeField {
    Logical,
    Allocated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeField {
    Modified,
    Created,
    Changed,
    Accessed,
}

/// Which object kinds to return as top-level results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResultMode {
    #[default]
    Files,
    Directories,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    AllocatedSize,
    SubtreeSize,
    Modified,
    Created,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Sort {
    #[serde(default)]
    pub field: SortField,
    #[serde(default)]
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetField {
    Source,
    Extension,
    Kind,
    ContentState,
    TopDirectory,
    SizeBucket,
    ModifiedBucket,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FacetRequest {
    pub field: FacetField,
    #[serde(default = "default_facet_limit")]
    pub limit: u32,
}

fn default_facet_limit() -> u32 {
    20
}

/// A complete search request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: Query,
    #[serde(default)]
    pub mode: ResultMode,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Include a machine- and human-readable explanation.
    #[serde(default)]
    pub explain: bool,
    /// Return content snippets for content clauses.
    #[serde(default = "default_true")]
    pub snippets: bool,
    #[serde(default)]
    pub facets: Vec<FacetRequest>,
    /// Include retired sources. Off by default.
    #[serde(default)]
    pub include_retired: bool,
}

fn default_limit() -> u32 {
    50
}
fn default_true() -> bool {
    true
}

impl SearchRequest {
    pub fn new(query: Query) -> Self {
        Self {
            query,
            mode: ResultMode::default(),
            sort: Sort::default(),
            limit: default_limit(),
            cursor: None,
            explain: false,
            snippets: true,
            facets: Vec::new(),
            include_retired: false,
        }
    }
}

/// Hard limits applied during validation.
#[derive(Debug, Clone, Copy)]
pub struct QueryLimits {
    pub max_clauses: usize,
    pub max_depth: usize,
    pub max_text_len: usize,
    pub max_regex_len: usize,
    pub max_limit: u32,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_clauses: 256,
            max_depth: 16,
            max_text_len: 4096,
            max_regex_len: 1024,
            max_limit: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryError {
    #[error("query has {count} clauses; limit is {limit}")]
    TooManyClauses { count: usize, limit: usize },
    #[error("query nesting depth {depth} exceeds limit {limit}")]
    TooDeep { depth: usize, limit: usize },
    #[error("text value too long ({len} > {limit})")]
    TextTooLong { len: usize, limit: usize },
    #[error("empty value in {clause} clause")]
    EmptyValue { clause: String },
    #[error("invalid regex: {message}")]
    InvalidRegex { message: String },
    #[error("regex is too broad to execute selectively: {message}")]
    RegexTooBroad { message: String },
    #[error("invalid glob: {message}")]
    InvalidGlob { message: String },
    #[error("{field:?} proximity requires slop > 0")]
    ProximityNeedsSlop { field: TextField },
    #[error("limit {limit} exceeds maximum {max}")]
    LimitTooLarge { limit: u32, max: u32 },
    #[error("invalid cursor")]
    InvalidCursor,
    #[error("{message}")]
    Other { message: String },
}

impl Query {
    pub fn and(clauses: Vec<Query>) -> Query {
        Query::And { clauses }
    }
    pub fn or(clauses: Vec<Query>) -> Query {
        Query::Or { clauses }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn not(clause: Query) -> Query {
        Query::Not {
            clause: Box::new(clause),
        }
    }
    pub fn text(field: TextField, mode: TextMode, value: impl Into<String>) -> Query {
        Query::Text {
            field,
            mode,
            value: value.into(),
            case_sensitive: false,
            slop: 0,
        }
    }
    pub fn exact(field: TextField, value: impl Into<String>) -> Query {
        Query::Text {
            field,
            mode: TextMode::Exact,
            value: value.into(),
            case_sensitive: true,
            slop: 0,
        }
    }
    pub fn extension(values: &[&str]) -> Query {
        Query::Extension {
            values: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Count clauses and maximum nesting depth.
    pub fn stats(&self) -> (usize, usize) {
        fn walk(q: &Query, depth: usize, count: &mut usize, max_depth: &mut usize) {
            *count += 1;
            *max_depth = (*max_depth).max(depth);
            match q {
                Query::And { clauses } | Query::Or { clauses } => {
                    for c in clauses {
                        walk(c, depth + 1, count, max_depth);
                    }
                }
                Query::Not { clause } => walk(clause, depth + 1, count, max_depth),
                _ => {}
            }
        }
        let mut count = 0;
        let mut depth = 0;
        walk(self, 1, &mut count, &mut depth);
        (count, depth)
    }

    /// Visit every clause in pre-order.
    pub fn visit<'a>(&'a self, f: &mut dyn FnMut(&'a Query)) {
        f(self);
        match self {
            Query::And { clauses } | Query::Or { clauses } => {
                for c in clauses {
                    c.visit(f);
                }
            }
            Query::Not { clause } => clause.visit(f),
            _ => {}
        }
    }

    /// Whether any clause requires the content index.
    pub fn needs_content(&self) -> bool {
        let mut needs = false;
        self.visit(&mut |q| {
            if let Query::Text {
                field: TextField::Content,
                ..
            } = q
            {
                needs = true;
            }
        });
        needs
    }

    /// Structural validation independent of any backend.
    pub fn validate(&self, limits: &QueryLimits) -> Result<(), QueryError> {
        let (count, depth) = self.stats();
        if count > limits.max_clauses {
            return Err(QueryError::TooManyClauses {
                count,
                limit: limits.max_clauses,
            });
        }
        if depth > limits.max_depth {
            return Err(QueryError::TooDeep {
                depth,
                limit: limits.max_depth,
            });
        }
        let mut err = None;
        self.visit(&mut |q| {
            if err.is_some() {
                return;
            }
            match q {
                Query::Text {
                    field,
                    mode,
                    value,
                    slop,
                    ..
                } => {
                    if value.is_empty() {
                        err = Some(QueryError::EmptyValue {
                            clause: "text".into(),
                        });
                    } else if *mode == TextMode::Regex && value.len() > limits.max_regex_len {
                        err = Some(QueryError::TextTooLong {
                            len: value.len(),
                            limit: limits.max_regex_len,
                        });
                    } else if value.len() > limits.max_text_len {
                        err = Some(QueryError::TextTooLong {
                            len: value.len(),
                            limit: limits.max_text_len,
                        });
                    } else if *mode == TextMode::Proximity && *slop == 0 {
                        err = Some(QueryError::ProximityNeedsSlop { field: *field });
                    }
                }
                Query::Path { value, .. } => {
                    if value.is_empty() {
                        err = Some(QueryError::EmptyValue {
                            clause: "path".into(),
                        });
                    } else if value.len() > limits.max_text_len {
                        err = Some(QueryError::TextTooLong {
                            len: value.len(),
                            limit: limits.max_text_len,
                        });
                    }
                }
                Query::DescendantExtension { extension, .. } => {
                    if extension.len() > 64 {
                        err = Some(QueryError::TextTooLong {
                            len: extension.len(),
                            limit: 64,
                        });
                    }
                }
                Query::Extension { values } if values.is_empty() => {
                    err = Some(QueryError::EmptyValue {
                        clause: "extension".into(),
                    });
                }
                Query::Source { ids, names } if ids.is_empty() && names.is_empty() => {
                    err = Some(QueryError::EmptyValue {
                        clause: "source".into(),
                    });
                }
                _ => {}
            }
        });
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl SearchRequest {
    pub fn validate(&self, limits: &QueryLimits) -> Result<(), QueryError> {
        self.query.validate(limits)?;
        if self.limit > limits.max_limit {
            return Err(QueryError::LimitTooLarge {
                limit: self.limit,
                max: limits.max_limit,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Query {
        Query::and(vec![
            Query::extension(&["md"]),
            Query::Time {
                field: TimeField::Modified,
                after: Some(UnixNanos(1_700_000_000_000_000_000)),
                before: None,
            },
            Query::or(vec![
                Query::text(TextField::Content, TextMode::Ranked, "zephyr diagnostics"),
                Query::exact(TextField::Content, "Zephyr"),
            ]),
            Query::not(Query::Path {
                mode: PathMode::Glob,
                value: "**/node_modules/**".into(),
                case_sensitive: false,
            }),
        ])
    }

    #[test]
    fn json_roundtrip() {
        let q = sample();
        let json = serde_json::to_string_pretty(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
        assert!(json.contains("\"op\": \"and\""));
        assert!(json.contains("\"mode\": \"exact\""));
    }

    #[test]
    fn json_shape_is_stable() {
        let q = Query::exact(TextField::Content, "Qz");
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "op": "text",
                "field": "content",
                "mode": "exact",
                "value": "Qz",
                "case_sensitive": true,
                "slop": 0
            })
        );
        // Defaults fill in on deserialization.
        let back: Query = serde_json::from_value(
            serde_json::json!({"op":"text","field":"name","mode":"ranked","value":"x"}),
        )
        .unwrap();
        assert_eq!(back, Query::text(TextField::Name, TextMode::Ranked, "x"));
    }

    #[test]
    fn request_defaults() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":{"op":"all"}}"#).unwrap();
        assert_eq!(r.limit, 50);
        assert_eq!(r.mode, ResultMode::Files);
        assert!(r.snippets);
        assert!(!r.explain);
    }

    #[test]
    fn stats_and_validation() {
        let q = sample();
        let (count, depth) = q.stats();
        assert_eq!(count, 8);
        assert_eq!(depth, 3);
        assert!(q.needs_content());
        assert!(q.validate(&QueryLimits::default()).is_ok());

        let bad = Query::text(TextField::Content, TextMode::Ranked, "");
        assert!(matches!(
            bad.validate(&QueryLimits::default()),
            Err(QueryError::EmptyValue { .. })
        ));

        let deep = (0..20).fold(Query::All, |acc, _| Query::not(acc));
        assert!(matches!(
            deep.validate(&QueryLimits::default()),
            Err(QueryError::TooDeep { .. })
        ));

        let prox = Query::text(TextField::Content, TextMode::Proximity, "a b");
        assert!(matches!(
            prox.validate(&QueryLimits::default()),
            Err(QueryError::ProximityNeedsSlop { .. })
        ));
    }

    #[test]
    fn unknown_op_rejected() {
        let r: Result<Query, _> = serde_json::from_str(r#"{"op":"teleport"}"#);
        assert!(r.is_err());
    }
}
