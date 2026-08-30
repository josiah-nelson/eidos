//! Test-driven export of the Rust-owned HTTP contract.

use std::any::TypeId;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use ts_rs::{Config, TypeVisitor, TS};

struct ContractTypes {
    config: Config,
    seen: HashSet<TypeId>,
    declarations: BTreeMap<String, String>,
}

fn compact_declaration(mut source: &str) -> String {
    let mut without_docs = String::with_capacity(source.len());
    while let Some(start) = source.find("/**") {
        without_docs.push_str(&source[..start]);
        let rest = &source[start + 3..];
        source = rest
            .find("*/")
            .map(|end| &rest[end + 2..])
            .expect("ts-rs emitted an unterminated documentation comment");
    }
    without_docs.push_str(source);
    without_docs
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl ContractTypes {
    fn new() -> Self {
        Self {
            config: Config::default().with_large_int("ApiInt"),
            seen: HashSet::new(),
            declarations: BTreeMap::new(),
        }
    }

    fn add<T: TS + 'static>(&mut self) {
        <Self as TypeVisitor>::visit::<T>(self);
    }

    fn finish(self) -> String {
        let mut output = String::from(
            "// This file is generated from Rust by `cargo test -p eidos-service export_api_contract`.\n\
             // Do not edit it by hand. See docs/development.md for compatibility policy.\n\n\
             /** Exact decimal JSON string used for every Rust i64/u64. */\n\
             export type ApiInt = string;\n\n",
        );
        for declaration in self.declarations.into_values() {
            output.push_str("export ");
            output.push_str(&declaration);
            output.push_str("\n\n");
        }
        output.pop();
        output
    }
}

impl TypeVisitor for ContractTypes {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        if !self.seen.insert(TypeId::of::<T>()) || T::output_path().is_none() {
            return;
        }
        let name = T::ident(&self.config);
        let declaration = compact_declaration(&T::decl(&self.config));
        if let Some(previous) = self.declarations.insert(name.clone(), declaration.clone()) {
            assert_eq!(
                previous, declaration,
                "duplicate TypeScript type name {name}"
            );
        }
        T::visit_dependencies(self);
    }
}

fn typescript_contract() -> String {
    let mut types = ContractTypes::new();

    // Endpoint request and response roots. Dependencies are discovered
    // recursively, so adding a Rust-owned field pulls its type into the same
    // checked-in contract without another handwritten TypeScript mirror.
    types.add::<crate::api::ApiErrorBody>();
    types.add::<crate::api::Health>();
    types.add::<crate::api::SourceView>();
    types.add::<crate::api::AddSourceBody>();
    types.add::<crate::api::SourceDetail>();
    types.add::<crate::api::ErrorsQuery>();
    types.add::<eidos_catalog::ErrorRecord>();
    types.add::<crate::api::ObjectDetail>();
    types.add::<crate::api::ChildrenQuery>();
    types.add::<crate::api::ChildrenView>();
    types.add::<crate::api::ArchiveQuery>();
    types.add::<crate::api::ArchiveView>();
    types.add::<crate::api::RequeueView>();
    types.add::<crate::api::LimitQuery>();
    types.add::<eidos_catalog::ExtensionCount>();
    types.add::<crate::api::ResolveQuery>();
    types.add::<crate::api::ResolveView>();
    types.add::<crate::api::SearchBody>();
    types.add::<crate::api::SearchView>();
    types.add::<crate::api::SearchGetQuery>();
    types.add::<crate::api::ParseQuery>();
    types.add::<crate::api::ParseView>();
    types.add::<crate::api::ContentPolicyBody>();
    types.add::<crate::content_control::ContentStatusView>();
    types.add::<crate::api::ActivityView>();
    types.add::<crate::api::IndexStatus>();
    types.add::<crate::content_preview::PreviewQuery>();
    types.add::<crate::content_preview::PreviewView>();
    types.add::<crate::interactions_api::InteractionBatch>();
    types.add::<crate::interactions_api::InteractionAck>();
    types.add::<crate::retry_api::RetryBody>();
    types.add::<eidos_catalog::retry::RetryReport>();
    types.add::<crate::export::ExportGetQuery>();
    types.add::<crate::export::ExportBody>();
    types.add::<crate::export::ExportDocument>();
    types.add::<crate::export::ExportNdjsonHeader>();
    types.add::<crate::export::ExportNdjsonSummary>();
    types.add::<eidos_fleet::FleetStatus>();
    types.add::<eidos_fleet::FleetConfig>();
    types.add::<crate::fleet_api::CentralBody>();
    types.add::<crate::fleet_api::InviteBody>();
    types.add::<crate::fleet_api::InviteView>();
    types.add::<crate::fleet_api::EnrollBody>();
    types.add::<crate::fleet_api::EnrollView>();
    types.add::<crate::fleet_api::SyncBody>();
    types.add::<crate::fleet_api::PeerBody>();
    types.add::<crate::fleet_api::ForgetView>();
    types.add::<crate::fleet_api::SyncPolicyBody>();

    types.finish()
}

#[test]
fn export_api_contract() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/src/generated/api.ts");
    let generated = typescript_contract();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(&generated) {
        std::fs::create_dir_all(path.parent().expect("generated contract parent")).unwrap();
        std::fs::write(path, generated).unwrap();
    }
}
