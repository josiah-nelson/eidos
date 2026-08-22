//! Catalog index schema (one document per live directory entry).
//!
//! Fields exist only for retrieval, sorting, faceting, and snippets; the
//! catalog remains canonical. Bump `CATALOG_SCHEMA_VERSION` on any change so
//! `CatalogIndex::open` rebuilds from the catalog.

use eidos_catalog::projection::ProjectionRow;
use eidos_domain::{FileAttributes, ObjectKind};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, FAST,
    INDEXED, STORED, STRING,
};
use tantivy::TantivyDocument;

pub const CATALOG_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct Fields {
    pub entry_id: Field,
    pub object_id: Field,
    pub source_id: Field,
    pub parent_id: Field,
    pub ancestors: Field,
    pub name: Field,
    pub name_folded: Field,
    pub path: Field,
    pub path_folded: Field,
    pub path_tokens: Field,
    pub extension: Field,
    pub kind: Field,
    pub content_state: Field,
    pub size: Field,
    pub allocated: Field,
    pub modified: Field,
    pub created: Field,
    pub attrs: Field,
    pub file_count: Field,
    pub dir_count: Field,
    pub subtree_logical: Field,
    pub subtree_allocated: Field,
    pub newest_modified: Field,
    pub agg_complete: Field,
    pub desc_ext: Field,
    pub generation: Field,
    pub link_count: Field,
    /// Folded character trigrams of the name / full path (doc ids only):
    /// candidate retrieval for substring, glob, and regex clauses.
    pub name_tri: Field,
    pub path_tri: Field,
}

pub fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let u64_fast_indexed = NumericOptions::default()
        .set_indexed()
        .set_fast()
        .set_stored();
    let u64_fast = NumericOptions::default().set_fast().set_stored();
    let i64_fast = NumericOptions::default().set_fast().set_stored();
    let text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let text_unstored = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let raw_fast = STRING | FAST;
    let trigrams = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(crate::content::TRIGRAM_TOKENIZER)
            .set_index_option(IndexRecordOption::Basic),
    );
    let fields = Fields {
        entry_id: b.add_u64_field("entry_id", u64_fast_indexed.clone()),
        object_id: b.add_u64_field("object_id", u64_fast_indexed.clone()),
        source_id: b.add_u64_field("source_id", u64_fast_indexed.clone()),
        parent_id: b.add_u64_field("parent_id", u64_fast_indexed.clone()),
        ancestors: b.add_u64_field("ancestors", INDEXED | STORED),
        name: b.add_text_field("name", text.clone()),
        name_folded: b.add_text_field("name_folded", raw_fast.clone()),
        path: b.add_text_field("path", STORED),
        path_folded: b.add_text_field("path_folded", raw_fast.clone()),
        path_tokens: b.add_text_field("path_tokens", text_unstored),
        extension: b.add_text_field("extension", raw_fast.clone()),
        kind: b.add_text_field("kind", raw_fast.clone()),
        content_state: b.add_text_field("content_state", raw_fast.clone()),
        size: b.add_u64_field("size", u64_fast.clone()),
        allocated: b.add_u64_field("allocated", u64_fast.clone()),
        modified: b.add_i64_field("modified", i64_fast.clone()),
        created: b.add_i64_field("created", i64_fast.clone()),
        attrs: b.add_text_field("attrs", STRING),
        file_count: b.add_u64_field("file_count", u64_fast.clone()),
        dir_count: b.add_u64_field("dir_count", u64_fast.clone()),
        subtree_logical: b.add_u64_field("subtree_logical", u64_fast.clone()),
        subtree_allocated: b.add_u64_field("subtree_allocated", u64_fast.clone()),
        newest_modified: b.add_i64_field("newest_modified", i64_fast),
        agg_complete: b.add_u64_field("agg_complete", u64_fast.clone()),
        desc_ext: b.add_text_field("desc_ext", STRING),
        generation: b.add_u64_field("generation", u64_fast.clone()),
        link_count: b.add_u64_field("link_count", u64_fast),
        name_tri: b.add_text_field("name_tri", trigrams.clone()),
        path_tri: b.add_text_field("path_tri", trigrams),
    };
    (b.build(), fields)
}

pub fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// Attribute terms stored in the `attrs` field.
pub fn attr_terms(a: FileAttributes) -> Vec<&'static str> {
    let mut out = Vec::new();
    let table: [(u32, &str); 11] = [
        (FileAttributes::READONLY, "readonly"),
        (FileAttributes::HIDDEN, "hidden"),
        (FileAttributes::SYSTEM, "system"),
        (FileAttributes::DIRECTORY, "directory"),
        (FileAttributes::ARCHIVE, "archive"),
        (FileAttributes::TEMPORARY, "temporary"),
        (FileAttributes::SPARSE, "sparse"),
        (FileAttributes::REPARSE_POINT, "reparse"),
        (FileAttributes::COMPRESSED, "compressed"),
        (FileAttributes::OFFLINE, "offline"),
        (FileAttributes::ENCRYPTED, "encrypted"),
    ];
    for (bit, name) in table {
        if a.has(bit) {
            out.push(name);
        }
    }
    out
}

/// Map an attribute name (as used in queries) to its bit.
pub fn attr_bit(name: &str) -> Option<u32> {
    Some(match name {
        "readonly" | "r" => FileAttributes::READONLY,
        "hidden" | "h" => FileAttributes::HIDDEN,
        "system" | "s" => FileAttributes::SYSTEM,
        "directory" | "d" => FileAttributes::DIRECTORY,
        "archive" | "a" => FileAttributes::ARCHIVE,
        "temporary" | "t" => FileAttributes::TEMPORARY,
        "sparse" | "p" => FileAttributes::SPARSE,
        "reparse" | "l" => FileAttributes::REPARSE_POINT,
        "compressed" | "c" => FileAttributes::COMPRESSED,
        "offline" | "o" => FileAttributes::OFFLINE,
        "encrypted" | "e" => FileAttributes::ENCRYPTED,
        _ => return None,
    })
}

pub fn attr_name(bit: u32) -> Option<&'static str> {
    attr_terms(FileAttributes(bit)).first().copied()
}

/// Build the index document for one projection row.
pub fn document(f: &Fields, row: &ProjectionRow) -> TantivyDocument {
    let mut d = TantivyDocument::new();
    d.add_u64(f.entry_id, row.entry_id as u64);
    d.add_u64(f.object_id, row.object_id.0 as u64);
    d.add_u64(f.source_id, row.source_id.0 as u64);
    d.add_u64(f.parent_id, row.parent_id.map(|p| p.0 as u64).unwrap_or(0));
    for a in &row.ancestors {
        d.add_u64(f.ancestors, a.0 as u64);
    }
    d.add_text(f.name, &row.name);
    d.add_text(f.name_folded, fold(&row.name));
    d.add_text(f.path, &row.path);
    d.add_text(f.path_folded, fold(&row.path));
    d.add_text(f.path_tokens, &row.path);
    d.add_text(f.extension, &row.extension);
    d.add_text(f.kind, row.kind.as_str());
    d.add_text(f.content_state, row.content_state.as_str());
    d.add_u64(f.size, row.size);
    d.add_u64(f.allocated, row.allocated);
    d.add_i64(f.modified, row.modified.map(|t| t.0).unwrap_or(0));
    d.add_i64(f.created, row.created.map(|t| t.0).unwrap_or(0));
    for a in attr_terms(row.attributes) {
        d.add_text(f.attrs, a);
    }
    let is_dir = row.kind == ObjectKind::Directory;
    d.add_u64(f.file_count, if is_dir { row.file_count } else { 0 });
    d.add_u64(f.dir_count, if is_dir { row.dir_count } else { 0 });
    d.add_u64(
        f.subtree_logical,
        if is_dir {
            row.subtree_logical
        } else {
            row.size
        },
    );
    d.add_u64(
        f.subtree_allocated,
        if is_dir {
            row.subtree_allocated
        } else {
            row.allocated
        },
    );
    d.add_i64(
        f.newest_modified,
        if is_dir {
            row.newest_modified.map(|t| t.0).unwrap_or(0)
        } else {
            row.modified.map(|t| t.0).unwrap_or(0)
        },
    );
    d.add_u64(f.agg_complete, if row.agg_complete { 1 } else { 0 });
    for e in &row.desc_extensions {
        d.add_text(f.desc_ext, e);
    }
    d.add_u64(f.generation, row.generation as u64);
    d.add_u64(f.link_count, row.link_count as u64);
    d.add_text(f.name_tri, &row.name);
    d.add_text(f.path_tri, &row.path);
    d
}
