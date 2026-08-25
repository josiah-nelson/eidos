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
pub mod render;

pub use parser::{parse, ParseError, ParsedQuery};
pub use render::render;

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
            "*.-",
            "*.NoNe",
            "in:fixture",
            "attr:hidden,system",
            "mtime:2026-01-01..2026-01-03",
            "fixtuxtr e:r\"kure_fieldC:Lctu\\\\\\f2ile?.txt\nd:\"alph\u{17}\u{5}ſſUa",
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
            "mtime:>=2026-08-16T00:00:00.000000000Z mtime:<2026-08-23T00:00:00.000000000Z"
        );
    }

    #[test]
    fn relative_time_ranges_keep_sub_millisecond_bounds() {
        let now = eidos_domain::UnixNanos(1_787_000_000_000_000_000);
        let q = parser::parse_at("in:fmi m m:0m", now).unwrap().query;
        let rendered = render(&q);
        assert!(rendered.contains(".000000001Z"), "{rendered}");
        assert_eq!(parser::parse_at(&rendered, now).unwrap().query, q);
    }
}
