//! Recursive-descent parser for the query syntax (see crate docs).
//!
//! ```text
//! expr    := or
//! or      := and ( "OR" and )*
//! and     := unary ( "AND"? unary )*
//! unary   := "-" unary | "NOT" unary | "(" expr ")" | term
//! ```

use eidos_domain::{
    ContentState, HostId, ObjectId, ObjectKind, PathMode, Query, QueryLimits, SizeField, SourceId,
    TextField, TextMode, TimeField, UnixNanos,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{message} (at {position})")]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedQuery {
    pub query: Query,
    /// Human-readable notes about how the input was interpreted.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen(usize),
    RParen(usize),
    Word(usize, String),
}

fn tokenize(input: &str) -> Result<Vec<Tok>, ParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '(' {
            out.push(Tok::LParen(i));
            i += 1;
            continue;
        }
        if c == ')' {
            out.push(Tok::RParen(i));
            i += 1;
            continue;
        }
        let start = i;
        let mut word = String::new();
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() || c == '(' || c == ')' {
                break;
            }
            let prev = chars.get(i.wrapping_sub(1)).copied();
            let at_value_start = i == start
                || matches!(prev, Some(':') | Some('=') | Some('~'))
                || (matches!(prev, Some('i')) && i >= 2 && chars[i - 2] == '=');
            if c == '"' {
                // Quoted segment.
                word.push('"');
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        if matches!(chars[i + 1], '\\' | '"') {
                            word.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        // Backslashes in quoted Windows/UNC paths are
                        // ordinary characters unless they escape a quote or
                        // another backslash.
                        word.push('\\');
                        i += 1;
                        continue;
                    }
                    if chars[i] == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    return Err(ParseError {
                        message: "unterminated quote".into(),
                        position: start,
                    });
                }
                word.push('"');
                continue;
            }
            if c == '/' && at_value_start {
                // Regex segment with optional trailing flags.
                word.push('/');
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        word.push('/');
                        i += 2;
                        continue;
                    }
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        word.push('\\');
                        word.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if chars[i] == '/' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    return Err(ParseError {
                        message: "unterminated regex".into(),
                        position: start,
                    });
                }
                word.push('/');
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    word.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            word.push(c);
            i += 1;
        }
        out.push(Tok::Word(start, word));
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    notes: Vec<String>,
    now: UnixNanos,
    max_depth: usize,
}

/// Parse query text into the typed AST.
pub fn parse(input: &str) -> Result<ParsedQuery, ParseError> {
    parse_at(input, UnixNanos::now())
}

/// Parse with an explicit "now" (for relative dates; used by tests).
pub fn parse_at(input: &str, now: UnixNanos) -> Result<ParsedQuery, ParseError> {
    let limits = QueryLimits::default();
    // A valid textual query can carry `max_clauses` values of up to
    // `max_text_len` bytes plus modest syntax overhead. Bound the tokenizer
    // before it duplicates the input into `Vec<char>` and token strings.
    let max_input_len = limits
        .max_clauses
        .saturating_mul(limits.max_text_len.saturating_add(32));
    if input.len() > max_input_len {
        return Err(ParseError {
            message: format!("query text is too long ({} > {max_input_len})", input.len()),
            position: 0,
        });
    }
    let toks = tokenize(input)?;
    let mut p = Parser {
        toks,
        pos: 0,
        notes: Vec::new(),
        now,
        max_depth: limits.max_depth,
    };
    if p.toks.is_empty() {
        return Ok(ParsedQuery {
            query: Query::All,
            notes: vec!["empty query matches everything".into()],
        });
    }
    let q = p.expr(1)?;
    if p.pos < p.toks.len() {
        let position = match &p.toks[p.pos] {
            Tok::LParen(i) | Tok::RParen(i) | Tok::Word(i, _) => *i,
        };
        return Err(ParseError {
            message: "unexpected token".into(),
            position,
        });
    }
    q.validate(&limits).map_err(|e| ParseError {
        message: e.to_string(),
        position: 0,
    })?;
    Ok(ParsedQuery {
        query: q,
        notes: p.notes,
    })
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(_, s)) if s == w)
    }

    fn expr(&mut self, depth: usize) -> Result<Query, ParseError> {
        self.or(depth)
    }

    fn or(&mut self, depth: usize) -> Result<Query, ParseError> {
        let mut parts = vec![self.and(depth)?];
        while self.is_word("OR") || self.is_word("|") {
            self.pos += 1;
            parts.push(self.and(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("one")
        } else {
            Query::Or { clauses: parts }
        })
    }

    fn and(&mut self, depth: usize) -> Result<Query, ParseError> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::RParen(_)) => break,
                Some(Tok::Word(_, w)) if w == "OR" || w == "|" => break,
                Some(Tok::Word(_, w)) if w == "AND" || w == "&&" => {
                    self.pos += 1;
                    continue;
                }
                _ => parts.push(self.unary(depth)?),
            }
        }
        if parts.is_empty() {
            let position = match self.peek() {
                Some(Tok::LParen(i) | Tok::RParen(i) | Tok::Word(i, _)) => *i,
                None => 0,
            };
            return Err(ParseError {
                message: "expected a term".into(),
                position,
            });
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("one")
        } else {
            Query::And { clauses: parts }
        })
    }

    fn unary(&mut self, depth: usize) -> Result<Query, ParseError> {
        if depth > self.max_depth {
            return Err(ParseError {
                message: format!(
                    "query syntax nesting depth {depth} exceeds limit {}",
                    self.max_depth
                ),
                position: self
                    .peek()
                    .map(|t| match t {
                        Tok::LParen(i) | Tok::RParen(i) | Tok::Word(i, _) => *i,
                    })
                    .unwrap_or(0),
            });
        }
        match self.peek().cloned() {
            Some(Tok::LParen(pos)) => {
                self.pos += 1;
                let inner = self.expr(depth + 1)?;
                match self.peek() {
                    Some(Tok::RParen(_)) => {
                        self.pos += 1;
                        Ok(inner)
                    }
                    _ => Err(ParseError {
                        message: "missing closing parenthesis".into(),
                        position: pos,
                    }),
                }
            }
            Some(Tok::RParen(pos)) => Err(ParseError {
                message: "unexpected ')'".into(),
                position: pos,
            }),
            // `NOT x`, `!x`, and a bare `-` in front of a group: `-(a b)`,
            // which is also what the renderer emits for a negated
            // conjunction.
            Some(Tok::Word(_, w)) if w == "NOT" || w == "!" || w == "-" => {
                self.pos += 1;
                Ok(Query::not(self.unary(depth + 1)?))
            }
            Some(Tok::Word(pos, w)) => {
                self.pos += 1;
                if let Some(rest) = w.strip_prefix('-') {
                    if !rest.is_empty() && !rest.chars().next().is_some_and(|c| c.is_ascii_digit())
                    {
                        return Ok(Query::not(self.term(pos, rest)?));
                    }
                }
                self.term(pos, &w)
            }
            None => Err(ParseError {
                message: "unexpected end of query".into(),
                position: 0,
            }),
        }
    }

    fn term(&mut self, pos: usize, raw: &str) -> Result<Query, ParseError> {
        if raw == "*" {
            return Ok(Query::All);
        }
        if raw.starts_with('"') || raw.starts_with('/') {
            return self.text_clause(TextField::Name, raw, true, pos);
        }
        // A bare absolute path (`G:\Tools`, `\\server\share`) scopes by path.
        if looks_absolute(raw)
            && (raw.len() <= 3
                || raw[2..].starts_with('\\')
                || raw[2..].starts_with('/')
                || raw.starts_with("\\\\"))
        {
            let mode = if raw.contains('*') || raw.contains('?') {
                PathMode::Glob
            } else {
                PathMode::Prefix
            };
            self.notes.push(format!(
                "\"{raw}\" interpreted as a path {}",
                if mode == PathMode::Glob {
                    "glob"
                } else {
                    "prefix"
                }
            ));
            return Ok(Query::Path {
                mode,
                value: raw.replace('/', "\\"),
                case_sensitive: false,
            });
        }
        if let Some(idx) = raw.find(':') {
            let (field, value) = (&raw[..idx], &raw[idx + 1..]);
            let field_l = field.to_ascii_lowercase();
            if let Some(q) = self.field_clause(&field_l, value, pos)? {
                return Ok(q);
            }
        }
        self.text_clause(TextField::Name, raw, true, pos)
    }

    /// Text clause from a value with optional mode prefixes.
    fn text_clause(
        &mut self,
        field: TextField,
        value: &str,
        bare: bool,
        pos: usize,
    ) -> Result<Query, ParseError> {
        if value.is_empty() {
            return Err(ParseError {
                message: "empty value".into(),
                position: pos,
            });
        }
        // Regex: /.../flags
        if let Some(rest) = value.strip_prefix('/') {
            if let Some(end) = rest.rfind('/') {
                let pattern = &rest[..end];
                let flags = &rest[end + 1..];
                let cs = flags.contains('c');
                let max_regex_len = QueryLimits::default().max_regex_len;
                if pattern.len() > max_regex_len {
                    return Err(ParseError {
                        message: format!("regex is too long ({} > {max_regex_len})", pattern.len()),
                        position: pos,
                    });
                }
                regex::RegexBuilder::new(pattern)
                    .case_insensitive(!cs)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|error| ParseError {
                        message: format!("invalid regex: {error}"),
                        position: pos,
                    })?;
                return Ok(Query::Text {
                    field,
                    mode: TextMode::Regex,
                    value: pattern.to_string(),
                    case_sensitive: cs,
                    slop: 0,
                });
            }
        }
        // Exact: =value (case-sensitive) or =ivalue (insensitive)
        if let Some(rest) = value.strip_prefix('=') {
            let (cs, v) = match rest.strip_prefix('i') {
                Some(v) if !v.is_empty() => (false, v),
                _ => (true, rest),
            };
            return Ok(Query::Text {
                field,
                mode: TextMode::Exact,
                value: unquote(v),
                case_sensitive: cs,
                slop: 0,
            });
        }
        // Case-sensitive substring: ~value
        if let Some(rest) = value.strip_prefix('~') {
            return Ok(Query::Text {
                field,
                mode: TextMode::Substring,
                value: unquote(rest),
                case_sensitive: true,
                slop: 0,
            });
        }
        // Quoted phrase: "..." optionally followed by ~slop
        if value.starts_with('"') {
            let (inner, slop) = match value.rfind('"') {
                Some(end) if end > 0 => {
                    let tail = &value[end + 1..];
                    let slop = tail.strip_prefix('~').and_then(|s| s.parse::<u32>().ok());
                    (value[1..end].to_string(), slop)
                }
                _ => (value.trim_matches('"').to_string(), None),
            };
            if bare && field == TextField::Name {
                return Ok(Query::Text {
                    field,
                    mode: if slop.is_some() {
                        TextMode::Proximity
                    } else {
                        TextMode::Phrase
                    },
                    value: inner,
                    case_sensitive: false,
                    slop: slop.unwrap_or(0),
                });
            }
            return Ok(Query::Text {
                field,
                mode: match (field, slop) {
                    (_, Some(_)) => TextMode::Proximity,
                    (TextField::Content, None) => TextMode::Phrase,
                    _ => TextMode::Substring,
                },
                value: inner,
                case_sensitive: false,
                slop: slop.unwrap_or(0),
            });
        }
        // Glob
        if value.contains('*') || value.contains('?') {
            // `*.ext` is an extension filter, not a dictionary walk.
            if field == TextField::Name {
                if let Some(ext) = value.strip_prefix("*.") {
                    if !ext.is_empty()
                        && ext
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                    {
                        self.notes
                            .push(format!("\"{value}\" interpreted as extension {ext}"));
                        return Ok(Query::Extension {
                            values: vec![ext.to_string()],
                        });
                    }
                }
            }
            let re = glob_to_regex(value);
            self.notes
                .push(format!("\"{value}\" interpreted as a glob"));
            return Ok(Query::Text {
                field,
                mode: TextMode::Regex,
                value: format!("^{re}$"),
                case_sensitive: false,
                slop: 0,
            });
        }
        Ok(Query::Text {
            field,
            mode: if bare || field == TextField::Content {
                TextMode::Ranked
            } else {
                TextMode::Substring
            },
            value: value.to_string(),
            case_sensitive: false,
            slop: 0,
        })
    }

    fn field_clause(
        &mut self,
        field: &str,
        value: &str,
        pos: usize,
    ) -> Result<Option<Query>, ParseError> {
        let err = |m: &str| ParseError {
            message: m.to_string(),
            position: pos,
        };
        Ok(Some(match field {
            "ranked" | "name_ranked" => Query::Text {
                field: TextField::Name,
                mode: TextMode::Ranked,
                value: unquote(value),
                case_sensitive: false,
                slop: 0,
            },
            "path_ranked" => Query::Text {
                field: TextField::Path,
                mode: TextMode::Ranked,
                value: unquote(value),
                case_sensitive: false,
                slop: 0,
            },
            "content_ranked" => Query::Text {
                field: TextField::Content,
                mode: TextMode::Ranked,
                value: unquote(value),
                case_sensitive: false,
                slop: 0,
            },
            "name" | "n" | "file" => self.text_clause(TextField::Name, value, false, pos)?,
            "content" | "text" | "body" => {
                self.text_clause(TextField::Content, value, false, pos)?
            }
            "path" | "p" | "folder" | "dir" => {
                if value.is_empty() {
                    return Err(err("empty path"));
                }
                if let Some(rest) = value.strip_prefix('=') {
                    Query::Path {
                        mode: PathMode::Exact,
                        value: unquote(rest),
                        case_sensitive: false,
                    }
                } else if value.starts_with('/') && value.len() > 1 && value[1..].contains('/') {
                    match self.text_clause(TextField::Path, value, false, pos)? {
                        Query::Text {
                            value,
                            case_sensitive,
                            ..
                        } => Query::Path {
                            mode: PathMode::Regex,
                            value,
                            case_sensitive,
                        },
                        other => other,
                    }
                } else {
                    let v = unquote(value);
                    if v.contains('*') || v.contains('?') {
                        Query::Path {
                            mode: PathMode::Glob,
                            value: v,
                            case_sensitive: false,
                        }
                    } else if looks_absolute(&v) {
                        Query::Path {
                            mode: PathMode::Prefix,
                            value: v,
                            case_sensitive: false,
                        }
                    } else {
                        Query::Text {
                            field: TextField::Path,
                            mode: TextMode::Substring,
                            value: v,
                            case_sensitive: false,
                            slop: 0,
                        }
                    }
                }
            }
            "ext" | "e" | "extension" => {
                let values: Vec<String> = value
                    .split(',')
                    .map(|v| v.trim().trim_start_matches('.').to_ascii_lowercase())
                    .map(|v| {
                        if v == "none" || v == "-" {
                            String::new()
                        } else {
                            v
                        }
                    })
                    .collect();
                Query::Extension { values }
            }
            "kind" | "k" | "type" | "is" => {
                let mut values = Vec::new();
                for v in value.split(',') {
                    values.push(match v.to_ascii_lowercase().as_str() {
                        "file" | "files" | "f" => ObjectKind::File,
                        "dir" | "directory" | "folder" | "d" | "folders" | "dirs" => {
                            ObjectKind::Directory
                        }
                        "reparse" | "link" | "symlink" | "junction" => ObjectKind::Reparse,
                        other => return Err(err(&format!("unknown kind: {other}"))),
                    });
                }
                Query::Kind { values }
            }
            "size" | "sz" => {
                let (min, max) = parse_size_range(value).ok_or_else(|| err("invalid size"))?;
                Query::Size {
                    field: SizeField::Logical,
                    min,
                    max,
                }
            }
            "alloc" | "allocated" => {
                let (min, max) = parse_size_range(value).ok_or_else(|| err("invalid size"))?;
                Query::Size {
                    field: SizeField::Allocated,
                    min,
                    max,
                }
            }
            "subtree" | "tree" => {
                let (min, max) = parse_size_range(value).ok_or_else(|| err("invalid size"))?;
                Query::SubtreeSize {
                    field: SizeField::Logical,
                    min,
                    max,
                }
            }
            "subtree_alloc" | "tree_alloc" => {
                let (min, max) = parse_size_range(value).ok_or_else(|| err("invalid size"))?;
                Query::SubtreeSize {
                    field: SizeField::Allocated,
                    min,
                    max,
                }
            }
            "files" | "count" => {
                let (min, max) = parse_size_range(value).ok_or_else(|| err("invalid count"))?;
                Query::DescendantCount { min, max }
            }
            "mtime" | "modified" | "m" | "dm" => {
                let (after, before) =
                    parse_time_range(value, self.now).ok_or_else(|| err("invalid time"))?;
                Query::Time {
                    field: TimeField::Modified,
                    after,
                    before,
                }
            }
            "subtree_mtime" | "subtree_modified" | "tree_mtime" => {
                let (after, before) =
                    parse_time_range(value, self.now).ok_or_else(|| err("invalid time"))?;
                Query::Time {
                    field: TimeField::SubtreeModified,
                    after,
                    before,
                }
            }
            "ctime" | "created" | "dc" => {
                let (after, before) =
                    parse_time_range(value, self.now).ok_or_else(|| err("invalid time"))?;
                Query::Time {
                    field: TimeField::Created,
                    after,
                    before,
                }
            }
            "state" | "content_state" | "indexed" => {
                let mut states = Vec::new();
                for v in value.split(',') {
                    states.push(
                        ContentState::parse(&v.to_ascii_lowercase())
                            .ok_or_else(|| err("unknown state"))?,
                    );
                }
                Query::ContentState { states }
            }
            "has" | "contains_ext" => {
                let v = value.trim_start_matches('.');
                let (ext, min, max) = if let Some((e, rest)) = v.split_once(">=") {
                    (
                        e,
                        rest.parse::<u64>().map_err(|_| err("invalid count"))?,
                        None,
                    )
                } else if let Some((e, rest)) = v.split_once('>') {
                    (
                        e,
                        rest.parse::<u64>().map_err(|_| err("invalid count"))? + 1,
                        None,
                    )
                } else if let Some((e, rest)) = v.split_once(':') {
                    let (a, b) = rest
                        .split_once("..")
                        .ok_or_else(|| err("expected min..max"))?;
                    (
                        e,
                        a.parse::<u64>().map_err(|_| err("invalid count"))?,
                        Some(b.parse::<u64>().map_err(|_| err("invalid count"))?),
                    )
                } else {
                    (v, 1, None)
                };
                Query::DescendantExtension {
                    extension: ext.to_ascii_lowercase(),
                    min_count: min,
                    max_count: max,
                }
            }
            "attr" | "attrs" | "a" => {
                let mut all_of = 0u32;
                for v in value.split(',') {
                    all_of |= crate::attr_bit(v)
                        .ok_or_else(|| err(&format!("unknown attribute: {v}")))?;
                }
                Query::Attributes { all_of, none_of: 0 }
            }
            "source" | "src" | "volume" | "drive" => {
                let mut ids = Vec::new();
                let mut names = Vec::new();
                for v in value.split(',') {
                    let v = v.trim();
                    if let Ok(n) = v.parse::<i64>() {
                        ids.push(SourceId(n));
                    } else if let Some(n) = v.strip_prefix("s:").and_then(|x| x.parse::<i64>().ok())
                    {
                        ids.push(SourceId(n));
                    } else if !v.is_empty() {
                        names.push(v.trim_end_matches(['\\', ':']).to_string());
                    }
                }
                Query::Source { ids, names }
            }
            "host" | "h" => {
                let ids = value
                    .split(',')
                    .filter_map(|v| v.trim().trim_start_matches("h:").parse::<i64>().ok())
                    .map(HostId)
                    .collect();
                Query::Host { ids }
            }
            "object" | "obj" | "o" => {
                let ids = value
                    .split(',')
                    .filter_map(|v| v.trim().trim_start_matches("o:").parse::<i64>().ok())
                    .map(ObjectId)
                    .collect();
                Query::Object { ids }
            }
            "in" | "under" => {
                let (target, depth) = match value.rsplit_once('~') {
                    Some((t, d)) if d.parse::<u32>().is_ok() => (t, d.parse::<u32>().ok()),
                    _ => (value, None),
                };
                if let Some(id) = target
                    .strip_prefix("o:")
                    .and_then(|x| x.parse::<i64>().ok())
                {
                    Query::DescendantOf {
                        directory: ObjectId(id),
                        max_depth: depth,
                    }
                } else if let Ok(id) = target.parse::<i64>() {
                    Query::DescendantOf {
                        directory: ObjectId(id),
                        max_depth: depth,
                    }
                } else {
                    Query::Path {
                        mode: PathMode::Prefix,
                        value: unquote(target),
                        case_sensitive: false,
                    }
                }
            }
            _ => return Ok(None),
        }))
    }
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn looks_absolute(v: &str) -> bool {
    let b = v.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
        || v.starts_with("\\\\")
        || v.starts_with("//")
}

/// Convert a glob (`*`, `?`) into a regex body.
pub fn glob_to_regex(glob: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                out.push_str(".*");
                while i + 1 < chars.len() && chars[i + 1] == '*' {
                    i += 1;
                }
            }
            '?' => out.push('.'),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

/// Parse a byte size with optional binary unit suffix: `10`, `4k`, `1.5M`, `2GiB`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.trim_end_matches("ib").trim_end_matches('b');
    let (num, mult) = if let Some(n) = s.strip_suffix('k') {
        (n, 1u64 << 10)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix('g') {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix('t') {
        (n, 1u64 << 40)
    } else {
        (s, 1u64)
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mult as f64).round() as u64)
}

/// `>N`, `>=N`, `<N`, `<=N`, `N..M`, `=N`, `N`
pub fn parse_size_range(s: &str) -> Option<(Option<u64>, Option<u64>)> {
    let s = s.trim();
    if let Some(v) = s.strip_prefix(">=") {
        return Some((Some(parse_size(v)?), None));
    }
    if let Some(v) = s.strip_prefix('>') {
        return Some((Some(parse_size(v)?.saturating_add(1)), None));
    }
    if let Some(v) = s.strip_prefix("<=") {
        return Some((None, Some(parse_size(v)?)));
    }
    if let Some(v) = s.strip_prefix('<') {
        let n = parse_size(v)?;
        return Some((None, Some(n.saturating_sub(1))));
    }
    if let Some((a, b)) = s.split_once("..") {
        let lo = if a.is_empty() {
            None
        } else {
            Some(parse_size(a)?)
        };
        let hi = if b.is_empty() {
            None
        } else {
            Some(parse_size(b)?)
        };
        return Some((lo, hi));
    }
    let v = parse_size(s.trim_start_matches('='))?;
    Some((Some(v), Some(v)))
}

const DAY_NS: i64 = 86_400_000_000_000;

/// Start and exclusive end of a date-like token at its granularity.
fn date_span(s: &str, now: UnixNanos) -> Option<(UnixNanos, UnixNanos)> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "today" => {
            let start = UnixNanos(now.0 - now.0.rem_euclid(DAY_NS));
            return Some((start, UnixNanos(start.0 + DAY_NS)));
        }
        "yesterday" => {
            let start = UnixNanos(now.0 - now.0.rem_euclid(DAY_NS) - DAY_NS);
            return Some((start, UnixNanos(start.0 + DAY_NS)));
        }
        _ => {}
    }
    // Relative durations: 7d, 24h, 30m, 2w, 3mo, 1y → [now - dur, now]
    if let Some(dur) = parse_duration_ns(s) {
        return Some((UnixNanos(now.0 - dur), UnixNanos(now.0 + 1)));
    }
    let digits_only = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    if s.len() == 4 && digits_only(s) {
        let y: i64 = s.parse().ok()?;
        let a = UnixNanos::parse(&format!("{y:04}-01-01"))?;
        let b = UnixNanos::parse(&format!("{:04}-01-01", y + 1))?;
        return Some((a, b));
    }
    if s.len() == 7 && digits_only(&s[..4]) && &s[4..5] == "-" && digits_only(&s[5..]) {
        let y: i64 = s[..4].parse().ok()?;
        let m: i64 = s[5..].parse().ok()?;
        let a = UnixNanos::parse(&format!("{y:04}-{m:02}-01"))?;
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let b = UnixNanos::parse(&format!("{ny:04}-{nm:02}-01"))?;
        return Some((a, b));
    }
    if s.len() == 10 {
        let a = UnixNanos::parse(s)?;
        return Some((a, UnixNanos(a.0 + DAY_NS)));
    }
    let a = UnixNanos::parse(s)?;
    Some((a, UnixNanos(a.0 + 1_000_000_000)))
}

fn parse_duration_ns(s: &str) -> Option<i64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.')?);
    let n: f64 = num.parse().ok()?;
    let unit_ns: f64 = match unit {
        "m" | "min" | "mins" => 60e9,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600e9,
        "d" | "day" | "days" => 86_400e9,
        "w" | "wk" | "week" | "weeks" => 7.0 * 86_400e9,
        "mo" | "month" | "months" => 30.0 * 86_400e9,
        "y" | "yr" | "year" | "years" => 365.0 * 86_400e9,
        _ => return None,
    };
    Some((n * unit_ns) as i64)
}

/// `>D`, `>=D`, `<D`, `<=D`, `D..E`, `D`, `7d`, `today`
pub fn parse_time_range(s: &str, now: UnixNanos) -> Option<(Option<UnixNanos>, Option<UnixNanos>)> {
    let s = s.trim();
    if let Some(v) = s.strip_prefix(">=") {
        return Some((Some(date_span(v, now)?.0), None));
    }
    if let Some(v) = s.strip_prefix('>') {
        return Some((Some(date_span(v, now)?.1), None));
    }
    if let Some(v) = s.strip_prefix("<=") {
        return Some((None, Some(date_span(v, now)?.1)));
    }
    if let Some(v) = s.strip_prefix('<') {
        return Some((None, Some(date_span(v, now)?.0)));
    }
    if let Some((a, b)) = s.split_once("..") {
        let lo = if a.is_empty() {
            None
        } else {
            Some(date_span(a, now)?.0)
        };
        let hi = if b.is_empty() {
            None
        } else {
            Some(date_span(b, now)?.1)
        };
        return Some((lo, hi));
    }
    let (a, b) = date_span(s.trim_start_matches('='), now)?;
    Some((Some(a), Some(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Query {
        parse_at(s, UnixNanos(1_787_000_000_000_000_000))
            .unwrap()
            .query
    }

    #[test]
    fn bare_words_are_ranked_name_terms() {
        assert_eq!(
            q("readme notes"),
            Query::and(vec![
                Query::text(TextField::Name, TextMode::Ranked, "readme"),
                Query::text(TextField::Name, TextMode::Ranked, "notes"),
            ])
        );
    }

    #[test]
    fn field_modes() {
        assert_eq!(q("name:=Qz"), Query::exact(TextField::Name, "Qz"));
        assert_eq!(
            q("name:foo"),
            Query::Text {
                field: TextField::Name,
                mode: TextMode::Substring,
                value: "foo".into(),
                case_sensitive: false,
                slop: 0
            }
        );
        assert_eq!(
            q("name:/ab+c/c"),
            Query::Text {
                field: TextField::Name,
                mode: TextMode::Regex,
                value: "ab+c".into(),
                case_sensitive: true,
                slop: 0
            }
        );
        assert_eq!(
            q("path:G:\\Tools"),
            Query::Path {
                mode: PathMode::Prefix,
                value: "G:\\Tools".into(),
                case_sensitive: false
            }
        );
        assert_eq!(q("ext:cs,IDB,none"), Query::extension(&["cs", "idb", ""]));
        assert_eq!(
            q("has:cs>3"),
            Query::DescendantExtension {
                extension: "cs".into(),
                min_count: 4,
                max_count: None
            }
        );
        assert_eq!(
            q("attr:hidden,system"),
            Query::Attributes {
                all_of: 0x6,
                none_of: 0
            }
        );
        assert_eq!(
            q("source:G,2"),
            Query::Source {
                ids: vec![SourceId(2)],
                names: vec!["G".into()]
            }
        );
    }

    #[test]
    fn invalid_regex_is_rejected_during_parse() {
        // Minimized hosted-fuzz input: the token shape used its final slash as
        // a delimiter and left the actual pattern with an unmatched `\`.
        let error = parse("//\0\0\0.C:ҍ\\/.fi").unwrap_err();
        assert!(error.message.contains("invalid regex"), "{error}");
        assert!(parse(r#"//\/.fi"#).is_err());
    }

    #[test]
    fn sizes_and_times() {
        assert_eq!(parse_size("1.5M"), Some(1_572_864));
        assert_eq!(parse_size("4k"), Some(4096));
        assert_eq!(parse_size_range(">1M"), Some((Some(1_048_577), None)));
        assert_eq!(
            parse_size_range("1M..10M"),
            Some((Some(1 << 20), Some(10 << 20)))
        );
        let now = UnixNanos(1_787_000_000_000_000_000);
        let (a, b) = parse_time_range("2026-01-01", now).unwrap();
        assert_eq!(a.unwrap().to_rfc3339(), "2026-01-01T00:00:00.000Z");
        assert_eq!(b.unwrap().to_rfc3339(), "2026-01-02T00:00:00.000Z");
        let (a, b) = parse_time_range("2026-02", now).unwrap();
        assert_eq!(a.unwrap().to_rfc3339(), "2026-02-01T00:00:00.000Z");
        assert_eq!(b.unwrap().to_rfc3339(), "2026-03-01T00:00:00.000Z");
        let (a, _) = parse_time_range("7d", now).unwrap();
        assert_eq!(a.unwrap().0, now.0 - 7 * DAY_NS);
        let (a, b) = parse_time_range(">=2026-01-01", now).unwrap();
        assert!(b.is_none());
        assert_eq!(a.unwrap().to_rfc3339(), "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn boolean_structure() {
        assert_eq!(
            q("(a OR b) -ext:log"),
            Query::and(vec![
                Query::or(vec![
                    Query::text(TextField::Name, TextMode::Ranked, "a"),
                    Query::text(TextField::Name, TextMode::Ranked, "b"),
                ]),
                Query::not(Query::extension(&["log"])),
            ])
        );
        assert_eq!(
            q("NOT x"),
            Query::not(Query::text(TextField::Name, TextMode::Ranked, "x"))
        );
        assert!(parse("(a").is_err());
        assert!(parse("\"unterminated").is_err());
        assert_eq!(q(""), Query::All);
    }

    #[test]
    fn quoted_and_glob() {
        assert_eq!(
            q("\"hello world\""),
            Query::Text {
                field: TextField::Name,
                mode: TextMode::Phrase,
                value: "hello world".into(),
                case_sensitive: false,
                slop: 0
            }
        );
        assert_eq!(
            q("*.cs"),
            Query::Extension {
                values: vec!["cs".into()]
            }
        );
        assert_eq!(
            q("setup?.cs"),
            Query::Text {
                field: TextField::Name,
                mode: TextMode::Regex,
                value: "^setup.\\.cs$".into(),
                case_sensitive: false,
                slop: 0
            }
        );
        assert_eq!(
            q("name:\"my file\""),
            Query::Text {
                field: TextField::Name,
                mode: TextMode::Substring,
                value: "my file".into(),
                case_sensitive: false,
                slop: 0
            }
        );
    }

    #[test]
    fn bare_absolute_path_is_a_prefix_and_unknown_fields_are_terms() {
        assert_eq!(
            q("C:\\stuff"),
            Query::Path {
                mode: PathMode::Prefix,
                value: "C:\\stuff".into(),
                case_sensitive: false
            }
        );
        assert_eq!(
            q("foo:bar"),
            Query::text(TextField::Name, TextMode::Ranked, "foo:bar")
        );
    }
}
