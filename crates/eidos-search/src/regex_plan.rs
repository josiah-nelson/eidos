//! Trigram query planning for regex clauses.
//!
//! A regex is turned into a boolean combination of folded trigrams that every
//! match must contain, after Cox's analysis for the trigram index in Google
//! Code Search: each sub-expression is summarised by the set of strings it
//! matches exactly (when bounded), the sets of strings every match starts
//! and ends with, and a trigram plan collected from information that the
//! summary can no longer carry. Alternations and optional pieces expand into
//! OR branches of longer strings instead of collapsing to a shared fragment,
//! so `timed? ?out after \d+` plans as
//! `"timeout after " | "timed out after " | "time out after " | "timedout after "`
//! rather than the weak `"time" & "out after "`.
//!
//! Strings are case folded with [`fold_chars`] so the plan addresses the
//! folded trigram fields; case sensitivity is enforced by verification.

use std::collections::BTreeSet;
use std::fmt;

use regex_syntax::hir::{Class, Hir, HirKind, Repetition};
use tantivy::query::Query as TQuery;
use tantivy::query::{AllQuery, BooleanQuery, Occur};

use crate::content::{fold_chars, trigrams};

/// Largest exact-string set kept; beyond it the set becomes prefix/suffix
/// information after its trigrams are folded into the plan.
const MAX_EXACT: usize = 20;
/// Largest prefix/suffix set kept; larger sets are cut to shorter strings
/// after their trigrams are folded into the plan.
const MAX_SET: usize = 20;
/// Largest character class expanded into alternatives; bigger classes count
/// as "any character".
const MAX_CLASS: usize = 10;
/// Bounded repetitions expand into at most this many copies.
const MAX_REPEAT: usize = 3;

/// Boolean combination of folded trigrams every match of a regex contains.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrigramPlan {
    /// No constraint: the regex has no usable literal.
    All,
    /// Every trigram of this folded string (three characters or more).
    Lit(String),
    And(Vec<TrigramPlan>),
    Or(Vec<TrigramPlan>),
}

impl TrigramPlan {
    /// Plan for a regex pattern; `All` when it cannot be narrowed (or does
    /// not parse — the matcher reports syntax errors).
    pub fn for_regex(pattern: &str) -> TrigramPlan {
        match regex_syntax::parse(pattern) {
            Ok(h) => analyze(&h).finish(),
            Err(_) => TrigramPlan::All,
        }
    }

    /// Plan requiring every literal (folded here); literals shorter than
    /// three characters contribute nothing.
    pub fn all_literals<'a>(literals: impl IntoIterator<Item = &'a str>) -> TrigramPlan {
        and(literals
            .into_iter()
            .map(|l| lit(fold_chars(l).into_iter().collect()))
            .collect())
    }

    pub fn is_all(&self) -> bool {
        matches!(self, TrigramPlan::All)
    }

    /// Tantivy query over a trigram field; `None` for `All`. `term` builds a
    /// term query for one trigram.
    pub fn query(&self, term: &dyn Fn(&str) -> Box<dyn TQuery>) -> Option<Box<dyn TQuery>> {
        fn build(p: &TrigramPlan, term: &dyn Fn(&str) -> Box<dyn TQuery>) -> Box<dyn TQuery> {
            match p {
                TrigramPlan::All => Box::new(AllQuery),
                TrigramPlan::Lit(s) => {
                    let mut qs: Vec<(Occur, Box<dyn TQuery>)> =
                        trigrams(s).iter().map(|t| (Occur::Must, term(t))).collect();
                    if qs.len() == 1 {
                        qs.pop().expect("one").1
                    } else {
                        Box::new(BooleanQuery::new(qs))
                    }
                }
                TrigramPlan::And(v) => Box::new(BooleanQuery::new(
                    v.iter().map(|c| (Occur::Must, build(c, term))).collect(),
                )),
                TrigramPlan::Or(v) => Box::new(BooleanQuery::new(
                    v.iter().map(|c| (Occur::Should, build(c, term))).collect(),
                )),
            }
        }
        (!self.is_all()).then(|| build(self, term))
    }

    /// Number of distinct trigram terms the query touches.
    pub fn term_count(&self) -> usize {
        fn collect(p: &TrigramPlan, out: &mut BTreeSet<String>) {
            match p {
                TrigramPlan::All => {}
                TrigramPlan::Lit(s) => out.extend(trigrams(s)),
                TrigramPlan::And(v) | TrigramPlan::Or(v) => v.iter().for_each(|c| collect(c, out)),
            }
        }
        let mut out = BTreeSet::new();
        collect(self, &mut out);
        out.len()
    }

    /// Whether every string matching this plan contains `s`.
    fn covers(&self, s: &str) -> bool {
        match self {
            TrigramPlan::All => false,
            TrigramPlan::Lit(l) => l.contains(s),
            TrigramPlan::And(v) => v.iter().any(|c| c.covers(s)),
            TrigramPlan::Or(v) => v.iter().all(|c| c.covers(s)),
        }
    }

    /// Whether every document passing this plan also passes `other`.
    fn implies(&self, other: &TrigramPlan) -> bool {
        match other {
            TrigramPlan::All => true,
            TrigramPlan::Lit(s) => self.covers(s),
            TrigramPlan::And(v) => v.iter().all(|c| self.implies(c)),
            TrigramPlan::Or(v) => v.iter().any(|c| self.implies(c)),
        }
    }
}

/// Drop parts made redundant by another part: for an AND a part implied by
/// a sibling, for an OR a part that implies a sibling. Equivalent parts keep
/// the first.
fn dedupe(parts: Vec<TrigramPlan>, is_and: bool) -> Vec<TrigramPlan> {
    let redundant = |i: usize| {
        parts.iter().enumerate().any(|(j, q)| {
            if j == i {
                return false;
            }
            let (p, q) = (&parts[i], q);
            let (fwd, back) = if is_and {
                (q.implies(p), p.implies(q))
            } else {
                (p.implies(q), q.implies(p))
            };
            fwd && (!back || j < i)
        })
    };
    let keep: Vec<bool> = (0..parts.len()).map(|i| !redundant(i)).collect();
    parts
        .into_iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(p))
        .collect()
}

impl fmt::Display for TrigramPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrigramPlan::All => write!(f, "*"),
            TrigramPlan::Lit(s) => write!(f, "{s:?}"),
            TrigramPlan::And(v) => {
                for (i, c) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, " & ")?;
                    }
                    if matches!(c, TrigramPlan::Or(_)) {
                        write!(f, "({c})")?;
                    } else {
                        write!(f, "{c}")?;
                    }
                }
                Ok(())
            }
            TrigramPlan::Or(v) => {
                for (i, c) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    if matches!(c, TrigramPlan::And(_)) {
                        write!(f, "({c})")?;
                    } else {
                        write!(f, "{c}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

// ----- plan constructors (always simplified) --------------------------------

fn lit(s: String) -> TrigramPlan {
    if s.chars().count() < 3 {
        TrigramPlan::All
    } else {
        TrigramPlan::Lit(s)
    }
}

fn and(parts: Vec<TrigramPlan>) -> TrigramPlan {
    let mut out: Vec<TrigramPlan> = Vec::new();
    for p in parts {
        match p {
            TrigramPlan::All => {}
            TrigramPlan::And(v) => out.extend(v),
            other => out.push(other),
        }
    }
    out.sort();
    out.dedup();
    let mut out = dedupe(out, true);
    match out.len() {
        0 => TrigramPlan::All,
        1 => out.pop().expect("one"),
        _ => TrigramPlan::And(out),
    }
}

fn or(parts: Vec<TrigramPlan>) -> TrigramPlan {
    let mut out: Vec<TrigramPlan> = Vec::new();
    for p in parts {
        match p {
            TrigramPlan::All => return TrigramPlan::All,
            TrigramPlan::Or(v) => out.extend(v),
            other => out.push(other),
        }
    }
    out.sort();
    out.dedup();
    let mut out = dedupe(out, false);
    match out.len() {
        0 => TrigramPlan::All,
        1 => out.pop().expect("one"),
        _ => TrigramPlan::Or(out),
    }
}

// ----- string sets ----------------------------------------------------------

type Set = BTreeSet<String>;

fn set1(s: &str) -> Set {
    let mut out = Set::new();
    out.insert(s.to_string());
    out
}

fn cross(a: &Set, b: &Set) -> Set {
    let mut out = Set::new();
    for x in a {
        for y in b {
            out.insert(format!("{x}{y}"));
        }
    }
    out
}

fn union(a: &Set, b: &Set) -> Set {
    a.union(b).cloned().collect()
}

/// Plan "contains one of these strings": an OR of literals, or `All` when
/// any string is too short to carry a trigram.
fn set_plan(s: &Set) -> TrigramPlan {
    if s.is_empty() || s.iter().any(|x| x.chars().count() < 3) {
        return TrigramPlan::All;
    }
    or(s.iter().map(|x| lit(x.clone())).collect())
}

/// Cut a set down to at most `MAX_SET` strings by shortening them (keeping
/// the front for prefixes, the back for suffixes). Callers fold the set's
/// trigrams into the plan first, so nothing the query needs is lost.
fn trim(mut s: Set, keep_front: bool) -> Set {
    let mut n = 3usize;
    while s.len() > MAX_SET {
        let longest = s.iter().map(|x| x.chars().count()).max().unwrap_or(0);
        n = n.min(longest.saturating_sub(1));
        s = s
            .iter()
            .map(|x| {
                let len = x.chars().count();
                if len <= n {
                    x.clone()
                } else if keep_front {
                    x.chars().take(n).collect()
                } else {
                    x.chars().skip(len - n).collect()
                }
            })
            .collect();
        if n == 0 {
            break;
        }
        n -= 1;
    }
    s
}

// ----- per-node summary -----------------------------------------------------

#[derive(Clone, Debug)]
struct Info {
    /// The expression can match the empty string.
    emptyable: bool,
    /// Every string the expression matches, when bounded and small.
    exact: Option<Set>,
    /// Strings every match starts with (meaningful when `exact` is `None`).
    prefix: Set,
    /// Strings every match ends with (meaningful when `exact` is `None`).
    suffix: Set,
    /// Trigram requirements collected so far.
    plan: TrigramPlan,
}

impl Info {
    fn empty_string() -> Info {
        Info {
            emptyable: true,
            exact: Some(set1("")),
            prefix: set1(""),
            suffix: set1(""),
            plan: TrigramPlan::All,
        }
    }
    fn any_char() -> Info {
        Info {
            emptyable: false,
            exact: None,
            prefix: set1(""),
            suffix: set1(""),
            plan: TrigramPlan::All,
        }
    }
    fn any_string() -> Info {
        Info {
            emptyable: true,
            ..Info::any_char()
        }
    }
    fn exact(set: Set) -> Info {
        Info {
            emptyable: set.contains(""),
            exact: Some(set),
            prefix: Set::new(),
            suffix: Set::new(),
            plan: TrigramPlan::All,
        }
        .simplify()
    }
    fn prefixes(&self) -> &Set {
        self.exact.as_ref().unwrap_or(&self.prefix)
    }
    fn suffixes(&self) -> &Set {
        self.exact.as_ref().unwrap_or(&self.suffix)
    }
    fn require(&mut self, set: &Set) {
        let p = set_plan(set);
        if !p.is_all() {
            self.plan = and(vec![std::mem::replace(&mut self.plan, TrigramPlan::All), p]);
        }
    }

    /// Keep the summary bounded: an oversized exact set becomes prefix and
    /// suffix sets, oversized sets are trimmed, and the information dropped
    /// on the way is folded into the plan first.
    fn simplify(mut self) -> Info {
        if let Some(ex) = &self.exact {
            if ex.len() > MAX_EXACT {
                let ex = self.exact.take().expect("checked");
                self.require(&ex);
                self.prefix = ex.clone();
                self.suffix = ex;
            }
        }
        if self.exact.is_none() {
            if self.prefix.len() > MAX_SET {
                let p = std::mem::take(&mut self.prefix);
                self.require(&p);
                self.prefix = trim(p, true);
            }
            if self.suffix.len() > MAX_SET {
                let s = std::mem::take(&mut self.suffix);
                self.require(&s);
                self.suffix = trim(s, false);
            }
        }
        self
    }

    fn finish(mut self) -> TrigramPlan {
        match self.exact.take() {
            Some(ex) => self.require(&ex),
            None => {
                let (p, s) = (
                    std::mem::take(&mut self.prefix),
                    std::mem::take(&mut self.suffix),
                );
                self.require(&p);
                self.require(&s);
            }
        }
        self.plan
    }
}

fn concat(x: Info, y: Info) -> Info {
    let mut plan = vec![x.plan.clone(), y.plan.clone()];
    let mut out = if let (Some(xe), Some(ye)) = (&x.exact, &y.exact) {
        Info {
            emptyable: x.emptyable && y.emptyable,
            exact: Some(cross(xe, ye)),
            prefix: Set::new(),
            suffix: Set::new(),
            plan: TrigramPlan::All,
        }
    } else {
        let prefix = match &x.exact {
            Some(xe) => cross(xe, y.prefixes()),
            None => {
                if y.exact.is_none() {
                    // y's prefixes are not carried forward.
                    plan.push(set_plan(y.prefixes()));
                }
                if x.emptyable {
                    union(&x.prefix, y.prefixes())
                } else {
                    x.prefix.clone()
                }
            }
        };
        let suffix = match &y.exact {
            Some(ye) => cross(x.suffixes(), ye),
            None => {
                if x.exact.is_none() {
                    // x's suffixes are not carried forward.
                    plan.push(set_plan(x.suffixes()));
                }
                if y.emptyable {
                    union(&y.suffix, x.suffixes())
                } else {
                    y.suffix.clone()
                }
            }
        };
        Info {
            emptyable: x.emptyable && y.emptyable,
            exact: None,
            prefix,
            suffix,
            plan: TrigramPlan::All,
        }
    };
    out.plan = and(plan);
    out.simplify()
}

fn alternate(x: Info, y: Info) -> Info {
    let plan = or(vec![x.plan.clone(), y.plan.clone()]);
    let out = match (&x.exact, &y.exact) {
        (Some(xe), Some(ye)) => Info {
            emptyable: x.emptyable || y.emptyable,
            exact: Some(union(xe, ye)),
            prefix: Set::new(),
            suffix: Set::new(),
            plan,
        },
        _ => Info {
            emptyable: x.emptyable || y.emptyable,
            exact: None,
            prefix: union(x.prefixes(), y.prefixes()),
            suffix: union(x.suffixes(), y.suffixes()),
            plan,
        },
    };
    out.simplify()
}

fn class(c: &Class) -> Info {
    let chars: Option<Vec<char>> = match c {
        Class::Unicode(u) => {
            let n: usize = u
                .ranges()
                .iter()
                .map(|r| (r.end() as usize) - (r.start() as usize) + 1)
                .sum();
            (n <= MAX_CLASS).then(|| {
                u.ranges()
                    .iter()
                    .flat_map(|r| (r.start() as u32..=r.end() as u32).filter_map(char::from_u32))
                    .collect()
            })
        }
        Class::Bytes(b) => {
            let n: usize = b
                .ranges()
                .iter()
                .map(|r| (r.end() as usize) - (r.start() as usize) + 1)
                .sum();
            (n <= MAX_CLASS && b.ranges().iter().all(|r| r.end().is_ascii())).then(|| {
                b.ranges()
                    .iter()
                    .flat_map(|r| (r.start()..=r.end()).map(char::from))
                    .collect()
            })
        }
    };
    match chars {
        Some(cs) if !cs.is_empty() => Info::exact(
            cs.iter()
                .map(|ch| fold_chars(&ch.to_string()).into_iter().collect())
                .collect(),
        ),
        _ => Info::any_char(),
    }
}

fn repetition(r: &Repetition) -> Info {
    let x = analyze(&r.sub);
    let min = r.min as usize;
    let mut out = if min == 0 {
        Info::empty_string()
    } else {
        let mut i = x.clone();
        for _ in 1..min.min(MAX_REPEAT) {
            i = concat(i, x.clone());
        }
        if min > MAX_REPEAT {
            i = concat(i, Info::any_string());
        }
        i
    };
    match r.max {
        Some(m) if m as usize == min => out,
        Some(m) if m as usize - min <= MAX_REPEAT && min <= MAX_REPEAT => {
            let optional = alternate(x, Info::empty_string());
            for _ in 0..(m as usize - min) {
                out = concat(out, optional.clone());
            }
            out
        }
        _ => concat(out, Info::any_string()),
    }
}

fn analyze(h: &Hir) -> Info {
    match h.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::empty_string(),
        HirKind::Literal(l) => match std::str::from_utf8(&l.0) {
            Ok(s) => Info::exact(set1(&fold_chars(s).into_iter().collect::<String>())),
            Err(_) => Info::any_char(),
        },
        HirKind::Class(c) => class(c),
        HirKind::Capture(c) => analyze(&c.sub),
        HirKind::Repetition(r) => repetition(r),
        HirKind::Concat(v) => v.iter().map(analyze).fold(Info::empty_string(), concat),
        HirKind::Alternation(v) => {
            let mut it = v.iter().map(analyze);
            let first = it.next().unwrap_or_else(Info::any_string);
            it.fold(first, alternate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(p: &str) -> String {
        TrigramPlan::for_regex(p).to_string()
    }

    #[test]
    fn optional_pieces_expand_into_alternatives() {
        assert_eq!(
            plan(r"timed? ?out after \d+"),
            r#""time out after " | "timed out after " | "timedout after " | "timeout after ""#
        );
        assert_eq!(plan(r"(error|warn)ing"), r#""erroring" | "warning""#);
        assert_eq!(plan(r"colou?r"), r#""color" | "colour""#);
    }

    #[test]
    fn inner_literals_survive_unbounded_neighbours() {
        assert_eq!(plan(r"postgresql-.*\.log$"), r#"".log" & "postgresql-""#);
        assert_eq!(plan(r"[A-Z][a-z]+Exception: "), r#""exception: ""#);
        assert_eq!(plan(r"abc.*def.*ghi"), r#""abc" & "def" & "ghi""#);
        assert_eq!(
            plan(r"^\s*import (foo|barbaz)\b"),
            r#""import barbaz" | "import foo""#
        );
    }

    #[test]
    fn weak_shapes_are_unconstrained() {
        assert!(TrigramPlan::for_regex(r"\d+").is_all());
        assert!(TrigramPlan::for_regex(r"[0-9]{4}-[0-9]{2}").is_all());
        assert!(TrigramPlan::for_regex(r"a|b").is_all());
        assert!(TrigramPlan::for_regex(r"(abc)?").is_all());
        assert!(TrigramPlan::for_regex(r"ab").is_all());
        // Unparseable: no constraint (the matcher reports the error).
        assert!(TrigramPlan::for_regex(r"(").is_all());
    }

    #[test]
    fn folding_and_case_classes() {
        assert_eq!(plan(r"(?i)Needle"), r#""needle""#);
        assert_eq!(plan(r"Qz\w+"), "*");
        assert_eq!(plan(r"QzEndpoint\w*"), r#""qzendpoint""#);
    }

    #[test]
    fn bounded_repetition_and_set_limits() {
        assert_eq!(plan(r"(ab){2}c"), r#""ababc""#);
        assert_eq!(plan(r"x{3,}"), r#""xxx""#);
        // 26 single-letter alternatives exceed the exact-set cap but still
        // keep the literal on both sides.
        let p = TrigramPlan::for_regex(
            r"needle(a|b|c|d|e|f|g|h|i|j|k|l|m|n|o|p|q|r|s|t|u|v|w|x|y|z)tail",
        );
        assert!(!p.is_all());
        assert!(p.covers("needle") && p.covers("tail"), "{p}");
        // Cross products stay bounded.
        let p = TrigramPlan::for_regex(r"[abcdefghij][abcdefghij][abcdefghij][abcdefghij]needle");
        assert!(p.covers("needle"), "{p}");
    }

    #[test]
    fn literal_lists_and_simplification() {
        assert_eq!(
            TrigramPlan::all_literals(["Abc", "de", "xyz"]).to_string(),
            r#""abc" & "xyz""#
        );
        assert!(TrigramPlan::all_literals(["ab"]).is_all());
        // Implied literals are dropped.
        assert_eq!(
            and(vec![
                lit("time".into()),
                or(vec![lit("timeout".into()), lit("timed out".into())])
            ])
            .to_string(),
            r#""timed out" | "timeout""#
        );
        assert_eq!(
            TrigramPlan::for_regex(r"abc.*abcd").to_string(),
            r#""abcd""#
        );
        assert_eq!(TrigramPlan::Lit("abcd".into()).term_count(), 2);
    }
}
