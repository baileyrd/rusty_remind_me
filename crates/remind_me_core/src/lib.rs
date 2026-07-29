pub mod backup;
pub mod db;
pub mod entity;
pub mod fts;
pub mod models;
pub mod normalize;
pub mod retrieval;
pub mod stats;
pub mod vitality;
pub mod wiki;
pub mod wiki_import;

pub use db::Database;
pub use entity::*;
pub use models::*;
pub use wiki::*;
// Deliberately not glob-re-exported: `wiki_import::slugify` would collide at
// the crate root with anything `wiki::*` grows later. Use `wiki_import::…`.
