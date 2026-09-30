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
    ir::{FlowExport, SourceSpan},
    link::ModuleLinker,
    models::{Model, load_model_files},
    project::{Project, is_source_file},
    queries::AuditReport,
    query::{QueryReport, QuerySpec},
};

pub struct Analyzer {
    project: Project,
}

struct SourceCatalog {
    imports: Vec<(PathBuf, Vec<String>)>,
}

impl SourceCatalog {
    fn build(project: &Project) -> Result<Self> {
        let mut imports = Vec::new();
        for path in project.discover_all_sources()? {
            if path
                .components()
                .any(|part| part.as_os_str() == OsStr::new("node_modules"))
            {
                continue;
            }
            let source = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let specifiers = module_specifiers(&source);
            if !specifiers.is_empty() {
                imports.push((path, specifiers));
            }
        }
        Ok(Self { imports })
    }

    fn reverse_importers(
        &self,
        project: &Project,
        targets: &BTreeSet<PathBuf>,
    ) -> BTreeSet<PathBuf> {
        let names = targets
            .iter()
            .flat_map(|path| {
                let mut names = Vec::new();
                if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                    names.push(stem.to_owned());
                    if stem == "index" {
                        if let Some(parent) = path
                            .parent()
                            .and_then(Path::file_name)
                            .and_then(|name| name.to_str())
                        {
                            names.push(parent.to_owned());
                        }
                    }
                }
                names
            })
            .collect::<BTreeSet<_>>();
        let linker = ModuleLinker::new(project);
        let mut result = BTreeSet::new();
        for (importer, specifiers) in &self.imports {
            if targets.contains(importer) {
                continue;
            }
            for specifier in specifiers {
                let stem = Path::new(specifier)
                    .file_stem()
                    .and_then(|name| name.to_str());
                if !stem.is_some_and(|stem| names.contains(stem)) {
                    continue;
                }
                let resolution = linker.resolve(
                    importer,
                    specifier,
                    SourceSpan {
                        file_id: FileId(0),
                        start: 0,
                        end: 0,
                    },
                    false,
                );
                if resolution
                    .resolved_path
                    .as_ref()
                    .is_some_and(|path| targets.contains(path))
                {
                    result.insert(importer.clone());
                    break;
                }
            }
        }
        result
    }
}

fn module_specifiers(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut specs = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        let keyword = [
            b"from".as_slice(),
            b"import".as_slice(),
            b"require".as_slice(),
        ]
        .into_iter()
        .find(|keyword| {
            bytes[index..].starts_with(keyword)
                && (index == 0
                    || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_')
                && bytes
                    .get(index + keyword.len())
                    .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
        });
        let Some(keyword) = keyword else {
            index += 1;
            continue;
        };
        let mut next = index + keyword.len();
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        if keyword == b"require" && bytes.get(next) == Some(&b'(') {
            next += 1;
            while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
        }
        if let Some(quote @ (b'\'' | b'"')) = bytes.get(next).copied() {
            let start = next + 1;
            next = start;
            while next < bytes.len() && bytes[next] != quote {
                next += 1;
            }
            if next < bytes.len() {
                specs.insert(source[start..next].to_owned());
            }
        }
        index += keyword.len();
    }
    specs.into_iter().collect()
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
        let catalog = if self.project.config.source_contains_any.is_empty() {
            None
        } else {
            Some(SourceCatalog::build(&self.project)?)
        };
        let mut followed = 0;
        let mut skipped = BTreeSet::new();
        let mut reverse_seed_paths = BTreeSet::new();
        let mut reverse_producer_paths = BTreeSet::new();
        for round in 0..=8 {
            let (mut report, requests, producers) = crate::solver::execute_query(
                &self.project,
                &snapshot,
                query,
                query_hash,
                &reverse_seed_paths,
                &reverse_producer_paths,
            )?;
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query round {round}: indexed={} requested={} producers={} reverse_seeds={}",
                    snapshot.files.len(),
                    requests.len(),
                    producers.len(),
                    reverse_seed_paths.len()
                );
            }
            if self.project.config.source_contains_any.is_empty() {
                return Ok(report);
            }
            reverse_producer_paths.extend(producers);
            let reverse_requests = catalog.as_ref().map_or_else(BTreeSet::new, |catalog| {
                catalog.reverse_importers(&self.project, &reverse_producer_paths)
            });
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query round {round}: reverse_candidates={}",
                    reverse_requests.len()
                );
            }
            let indexed = snapshot
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<BTreeSet<_>>();
            let mut additions = Vec::new();
            for path in requests
                .union(&reverse_requests)
                .filter(|path| !indexed.contains(*path))
            {
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
            reverse_seed_paths.extend(
                additions
                    .iter()
                    .filter(|path| reverse_requests.contains(*path))
                    .cloned(),
            );
            self.extend_snapshot(&mut snapshot, &additions)?;
            for path in additions
                .iter()
                .filter(|path| reverse_requests.contains(*path))
            {
                let Some(file) = snapshot.files.iter().find(|file| &file.path == path) else {
                    continue;
                };
                let forwards = file.flow.exports.iter().any(|export| {
                    let module = match export {
                        FlowExport::ReExport { module, .. }
                        | FlowExport::Star { module, .. }
                        | FlowExport::Namespace { module, .. } => Some(module.as_str()),
                        FlowExport::Local { local, .. } => file
                            .flow
                            .imports
                            .iter()
                            .find(|import| import.local == *local)
                            .map(|import| import.module.as_str()),
                    };
                    module.is_some_and(|module| {
                        snapshot.resolutions.iter().any(|resolution| {
                            resolution.importer == file.path
                                && resolution.specifier == module
                                && resolution
                                    .resolved_path
                                    .as_ref()
                                    .is_some_and(|target| reverse_producer_paths.contains(target))
                        })
                    })
                });
                if forwards {
                    reverse_producer_paths.insert(path.clone());
                }
            }
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
