#![no_main]

use eidos_domain::UnixNanos;
use eidos_query::parser::parse_at;
use eidos_query::render;
use libfuzzer_sys::fuzz_target;

const MAX_FUZZ_QUERY_BYTES: usize = 16 * 1024;
const FIXED_NOW: UnixNanos = UnixNanos(1_787_000_000_000_000_000);

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_FUZZ_QUERY_BYTES {
        return;
    }
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(parsed) = parse_at(input, FIXED_NOW) else {
        return;
    };

    let rendered = render(&parsed.query);
    let reparsed = parse_at(&rendered, FIXED_NOW)
        .unwrap_or_else(|error| panic!("rendered query did not parse: {rendered:?}: {error}"));
    assert_eq!(
        reparsed.query, parsed.query,
        "parse/render changed the AST: {input:?} -> {rendered:?}"
    );
});
