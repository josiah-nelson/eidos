use eidos_domain::{Query, QueryError, QueryLimits, TextField, TextMode, UnixNanos};
use eidos_query::parser::parse_at;
use eidos_query::render;
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed};

const NOW: UnixNanos = UnixNanos(1_787_000_000_000_000_000);

fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn unicode_value() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 1..32).prop_map(|chars| chars.into_iter().collect())
}

fn syntax_weighted_char() -> impl Strategy<Value = char> {
    prop_oneof![
        8 => prop::sample::select(vec![
            '/', '\\', '"', '-', '!', '=', '~', ':', '*', '?', '(', ')', ',', '|', '&', ' ',
            '\t', '\n', '\r', '\0',
        ]),
        4 => prop::char::range(' ', '~'),
        1 => any::<char>(),
    ]
}

fn leaf() -> impl Strategy<Value = String> {
    prop_oneof![
        1 => Just("*".to_string()),
        3 => "[a-z0-9_]{1,24}",
        2 => unicode_value().prop_map(|v| quoted(&v)),
        2 => unicode_value().prop_map(|v| format!("name:{}", quoted(&v))),
        2 => unicode_value().prop_map(|v| format!("name:~{}", quoted(&v))),
        2 => (unicode_value(), any::<bool>()).prop_map(|(v, insensitive)| {
            format!(
                "name:={}{}",
                if insensitive { "i" } else { "" },
                quoted(&v)
            )
        }),
        2 => unicode_value().prop_map(|v| format!("content:{}", quoted(&v))),
        1 => "[a-z0-9_]{1,24}".prop_map(|v| format!("content:{v}")),
        2 => unicode_value().prop_map(|v| {
            let path = format!("C:\\fixture\\{v}");
            format!("path:={}", quoted(&path))
        }),
        2 => unicode_value().prop_map(|v| {
            let pattern = regex::escape(&v).replace('/', "\\/");
            format!("name:/{pattern}/c")
        }),
        1 => (0u64..=u32::MAX as u64).prop_map(|v| format!("size:>={v}")),
        1 => prop::collection::vec("[a-zA-Z0-9]{1,8}", 1..5)
            .prop_map(|v| format!("ext:{}", v.join(","))),
        1 => "[a-zA-Z0-9_-]{1,24}".prop_map(|v| format!("*.{v}")),
        1 => Just("in:fixture".to_string()),
        1 => Just("attr:hidden,system".to_string()),
        1 => Just("mtime:2026-01-01..2026-01-03".to_string()),
    ]
}

fn query_text() -> impl Strategy<Value = String> {
    leaf().prop_recursive(5, 96, 4, |inner| {
        prop_oneof![
            3 => (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} {b})")),
            2 => (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} OR {b})")),
            1 => inner.prop_map(|a| format!("-({a})")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        rng_seed: RngSeed::Fixed(0xe1d0_0027),
        max_shrink_iters: 20_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn parsed_queries_round_trip_through_the_renderer(input in query_text()) {
        let parsed = parse_at(&input, NOW).expect("generated syntax is valid");
        let rendered = render(&parsed.query);
        let reparsed = parse_at(&rendered, NOW).unwrap_or_else(|error| {
            panic!("rendered query did not parse: {input:?} -> {rendered:?}: {error}")
        });
        prop_assert_eq!(
            reparsed.query,
            parsed.query,
            "{:?} -> {:?}",
            input,
            rendered
        );
    }

    #[test]
    fn clause_limit_is_enforced_at_the_exact_ast_count(
        leaves in 1usize..80,
        max_clauses in 1usize..80,
    ) {
        let query = if leaves == 1 {
            Query::All
        } else {
            Query::and(vec![Query::All; leaves])
        };
        let limits = QueryLimits {
            max_clauses,
            ..QueryLimits::default()
        };
        let (count, _) = query.stats();
        let result = query.validate(&limits);
        prop_assert_eq!(result.is_ok(), count <= max_clauses);
        if count > max_clauses {
            let exact_error = matches!(
                result,
                Err(QueryError::TooManyClauses { count: actual, limit })
                    if actual == count && limit == max_clauses
            );
            prop_assert!(exact_error);
        }
    }

    #[test]
    fn nesting_limit_is_enforced_at_the_exact_ast_depth(
        wrappers in 0usize..40,
        max_depth in 1usize..40,
    ) {
        let mut query = Query::All;
        for _ in 0..wrappers {
            query = Query::not(query);
        }
        let limits = QueryLimits {
            max_depth,
            max_clauses: usize::MAX,
            ..QueryLimits::default()
        };
        let (_, depth) = query.stats();
        let result = query.validate(&limits);
        prop_assert_eq!(result.is_ok(), depth <= max_depth);
        if depth > max_depth {
            let exact_error = matches!(
                result,
                Err(QueryError::TooDeep { depth: actual, limit })
                    if actual == depth && limit == max_depth
            );
            prop_assert!(exact_error);
        }
    }

    #[test]
    fn text_and_regex_byte_limits_are_enforced(
        len in 1usize..160,
        max_text_len in 1usize..128,
        regex in any::<bool>(),
    ) {
        let query = Query::Text {
            field: TextField::Name,
            mode: if regex { TextMode::Regex } else { TextMode::Substring },
            value: "x".repeat(len),
            case_sensitive: false,
            slop: 0,
        };
        let limits = QueryLimits {
            max_text_len,
            max_regex_len: max_text_len,
            ..QueryLimits::default()
        };
        let result = query.validate(&limits);
        prop_assert_eq!(result.is_ok(), len <= max_text_len);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 8_192,
        rng_seed: RngSeed::Fixed(0xe1d0_b027),
        max_shrink_iters: 60_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn syntax_weighted_accepted_inputs_round_trip(
        chars in prop::collection::vec(syntax_weighted_char(), 0..128),
    ) {
        let input: String = chars.into_iter().collect();
        if let Ok(parsed) = parse_at(&input, NOW) {
            let rendered = render(&parsed.query);
            let reparsed = parse_at(&rendered, NOW).unwrap_or_else(|error| {
                panic!("rendered query did not parse: {input:?} -> {rendered:?}: {error}")
            });
            prop_assert_eq!(
                reparsed.query,
                parsed.query,
                "{:?} -> {:?}",
                input,
                rendered
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 4_096,
        rng_seed: RngSeed::Fixed(0xe1d0_a027),
        max_shrink_iters: 40_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_accepted_utf8_round_trips(
        chars in prop::collection::vec(any::<char>(), 0..192),
    ) {
        let input: String = chars.into_iter().collect();
        if let Ok(parsed) = parse_at(&input, NOW) {
            let rendered = render(&parsed.query);
            let reparsed = parse_at(&rendered, NOW).unwrap_or_else(|error| {
                panic!("rendered query did not parse: {input:?} -> {rendered:?}: {error}")
            });
            prop_assert_eq!(
                reparsed.query,
                parsed.query,
                "{:?} -> {:?}",
                input,
                rendered
            );
        }
    }
}

/// The renderer's bare/quoted decision is `parser::bare_value_is_literal`.
/// Check it against the real lexer: every value it calls literal must lex
/// back as that exact `ext:` value, and every value it rejects must either
/// fail to parse bare or parse as something else, so quoting is required.
#[test]
fn bare_literal_predicate_matches_the_lexer() {
    let specials = "()\":=~*?\\,-/ \t";
    let mut samples: Vec<String> = vec!["abc".into(), "a1_b".into(), "é".into()];
    for c in specials.chars() {
        samples.push(format!("a{c}b"));
        samples.push(format!("{c}ab"));
        samples.push(format!("ab{c}"));
    }
    samples.extend(["NOT", "OR", "AND", "!", "|", "&&"].map(String::from));
    for v in samples {
        if v.contains(',') {
            // A comma is the list separator and is split before quotes are
            // removed, so no `ext:` value can contain one; the parser never
            // produces such an AST and the renderer never meets it.
            continue;
        }
        let bare = parse_at(&format!("ext:{v}"), NOW).map(|p| p.query);
        let expected = Query::Extension {
            values: vec![v.to_ascii_lowercase()],
        };
        if eidos_query::parser::bare_value_is_literal(&v) {
            assert_eq!(
                bare.ok(),
                Some(expected.clone()),
                "{v:?} is literal but did not lex as one value"
            );
        }
        // Whether bare or quoted, the renderer's choice must round-trip.
        // (The predicate may be stricter than one context needs: `a:b` is
        // literal inside an `ext:` value but an attribute as a bare word.)
        let rendered = render(&expected);
        assert_eq!(
            parse_at(&rendered, NOW).map(|p| p.query).ok(),
            Some(expected),
            "{v:?} rendered as {rendered:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x05ee_de17),
        ..ProptestConfig::default()
    })]

    /// Any extension value the parser can produce renders back to itself,
    /// whatever characters it contains.
    #[test]
    fn extension_values_round_trip_through_render(raw in unicode_value()) {
        let value: String = raw
            .to_ascii_lowercase()
            .trim_start_matches('.')
            .chars()
            .filter(|c| *c != ',')
            .collect();
        prop_assume!(!value.is_empty() && value != "none" && value != "-");
        let query = Query::Extension { values: vec![value.clone(), "cs".into()] };
        let rendered = render(&query);
        let reparsed = parse_at(&rendered, NOW)
            .unwrap_or_else(|e| panic!("{value:?} rendered as {rendered:?}: {e}"))
            .query;
        prop_assert_eq!(reparsed, query, "{:?}", rendered);
    }
}

#[test]
fn extension_values_with_regex_openers_survive_render() {
    // Hosted fuzz run: an `ext:` value containing `~/` rendered bare, and the
    // lexer then read `~/im|t...` as an unterminated regex.
    let fuzz = "\u{8}rane\n\u{1d}m\u{1d}79\u{1d}PPPPPPPPPPPP#PPPPPranke\"~a:/i/b\"\ne:\"a~/im|t\u{45a}\"\n";
    for input in [
        fuzz,
        "ext:\"a~/b\"",
        "ext:\"a=/b\"",
        "ext:\"a=i/b\"",
        "ext:\"a:/b\"",
        "ext:\"/b\"",
        "ext:\"a~b\",cs",
    ] {
        let parsed = parse_at(input, NOW).unwrap();
        let rendered = render(&parsed.query);
        let reparsed = parse_at(&rendered, NOW)
            .unwrap_or_else(|e| panic!("{input:?} rendered as {rendered:?}: {e}"));
        assert_eq!(reparsed.query, parsed.query, "{input:?} -> {rendered:?}");
    }
}

#[test]
fn quoted_backslashes_and_phrases_are_normal_regressions() {
    for input in [
        r#"path:="C:\fixture\folder with spaces\file.txt""#,
        r#"name:="i-prefixed exact""#,
        r#"name:~"quote \" and slash \\""#,
        r#""a phrase with spaces""#,
    ] {
        let parsed = parse_at(input, NOW).unwrap();
        let rendered = render(&parsed.query);
        assert_eq!(parse_at(&rendered, NOW).unwrap().query, parsed.query);
    }
}

#[test]
fn textual_parser_rejects_work_before_ast_limits_can_be_exceeded() {
    let limits = QueryLimits::default();
    let nested = format!(
        "{}needle{}",
        "(".repeat(limits.max_depth + 1),
        ")".repeat(limits.max_depth + 1)
    );
    let error = parse_at(&nested, NOW).unwrap_err();
    assert!(error.message.contains("syntax nesting depth"), "{error}");

    // An AND node also counts, so only max_clauses - 1 leaves fit.
    let too_many = vec!["needle"; limits.max_clauses].join(" ");
    let error = parse_at(&too_many, NOW).unwrap_err();
    assert!(error.message.contains("clauses"), "{error}");
}
