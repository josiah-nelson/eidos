//! Archive member inventories from container metadata, never from member
//! data: v0.5 reads the ZIP central directory (ADR-0010). Members become
//! virtual entries beneath their container in the catalog.

pub mod fixture;
pub mod zip;

pub use eidos_domain::archive::{archive_format, ArchiveFormat};
pub use zip::{inventory, inventory_reader, ArchiveError, ArchiveLimits, Inventory, Member};
