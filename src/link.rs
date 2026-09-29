use std::path::{Path, PathBuf};

use oxc_resolver::{ResolveOptions, Resolver};
use serde::{Deserialize, Serialize};

use crate::ir::SourceSpan;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Resolved,
    Unresolved,
    TypeOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportResolution {
    pub importer: PathBuf,
    pub specifier: String,
    pub span: SourceSpan,
    pub status: ResolutionStatus,
    pub resolved_path: Option<PathBuf>,
    pub diagnostic: Option<String>,
}

pub struct ModuleLinker {
    resolver: Resolver,
}

impl ModuleLinker {
    pub fn new(conditions: &[String]) -> Self {
        let options = ResolveOptions {
            extensions: vec![
                ".ts".into(),
                ".tsx".into(),
                ".mts".into(),
                ".cts".into(),
                ".js".into(),
                ".jsx".into(),
                ".mjs".into(),
                ".cjs".into(),
                ".json".into(),
            ],
            condition_names: conditions.to_vec(),
            ..ResolveOptions::default()
        };
        Self {
            resolver: Resolver::new(options),
        }
    }

    pub fn resolve(
        &self,
        importer: &Path,
        specifier: &str,
        span: SourceSpan,
        type_only: bool,
    ) -> ImportResolution {
        if type_only {
            return ImportResolution {
                importer: importer.to_path_buf(),
                specifier: specifier.to_owned(),
                span,
                status: ResolutionStatus::TypeOnly,
                resolved_path: None,
                diagnostic: None,
            };
        }
        match self.resolver.resolve_file(importer, specifier) {
            Ok(resolution) => ImportResolution {
                importer: importer.to_path_buf(),
                specifier: specifier.to_owned(),
                span,
                status: ResolutionStatus::Resolved,
                resolved_path: Some(resolution.into_path_buf()),
                diagnostic: None,
            },
            Err(error) => ImportResolution {
                importer: importer.to_path_buf(),
                specifier: specifier.to_owned(),
                span,
                status: ResolutionStatus::Unresolved,
                resolved_path: None,
                diagnostic: Some(error.to_string()),
            },
        }
    }
}
