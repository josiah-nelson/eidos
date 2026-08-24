//! Query syntax: a compact, Everything-style text syntax that compiles to
//! the typed AST (`eidos_domain::Query`), plus a renderer that turns any AST
//! back into readable text for the "editable interpretation" UI.
//!
//! Syntax summary (see `parser.rs` for the grammar):
//!
//! ```text
//! word                 ranked name/path term (all words required)
//! "some phrase"        name phrase
//! name:foo             name contains "foo" (case-insensitive)
//! name:=Qz             name is exactly "Qz" (case-sensitive)
//! name:~Qz             name contains "Qz" (case-sensitive)
//! name:/re(ge)x/       regex (case-insensitive); /.../c for case-sensitive
//! name:*.cs            glob
//! path:G:\Tools        under that directory (prefix)
//! path:/re/  path:*x*  regex / glob on the full path
//! ext:cs,idb           extension in set; ext:none for extensionless
//! kind:file|dir        object kind
//! size:>1M  size:1M..10M  alloc:<4k
//! mtime:>2026-01-01  mtime:7d  ctime:2026-01..2026-03
//! state:pending        content state
//! has:idb              directory containing .idb anywhere beneath; has:cs>3
//! files:>100  subtree:>1G  subtree_alloc:>1G   directory predicates
//! subtree_mtime:>=2026-01-01   newest change beneath a directory
//! attr:hidden          attribute set (readonly hidden system reparse ...)
//! source:G             configured source name or id
//! in:o:123             under object id 123
//! content:...          content clauses (Milestone 4)
//! -term  NOT term      negation
//! a OR b  (a b) OR c   disjunction and grouping
//! ```

pub mod parser;

pub use parser::{parse, ParseError, ParsedQuery};

use eidos_domain::{PathMode, Query, SizeField, TextField, TextMode, TimeField};

/// Render an AST as readable text (not necessarily re-parseable for every
/// clause, but close to the input syntax).
pub fn render(q: &Query) -> String {
    fn quoted(v: &str) -> String {
        let mut out = String::with_capacity(v.len() + 2);
        out.push('"');
        for c in v.chars() {
            if matches!(c, '\\' | '"') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    }

    fn ranked_bare_safe(v: &str) -> bool {
        !v.is_empty()
            && !v.starts_with(['-', '/', '\\'])
            && !matches!(v, "NOT" | "!" | "OR" | "|" | "AND" | "&&")
            && !v.chars().any(|c| {
                c.is_whitespace() || matches!(c, '(' | ')' | '"' | ':' | '*' | '?' | '=' | '~')
            })
    }

    fn size(v: u64) -> String {
        const U: [&str; 5] = ["", "k", "M", "G", "T"];
        let mut f = v as f64;
        let mut i = 0;
        while f >= 1024.0 && i < 4 {
            f /= 1024.0;
            i += 1;
        }
        if i == 0 {
            return format!("{v}");
        }
        let unit = if f.fract() == 0.0 {
            format!("{f:.0}{}", U[i])
        } else {
            format!("{f:.1}{}", U[i])
        };
        // Only use the unit form when it denotes exactly `v`; a rounded
        // suffix (`64.0k` for 65535) would not survive a re-parse.
        match parser::parse_size(&unit) {
            Some(x) if x == v => unit,
            _ => format!("{v}"),
        }
    }
    fn go(q: &Query, out: &mut Vec<String>) {
        match q {
            Query::All => out.push("*".into()),
            Query::And { clauses } => {
                let parts: Vec<String> = clauses
                    .iter()
                    .map(|c| {
                        let mut v = Vec::new();
                        go(c, &mut v);
                        let s = v.join(" ");
                        if matches!(c, Query::And { .. } | Query::Or { .. }) {
                            format!("({s})")
                        } else {
                            s
                        }
                    })
                    .collect();
                out.push(parts.join(" "));
            }
            Query::Or { clauses } => {
                let parts: Vec<String> = clauses
                    .iter()
                    .map(|c| {
                        let mut v = Vec::new();
                        go(c, &mut v);
                        let s = v.join(" ");
                        if matches!(c, Query::And { .. } | Query::Or { .. }) {
                            format!("({s})")
                        } else {
                            s
                        }
                    })
                    .collect();
                out.push(parts.join(" OR "));
            }
            Query::Not { clause } => {
                let mut v = Vec::new();
                go(clause, &mut v);
                let s = v.join(" ");
                // Always retain the unary boundary. `-0` is an ordinary
                // numeric-looking term in the parser, not negation, and
                // nested Boolean expressions also need their grouping.
                out.push(format!("-({s})"));
            }
            Query::Text {
                field,
                mode,
                value,
                case_sensitive,
                slop,
            } => {
                let prefix = match field {
                    TextField::Name => "name:",
                    TextField::Path => "path:",
                    TextField::Content => "content:",
                };
                let s = match mode {
                    TextMode::Ranked => {
                        let implicit_is_ranked = match field {
                            TextField::Name | TextField::Content => ranked_bare_safe(value),
                            // `path:value` is substring mode, so a ranked path
                            // always needs its unambiguous internal spelling.
                            TextField::Path => false,
                        };
                        if implicit_is_ranked {
                            if *field == TextField::Name {
                                value.clone()
                            } else {
                                format!("{prefix}{value}")
                            }
                        } else {
                            let explicit = match field {
                                TextField::Name => "ranked:",
                                TextField::Path => "path_ranked:",
                                TextField::Content => "content_ranked:",
                            };
                            format!("{explicit}{}", quoted(value))
                        }
                    }
                    TextMode::Phrase if *field == TextField::Name => quoted(value),
                    TextMode::Phrase => format!("{prefix}{}", quoted(value)),
                    TextMode::Proximity if *field == TextField::Name => {
                        format!("{}~{slop}", quoted(value))
                    }
                    TextMode::Proximity => format!("{prefix}{}~{slop}", quoted(value)),
                    TextMode::Exact => {
                        if *case_sensitive {
                            format!("{prefix}={}", quoted(value))
                        } else {
                            format!("{prefix}=i{}", quoted(value))
                        }
                    }
                    TextMode::Substring => {
                        if *case_sensitive {
                            format!("{prefix}~{}", quoted(value))
                        } else {
                            format!("{prefix}{}", quoted(value))
                        }
                    }
                    TextMode::Regex => format!(
                        "{prefix}/{}/{}",
                        value.replace('/', "\\/"),
                        if *case_sensitive { "c" } else { "" }
                    ),
                };
                out.push(s);
            }
            Query::Host { ids } => out.push(format!(
                "host:{}",
                ids.iter()
                    .map(|i| i.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Query::Source { ids, names } => {
                let mut parts: Vec<String> = ids.iter().map(|i| i.0.to_string()).collect();
                parts.extend(names.iter().cloned());
                out.push(format!("source:{}", parts.join(",")));
            }
            Query::Object { ids } => out.push(format!(
                "object:{}",
                ids.iter()
                    .map(|i| i.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Query::Path {
                mode,
                value,
                case_sensitive,
            } => out.push(match mode {
                PathMode::Exact => format!("path:={}", quoted(value)),
                // `path:value` is a substring unless `value` looks absolute;
                // quoted `in:value` is unambiguously a path prefix for every
                // spelling, including numeric and relative paths.
                PathMode::Prefix => format!("in:{}", quoted(value)),
                PathMode::Glob => format!("path:{}", quoted(value)),
                PathMode::Regex => format!(
                    "path:/{}/{}",
                    value.replace('/', "\\/"),
                    if *case_sensitive { "c" } else { "" }
                ),
            }),
            Query::DescendantOf {
                directory,
                max_depth,
            } => out.push(match max_depth {
                Some(d) => format!("in:{directory}~{d}"),
                None => format!("in:{directory}"),
            }),
            Query::Extension { values } => out.push(format!(
                "ext:{}",
                values
                    .iter()
                    .map(|v| if v.is_empty() {
                        "none".to_string()
                    } else {
                        v.clone()
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Query::Kind { values } => out.push(format!(
                "kind:{}",
                values
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Query::Size { field, min, max } => {
                let key = match field {
                    SizeField::Logical => "size",
                    SizeField::Allocated => "alloc",
                };
                out.push(range_text(key, min.map(size), max.map(size)));
            }
            Query::Time {
                field,
                after,
                before,
            } => {
                let key = match field {
                    TimeField::Modified => "mtime",
                    TimeField::SubtreeModified => "subtree_mtime",
                    TimeField::Created => "ctime",
                    TimeField::Changed => "chtime",
                    TimeField::Accessed => "atime",
                };
                let day = |t: eidos_domain::UnixNanos| t.to_rfc3339();
                out.push(time_range_text(key, after.map(day), before.map(day)));
            }
            Query::Attributes { all_of, none_of } => {
                let required: Vec<&str> = (0..32u32)
                    .map(|bit| 1u32 << bit)
                    .filter(|mask| all_of & mask != 0)
                    .map(attr_name)
                    .collect();
                if !required.is_empty() {
                    out.push(format!("attr:{}", required.join(",")));
                }
                for bit in 0..32u32 {
                    let m = 1u32 << bit;
                    if none_of & m != 0 {
                        out.push(format!("-attr:{}", attr_name(m)));
                    }
                }
            }
            Query::ContentState { states } => out.push(format!(
                "state:{}",
                states
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Query::DescendantExtension {
                extension,
                min_count,
                max_count,
            } => out.push(match (min_count, max_count) {
                (1, None) => format!("has:{extension}"),
                (n, None) => format!("has:{extension}>={n}"),
                (n, Some(m)) => format!("has:{extension}:{n}..{m}"),
            }),
            Query::SubtreeSize { field, min, max } => {
                let key = match field {
                    SizeField::Logical => "subtree",
                    SizeField::Allocated => "subtree_alloc",
                };
                out.push(range_text(key, min.map(size), max.map(size)));
            }
            Query::DescendantCount { min, max } => out.push(range_text(
                "files",
                min.map(|v| v.to_string()),
                max.map(|v| v.to_string()),
            )),
            Query::Archive {
                in_archive,
                container,
                max_depth,
            } => {
                let mut s = String::from("archive:");
                if let Some(b) = in_archive {
                    s.push_str(if *b { "member" } else { "physical" });
                }
                if let Some(c) = container {
                    s.push_str(&format!(" container:{c}"));
                }
                if let Some(d) = max_depth {
                    s.push_str(&format!(" depth<={d}"));
                }
                out.push(s);
            }
        }
    }
    let mut v = Vec::new();
    go(q, &mut v);
    v.join(" ")
}

/// Time bounds are `[after, before)`, so the upper bound has to render as an
/// exclusive `<`: `key:<=T` and `key:A..B` both mean "up to and including T"
/// and would move the boundary when re-parsed.
fn time_range_text(key: &str, after: Option<String>, before: Option<String>) -> String {
    match (after, before) {
        // Keep a bounded range in one clause so formatting and reparsing do
        // not turn one `Query::Time` node into an implicit conjunction.
        (Some(a), Some(b)) => format!("{key}:>={a},<{b}"),
        (Some(a), None) => format!("{key}:>={a}"),
        (None, Some(b)) => format!("{key}:<{b}"),
        (None, None) => format!("{key}:*"),
    }
}

fn range_text(key: &str, min: Option<String>, max: Option<String>) -> String {
    match (min, max) {
        (Some(a), Some(b)) => format!("{key}:{a}..{b}"),
        (Some(a), None) => format!("{key}:>={a}"),
        (None, Some(b)) => format!("{key}:<={b}"),
        (None, None) => format!("{key}:*"),
    }
}

pub fn attr_name(bit: u32) -> &'static str {
    match bit {
        0x1 => "readonly",
        0x2 => "hidden",
        0x4 => "system",
        0x10 => "directory",
        0x20 => "archive",
        0x100 => "temporary",
        0x200 => "sparse",
        0x400 => "reparse",
        0x800 => "compressed",
        0x1000 => "offline",
        0x4000 => "encrypted",
        _ => "unknown",
    }
}

pub fn attr_bit(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "readonly" | "r" => 0x1,
        "hidden" | "h" => 0x2,
        "system" | "s" => 0x4,
        "directory" | "d" => 0x10,
        "archive" | "a" => 0x20,
        "temporary" | "t" => 0x100,
        "sparse" | "p" => 0x200,
        "reparse" | "l" | "link" => 0x400,
        "compressed" | "c" => 0x800,
        "offline" | "o" => 0x1000,
        "encrypted" | "e" => 0x4000,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_common_queries() {
        for q in [
            "readme ext:md mtime:>=2026-01-01",
            "name:=Qz -ext:log",
            "has:idb has:cs kind:directory",
            "(a OR b) ext:cs",
            "path:G:\\Tools size:>=1M",
            // Bucket clauses: bounded ranges, their grouped negations, and
            // the directory-side subtree predicates.
            "size:>=1M size:<16M",
            "-(size:>=1M size:<16M)",
            "mtime:>=2026-08-16 mtime:<2026-08-23",
            "-(mtime:>=2026-08-16 mtime:<2026-08-23)",
            "subtree:>=16M subtree:<256M",
            "subtree_mtime:>=2026-08-16 subtree_mtime:<2026-08-23",
            "-subtree_mtime:<2025-08-23",
        ] {
            let parsed = parse(q).unwrap();
            let rendered = render(&parsed.query);
            let again = parse(&rendered).unwrap();
            assert_eq!(parsed.query, again.query, "{q} -> {rendered}");
        }
    }

    #[test]
    fn negation_of_a_group_negates_the_whole_group() {
        let q = parse("-(size:>=1M size:<16M)").unwrap().query;
        let Query::Not { clause } = &q else {
            panic!("expected a negation, got {q:?}")
        };
        assert!(matches!(clause.as_ref(), Query::And { clauses } if clauses.len() == 2));
        assert_eq!(
            parse("-(a b)").unwrap().query,
            parse("NOT (a b)").unwrap().query
        );
    }

    #[test]
    fn parser_outputs_round_trip_without_changing_the_ast() {
        for input in [
            "",
            "*",
            "(a (b c))",
            "(a OR (b OR c))",
            "-(0)",
            r#"fixture_field:"alpha beta""#,
            r#"content:alpha" beta""#,
            r#"C:\fixture\file?.txt"#,
            "--fixture",
            "ranked:ext:txt",
            "ranked:*",
            "ranked:=fixture",
            "ranked:~fixture",
            r#"ranked:"/fm.txt""#,
            "path_ranked:fixture",
            r#"ranked:"a=/b""#,
            r#"ranked:"a~/b""#,
            r#"ranked:"a=i/b""#,
            "*.MiXeD",
            "in:fixture",
            "attr:hidden,system",
            "mtime:2026-01-01..2026-01-03",
        ] {
            let parsed = parse(input).unwrap();
            let rendered = render(&parsed.query);
            assert_eq!(parse(&rendered).unwrap().query, parsed.query, "{rendered}");
        }
    }

    #[test]
    fn sizes_render_exactly() {
        // A rounded binary suffix would not re-parse to the same byte count.
        for bytes in [
            0u64,
            1,
            4096,
            65_535,
            65_536,
            1_572_864,
            16_777_215,
            1 << 30,
        ] {
            let q = Query::Size {
                field: SizeField::Logical,
                min: Some(bytes),
                max: None,
            };
            let rendered = render(&q);
            assert_eq!(parse(&rendered).unwrap().query, q, "{bytes} -> {rendered}");
        }
        assert_eq!(
            render(&Query::Size {
                field: SizeField::Logical,
                min: Some(65_535),
                max: None
            }),
            "size:>=65535"
        );
    }

    /// `before` is exclusive; rendering it as `<=` would move the boundary.
    #[test]
    fn time_upper_bound_renders_exclusively() {
        let q = parse("mtime:>=2026-08-16 mtime:<2026-08-23").unwrap().query;
        assert_eq!(
            render(&q),
            "mtime:>=2026-08-16T00:00:00.000Z mtime:<2026-08-23T00:00:00.000Z"
        );
    }
}
