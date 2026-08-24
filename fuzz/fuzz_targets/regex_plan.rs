#![no_main]

use std::collections::BTreeSet;

use eidos_search::content::trigrams;
use eidos_search::regex_plan::TrigramPlan;
use libfuzzer_sys::fuzz_target;

const MAX_REGEX_BYTES: usize = 1024;
const MAX_TEXT_BYTES: usize = 4096;

fn candidate_accepts(plan: &TrigramPlan, indexed: &BTreeSet<String>) -> bool {
    match plan {
        TrigramPlan::All => true,
        TrigramPlan::Lit(literal) => trigrams(literal)
            .into_iter()
            .all(|term| indexed.contains(&term)),
        TrigramPlan::And(parts) => parts.iter().all(|part| candidate_accepts(part, indexed)),
        TrigramPlan::Or(parts) => parts.iter().any(|part| candidate_accepts(part, indexed)),
    }
}

// Corpus/input format: one mode byte (`c` for case-sensitive, anything else
// for insensitive), then `<pattern>\n<haystack>` as UTF-8.
fuzz_target!(|data: &[u8]| {
    let Some((&mode, payload)) = data.split_first() else {
        return;
    };
    let Ok(payload) = std::str::from_utf8(payload) else {
        return;
    };
    let Some((pattern, text)) = payload.split_once('\n') else {
        return;
    };
    if pattern.len() > MAX_REGEX_BYTES || text.len() > MAX_TEXT_BYTES {
        return;
    }

    let case_sensitive = mode == b'c';
    let Ok(regex) = regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(1 << 20)
        .build()
    else {
        return;
    };
    if !regex.is_match(text) {
        return;
    }

    let plan = TrigramPlan::for_regex_with_case(pattern, case_sensitive);
    let indexed = trigrams(text).into_iter().collect();
    assert!(
        candidate_accepts(&plan, &indexed),
        "candidate false negative: /{pattern}/ on {text:?} planned as {plan}"
    );
});
