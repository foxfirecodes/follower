use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use oxc_resolver::{ResolveOptions, Resolver};
use serde::{Deserialize, Serialize};

use crate::{
    cache::Snapshot,
    ids::FileId,
    ir::{FileIr, FlowExport, SourceSpan},
    models::SymbolMatcher,
    project::Project,
};

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

/// Resolves value-level imported bindings to their canonical exported identity.
///
/// This follows named aliases, local re-exports, explicit re-exports, and
/// `export *` chains. Namespace imports are resolved one member at a time.
pub struct SymbolLinker<'a> {
    project: &'a Project,
    snapshot: &'a Snapshot,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct LinkedSymbol {
    pub file_id: FileId,
    pub name: String,
}

impl<'a> SymbolLinker<'a> {
    pub const fn new(project: &'a Project, snapshot: &'a Snapshot) -> Self {
        Self { project, snapshot }
    }

    pub fn imported_binding_matches(
        &self,
        file_id: FileId,
        local: &str,
        matcher: &SymbolMatcher,
    ) -> bool {
        self.resolve_imported_binding(file_id, local)
            .zip(self.resolve_matcher(matcher))
            .is_some_and(|(candidate, expected)| candidate == expected)
    }

    pub fn namespace_member_matches(
        &self,
        file_id: FileId,
        local: &str,
        member: &str,
        matcher: &SymbolMatcher,
    ) -> bool {
        self.resolve_namespace_member(file_id, local, member)
            .zip(self.resolve_matcher(matcher))
            .is_some_and(|(candidate, expected)| candidate == expected)
    }

    pub fn local_declaration_matches(
        &self,
        file_id: FileId,
        name: &str,
        matcher: &SymbolMatcher,
    ) -> bool {
        self.resolve_local_declaration(file_id, name)
            .zip(self.resolve_matcher(matcher))
            .is_some_and(|(candidate, expected)| candidate == expected)
    }

    pub fn resolve_imported_binding(&self, file_id: FileId, local: &str) -> Option<LinkedSymbol> {
        let file = self.file(file_id)?;
        let import = file
            .flow
            .imports
            .iter()
            .find(|import| import.local == local)?;
        if import.type_only || import.imported == "*" {
            return None;
        }
        let resolved = self.resolved_module(file, &import.module)?;
        self.resolve_export(resolved, &import.imported, &mut BTreeSet::new())
    }

    pub fn resolve_namespace_member(
        &self,
        file_id: FileId,
        local: &str,
        member: &str,
    ) -> Option<LinkedSymbol> {
        let file = self.file(file_id)?;
        let import = file
            .flow
            .imports
            .iter()
            .find(|import| import.local == local)?;
        if import.type_only || import.imported != "*" {
            return None;
        }
        let resolved = self.resolved_module(file, &import.module)?;
        self.resolve_export(resolved, member, &mut BTreeSet::new())
    }

    pub fn resolve_local_declaration(&self, file_id: FileId, name: &str) -> Option<LinkedSymbol> {
        self.file(file_id).map(|_| LinkedSymbol {
            file_id,
            name: name.to_owned(),
        })
    }

    fn resolve_matcher(&self, matcher: &SymbolMatcher) -> Option<LinkedSymbol> {
        let path = self
            .project
            .resolve_path(Path::new(&matcher.module))
            .canonicalize()
            .ok()?;
        self.resolve_export(&path, &matcher.export, &mut BTreeSet::new())
    }

    fn resolve_export(
        &self,
        module_path: &Path,
        export_name: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> Option<LinkedSymbol> {
        if !visited.insert((module_path.to_path_buf(), export_name.to_owned())) {
            return None;
        }
        let file = self
            .snapshot
            .files
            .iter()
            .find(|file| file.path == module_path)?;
        for export in &file.flow.exports {
            match export {
                FlowExport::ReExport {
                    imported,
                    exported,
                    module,
                    type_only: false,
                    ..
                } if exported == export_name => {
                    if let Some(symbol) = self.resolve_re_export(file, module, imported, visited) {
                        return Some(symbol);
                    }
                }
                FlowExport::Local {
                    local,
                    exported,
                    type_only: false,
                    ..
                } if exported == export_name => {
                    if let Some(import) = file
                        .flow
                        .imports
                        .iter()
                        .find(|import| import.local == *local)
                    {
                        if import.type_only || import.imported == "*" {
                            return None;
                        }
                        if let Some(resolved) = self.resolved_module(file, &import.module)
                            && let Some(symbol) =
                                self.resolve_export(resolved, &import.imported, visited)
                        {
                            return Some(symbol);
                        }
                        return None;
                    }
                    return Some(LinkedSymbol {
                        file_id: file.file_id,
                        name: local.clone(),
                    });
                }
                FlowExport::Star {
                    module,
                    type_only: false,
                    ..
                } if export_name != "default" => {
                    if let Some(symbol) = self.resolve_re_export(file, module, export_name, visited)
                    {
                        return Some(symbol);
                    }
                }
                FlowExport::Local { .. }
                | FlowExport::ReExport { .. }
                | FlowExport::Star { .. }
                | FlowExport::Namespace { .. } => {}
            }
        }
        None
    }

    fn resolve_re_export(
        &self,
        importer: &FileIr,
        module: &str,
        export_name: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> Option<LinkedSymbol> {
        self.resolved_module(importer, module)
            .and_then(|resolved| self.resolve_export(resolved, export_name, visited))
    }

    fn resolved_module<'b>(&'b self, importer: &FileIr, module: &str) -> Option<&'b Path> {
        self.snapshot
            .resolutions
            .iter()
            .find(|resolution| {
                resolution.importer == importer.path && resolution.specifier == module
            })
            .and_then(|resolution| resolution.resolved_path.as_deref())
    }

    fn file(&self, file_id: FileId) -> Option<&FileIr> {
        self.snapshot
            .files
            .iter()
            .find(|file| file.file_id == file_id)
    }
}
