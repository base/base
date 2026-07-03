use std::path::PathBuf;

use tree_sitter::Tree;

use crate::languages::Language;

pub struct FileContext {
    pub path: PathBuf,
    pub language: Language,
    pub code: String,
    pub tree: Tree,
    pub content_hash: u64,
}
