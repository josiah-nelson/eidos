//! Stored-text preview: `GET /api/objects/{id}/content`.
//!
//! The catalog is the extraction cache: every chunk's decoded text is stored
//! with its exact byte and line ranges. This endpoint serves *that* copy —
//! never the source file — so a preview cannot leak bytes the indexer never
//! read, cannot be addressed by filesystem path, and keeps working after the
//! file moves or disappears.
//!
//! Contract:
//!
//! - Address: object id + content generation + chunk ordinal, plus up to
//!   [`MAX_NEIGHBORS`] neighbouring chunks per side.
//! - `generation` is optional. When it is given and does not match the
//!   generation the stored chunks belong to, the request is rejected with
//!   `409 Conflict` and `kind: "stale_generation"` carrying
//!   `current_generation`, so a client holding an old search hit refetches
//!   rather than silently rendering text from another version of the file.
//! - `stale: true` in a successful response means the object has moved on to
//!   a newer generation than the stored text (re-extraction is pending). The
//!   text is still the newest text the catalog has.
//! - Text is sanitised (C0/C1 controls other than tab/CR/LF, and bidi
//!   overrides, become U+FFFD) and returned as JSON strings, so a client that
//!   renders it as a text node cannot be affected by its contents.

use crate::admission::Expensive;
use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use eidos_catalog::content::ChunkRow;
use eidos_domain::{ContentState, Coverage, ObjectId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Neighbouring chunks per side. Requests above this are clamped, not
/// rejected: the response reports what it actually contains.
pub const MAX_NEIGHBORS: u32 = 4;
/// Sanitised text per response. A single chunk larger than this is cut.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Lines of text per response.
pub const MAX_RESPONSE_LINES: usize = 4_000;

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    /// Content generation the caller believes it is reading (from a search
    /// hit's `content.generation`). Omit to read whatever is current.
    #[serde(default)]
    generation: Option<u32>,
    /// Chunk to centre the window on.
    #[serde(default)]
    ordinal: u32,
    #[serde(default)]
    before: u32,
    #[serde(default)]
    after: u32,
}

#[derive(Debug, Serialize)]
pub struct ChunkView {
    pub ordinal: u32,
    /// Byte range in the decoded source `[byte_start, byte_end)`.
    pub byte_start: u64,
    pub byte_end: u64,
    /// Zero-based, inclusive line range.
    pub line_start: u64,
    pub line_end: u64,
    /// Characters in the stored chunk, before any truncation here.
    pub chars: u32,
    pub text: String,
    /// `text` was cut to keep the response within its limits.
    pub truncated: bool,
    /// Control characters in the stored text were replaced with U+FFFD.
    pub sanitized: bool,
}

#[derive(Debug, Serialize)]
pub struct PreviewLimits {
    pub max_neighbors: u32,
    pub max_bytes: usize,
    pub max_lines: usize,
}

#[derive(Debug, Serialize)]
pub struct PreviewView {
    pub object_id: ObjectId,
    /// Current rendered path, for labelling only.
    pub path: Option<String>,
    /// Generation the returned text belongs to.
    pub generation: u32,
    /// The object's current generation.
    pub object_generation: u32,
    /// The object changed after this text was extracted.
    pub stale: bool,
    pub state: ContentState,
    pub coverage: Coverage,
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    /// Chunks stored for this generation.
    pub chunk_count: u32,
    pub line_count: u64,
    pub encoding: Option<String>,
    /// Why coverage is partial, or why extraction failed.
    pub reason: Option<String>,
    pub requested_ordinal: u32,
    pub chunks: Vec<ChunkView>,
    pub has_more_before: bool,
    pub has_more_after: bool,
    /// Text was dropped or cut to respect the limits below.
    pub truncated: bool,
    pub limits: PreviewLimits,
}

/// Stored text around one chunk of one object generation.
///
/// A preview decompresses up to `2 * MAX_NEIGHBORS + 1` chunks on a blocking
/// thread, so it goes through the same admission gate as browsing: the permit
/// is owned by the blocking closure, which keeps a client that disconnects
/// mid-request from releasing it while the catalog read is still running.
pub async fn object_content(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<PreviewQuery>,
) -> ApiResult<PreviewView> {
    let (before, after) = (q.before.min(MAX_NEIGHBORS), q.after.min(MAX_NEIGHBORS));
    let (object, generation, ordinal) = (ObjectId(id), q.generation, q.ordinal);
    let gate = st.admission.clone();
    let view = gate
        .run(Expensive::Browse, move || {
            load_preview(&st, object, generation, ordinal, before, after)
        })
        .await??;
    Ok(Json(view))
}

fn load_preview(
    st: &AppState,
    object: ObjectId,
    requested_generation: Option<u32>,
    ordinal: u32,
    before: u32,
    after: u32,
) -> Result<PreviewView, ApiError> {
    let obj = st
        .catalog
        .get_object(object)?
        .filter(|o| o.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found(format!("object {object}")))?;
    let rec = st
        .catalog
        .content_record(object)?
        .ok_or_else(|| ApiError::not_found(format!("object {object} has no extracted content")))?;
    if let Some(asked) = requested_generation {
        if asked != rec.generation {
            return Err(ApiError::conflict(format!(
                "object {object} content generation {asked} is no longer stored; \
                 the catalog now holds generation {}",
                rec.generation
            ))
            .with_kind("stale_generation")
            .with_details(serde_json::json!({
                "object_id": object.0,
                "requested_generation": asked,
                "current_generation": rec.generation,
            })));
        }
    }
    // Chunks of superseded generations are deleted on store, so the record's
    // generation is the only one with text.
    let from = ordinal.saturating_sub(before);
    let to = ordinal.saturating_add(after);
    let rows = st.catalog.chunks_range(object, rec.generation, from, to)?;
    let center = rows
        .iter()
        .position(|r| r.ordinal == ordinal)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "object {object} generation {} has no chunk {ordinal} ({} stored)",
                rec.generation, rec.chunk_count
            ))
        })?;

    let (chunks, mut truncated) = window(&rows, center);
    let first = chunks.first().map(|c| c.ordinal).unwrap_or(ordinal);
    let last = chunks.last().map(|c| c.ordinal).unwrap_or(ordinal);
    // `chunk_count` is the number of chunks stored for this generation, so
    // the highest ordinal is one less; guard against an empty record.
    let last_stored = rec.chunk_count.saturating_sub(1).max(last);
    truncated |= last < to.min(last_stored) || first > from;
    Ok(PreviewView {
        object_id: object,
        path: st.catalog.render_path(object)?,
        generation: rec.generation,
        object_generation: obj.generation,
        stale: rec.generation != obj.generation,
        state: rec.state,
        coverage: rec.coverage,
        indexed_bytes: rec.indexed_bytes,
        total_bytes: rec.total_bytes,
        chunk_count: rec.chunk_count,
        line_count: rec.line_count,
        encoding: rec.encoding.clone(),
        reason: rec.reason.clone().or_else(|| rec.error.clone()),
        requested_ordinal: ordinal,
        has_more_before: first > 0,
        has_more_after: last < last_stored,
        chunks,
        truncated,
        limits: PreviewLimits {
            max_neighbors: MAX_NEIGHBORS,
            max_bytes: MAX_RESPONSE_BYTES,
            max_lines: MAX_RESPONSE_LINES,
        },
    })
}

/// The requested chunk plus as many whole neighbours as the byte and line
/// budgets allow, expanded alternately outwards so the window stays
/// contiguous and centred. Returns the window and whether anything was cut.
fn window(rows: &[ChunkRow], center: usize) -> (Vec<ChunkView>, bool) {
    let mut budget = Budget {
        bytes: MAX_RESPONSE_BYTES,
        lines: MAX_RESPONSE_LINES,
    };
    // The requested chunk always comes back, cut down if it alone is too big.
    let mut out = std::collections::VecDeque::new();
    let (view, mut truncated) = center_view(&rows[center], &mut budget);
    out.push_back(view);
    let (mut lo, mut hi) = (center, center);
    let (mut grow_lo, mut grow_hi) = (true, true);
    while grow_lo || grow_hi {
        let mut grew = false;
        if grow_lo {
            match lo.checked_sub(1).and_then(|i| take(&rows[i], &mut budget)) {
                Some(v) => {
                    out.push_front(v);
                    lo -= 1;
                    grew = true;
                }
                None => {
                    truncated |= lo > 0;
                    grow_lo = false;
                }
            }
        }
        if grow_hi {
            match rows.get(hi + 1).and_then(|r| take(r, &mut budget)) {
                Some(v) => {
                    out.push_back(v);
                    hi += 1;
                    grew = true;
                }
                None => {
                    truncated |= hi + 1 < rows.len();
                    grow_hi = false;
                }
            }
        }
        if !grew {
            break;
        }
    }
    (out.into(), truncated)
}

struct Budget {
    bytes: usize,
    lines: usize,
}

/// A neighbour is included only whole: a partial neighbour would misreport
/// its own byte and line ranges.
fn take(row: &ChunkRow, budget: &mut Budget) -> Option<ChunkView> {
    let (text, sanitized) = sanitize(&row.text);
    let lines = text.lines().count().max(1);
    if text.len() > budget.bytes || lines > budget.lines {
        return None;
    }
    budget.bytes -= text.len();
    budget.lines -= lines;
    Some(view(row, text, sanitized, false))
}

fn center_view(row: &ChunkRow, budget: &mut Budget) -> (ChunkView, bool) {
    let (text, sanitized) = sanitize(&row.text);
    let (text, cut) = truncate(text, budget.bytes, budget.lines);
    budget.bytes -= text.len();
    budget.lines = budget.lines.saturating_sub(text.lines().count());
    (view(row, text, sanitized, cut), cut)
}

fn view(row: &ChunkRow, text: String, sanitized: bool, truncated: bool) -> ChunkView {
    ChunkView {
        ordinal: row.ordinal,
        byte_start: row.byte_start,
        byte_end: row.byte_end,
        line_start: row.line_start,
        line_end: row.line_end,
        chars: row.chars,
        text,
        truncated,
        sanitized,
    }
}

/// True for characters that must never reach a terminal or a DOM as-is:
/// C0/C1 controls other than tab, CR and LF, and the bidirectional
/// formatting characters that let stored text reorder what surrounds it.
fn unsafe_char(c: char) -> bool {
    match c {
        '\t' | '\n' | '\r' => false,
        '\u{061C}' | '\u{200E}' | '\u{200F}' => true,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => true,
        _ => c.is_control(),
    }
}

/// Replace (never delete) unsafe characters, so character offsets into the
/// stored text still line up with what the client sees.
fn sanitize(text: &str) -> (String, bool) {
    if !text.chars().any(unsafe_char) {
        return (text.to_string(), false);
    }
    let out = text
        .chars()
        .map(|c| if unsafe_char(c) { '\u{FFFD}' } else { c })
        .collect();
    (out, true)
}

/// Cut `text` to at most `max_bytes` bytes and `max_lines` lines, on a
/// character boundary.
fn truncate(text: String, max_bytes: usize, max_lines: usize) -> (String, bool) {
    if text.len() <= max_bytes && text.lines().count() <= max_lines {
        return (text, false);
    }
    let mut end = 0;
    let mut lines = 1;
    for (i, c) in text.char_indices() {
        if i + c.len_utf8() > max_bytes {
            break;
        }
        if c == '\n' {
            lines += 1;
            if lines > max_lines {
                break;
            }
        }
        end = i + c.len_utf8();
    }
    let mut text = text;
    text.truncate(end);
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_controls_but_keeps_whitespace_and_unicode() {
        let (out, changed) = sanitize("a\tb\r\nc\u{0}\u{1b}[31m\u{7f}\u{202e}d — 😀 漢");
        assert!(changed);
        assert_eq!(
            out,
            "a\tb\r\nc\u{fffd}\u{fffd}[31m\u{fffd}\u{fffd}d — 😀 漢"
        );
        assert_eq!(sanitize("plain text"), ("plain text".to_string(), false));
    }

    #[test]
    fn truncate_respects_char_boundaries_and_lines() {
        let (out, cut) = truncate("😀😀😀".into(), 5, 100);
        assert!(cut);
        assert_eq!(out, "😀");
        let (out, cut) = truncate("a\nb\nc\n".into(), 1000, 2);
        assert!(cut);
        assert_eq!(out, "a\nb");
        assert_eq!(truncate("short".into(), 1000, 100), ("short".into(), false));
    }
}
