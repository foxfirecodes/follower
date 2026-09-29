use std::{fs, path::Path};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::{
    cache::{FRONTEND_VERSION, SNAPSHOT_SCHEMA_VERSION, Snapshot},
    frontend::parse_and_lower,
    ids::FileId,
    link::ModuleLinker,
    models::{Model, load_model_files},
    project::Project,
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

        let linker = ModuleLinker::new(&self.project.config.resolution_conditions);
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

        let mut hasher = Sha256::new();
        hasher.update(self.project.config_hash.as_bytes());
        hasher.update(FRONTEND_VERSION.as_bytes());
        for file in &files {
            hasher.update(file.path.to_string_lossy().as_bytes());
            hasher.update(file.content_hash.as_bytes());
        }
        let snapshot_id = hex::encode(hasher.finalize());

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
        let snapshot = self.index()?;
        crate::solver::execute_query(&self.project, &snapshot, query, query_hash)
    }
}
