use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::{
    cache::{FRONTEND_VERSION, SNAPSHOT_SCHEMA_VERSION, Snapshot},
    frontend::parse_and_lower,
    ids::FileId,
    link::ModuleLinker,
    models::{Model, load_model_files},
    project::{Project, is_source_file},
    queries::AuditReport,
    query::{QueryReport, QuerySpec},
};

pub struct Analyzer {
    project: Project,
}

impl Analyzer {
    pub const fn new(project: Project) -> Self {
        Self { project }
    }

    pub const fn project(&self) -> &Project {
        &self.project
    }

    pub fn index(&self) -> Result<Snapshot> {
        let source_paths = self.project.discover_sources()?;
        let mut files = Vec::with_capacity(source_paths.len());
        for (index, path) in source_paths.iter().enumerate() {
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            files.push(parse_and_lower(
                FileId(u32::try_from(index).unwrap_or(u32::MAX)),
                path,
                &source,
            )?);
        }

        let linker = ModuleLinker::new(&self.project);
        let mut resolutions = Vec::new();
        for file in &files {
            for import in &file.imports {
                resolutions.push(linker.resolve(
                    &file.path,
                    &import.specifier,
                    import.span.clone(),
                    import.type_only,
                ));
            }
        }

        let snapshot_id = self.snapshot_id(&files);

        Ok(Snapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            frontend_version: FRONTEND_VERSION.to_owned(),
            snapshot_id,
            project_name: self.project.config.name.clone(),
            config_hash: self.project.config_hash.clone(),
            files,
            resolutions,
        })
    }

    pub fn default_snapshot_path(&self) -> std::path::PathBuf {
        self.project.root.join(Path::new(".flow/snapshot.json"))
    }

    pub fn audit(&self, model_id: &str) -> Result<AuditReport> {
        let snapshot = self.index()?;
        let models = load_model_files(&self.project.root, &self.project.config.model_files)?;
        let model = models
            .find(model_id)
            .with_context(|| format!("configured model not found: {model_id}"))?;
        match model {
            Model::CallbackFactory(factory) => {
                crate::solver::audit(&self.project, &snapshot, factory, &models.content_hash)
            }
        }
    }

    pub fn query(&self, query: &QuerySpec, query_hash: &str) -> Result<QueryReport> {
        query.validate()?;
        let mut snapshot = self.index()?;
        let mut followed = 0;
        let mut skipped = BTreeSet::new();
        for round in 0..=8 {
            let (mut report, requests) =
                crate::solver::execute_query(&self.project, &snapshot, query, query_hash)?;
            if self.project.config.source_contains_any.is_empty() {
                return Ok(report);
            }
            let indexed = snapshot
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<BTreeSet<_>>();
            let mut additions = Vec::new();
            for path in requests.difference(&indexed) {
                if path.is_file()
                    && is_source_file(path)
                    && !path
                        .components()
                        .any(|component| component.as_os_str() == OsStr::new("node_modules"))
                {
                    additions.push(path.clone());
                } else {
                    skipped.insert(path.clone());
                }
            }
            if additions.is_empty() || round == 8 || followed + additions.len() > 256 {
                if !additions.is_empty() {
                    report.coverage.gaps.push(format!(
                        "capability import expansion stopped after {followed} files and {round} rounds; {} requested files remain",
                        additions.len()
                    ));
                }
                if !skipped.is_empty() {
                    report.coverage.gaps.push(format!(
                        "{} capability imports were outside supported source files or node_modules",
                        skipped.len()
                    ));
                }
                if !report.coverage.gaps.is_empty() {
                    report.coverage.complete = false;
                }
                return Ok(report);
            }
            followed += additions.len();
            self.extend_snapshot(&mut snapshot, &additions)?;
        }
        unreachable!("bounded import expansion always returns")
    }

    fn extend_snapshot(&self, snapshot: &mut Snapshot, paths: &[PathBuf]) -> Result<()> {
        let linker = ModuleLinker::new(&self.project);
        for path in paths {
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let file_id =
                FileId(u32::try_from(snapshot.files.len()).context("too many source files")?);
            let file = parse_and_lower(file_id, path, &source)?;
            for import in &file.imports {
                snapshot.resolutions.push(linker.resolve(
                    &file.path,
                    &import.specifier,
                    import.span.clone(),
                    import.type_only,
                ));
            }
            snapshot.files.push(file);
        }
        snapshot.snapshot_id = self.snapshot_id(&snapshot.files);
        Ok(())
    }

    fn snapshot_id(&self, files: &[crate::ir::FileIr]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.project.config_hash.as_bytes());
        hasher.update(FRONTEND_VERSION.as_bytes());
        for file in files {
            hasher.update(file.path.to_string_lossy().as_bytes());
            hasher.update(file.content_hash.as_bytes());
        }
        hex::encode(hasher.finalize())
    }
}
