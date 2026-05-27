use crate::{
    graph_export::GraphExport,
    obsidian_export::{write_obsidian_vault, ObsidianSummary},
};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct RefreshSummary {
    pub obsidian: ObsidianSummary,
    pub feature_pages: Vec<PathBuf>,
    pub skipped_feature_pages: Vec<PathBuf>,
}

pub fn refresh_project_exports(
    graph: &GraphExport,
    obsidian_output: &Path,
    _features_dir: &Path,
    _all_features: bool,
) -> Result<RefreshSummary> {
    let obsidian = write_obsidian_vault(obsidian_output, graph)?;

    Ok(RefreshSummary {
        obsidian,
        feature_pages: Vec::new(),
        skipped_feature_pages: Vec::new(),
    })
}
