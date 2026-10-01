use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub module: PathBuf,
    pub export: String,
}

/// An imported function that returns the result of calling one callback argument.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackSelectorImport {
    pub module: String,
    pub export: String,
    pub callback_argument: usize,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    pub source_roots: Vec<PathBuf>,
    /// Text prefilter for directory roots. Explicit file roots are always indexed.
    #[serde(default)]
    pub source_contains_any: Vec<String>,
    #[serde(default)]
    pub entries: Vec<EntryPoint>,
    #[serde(default)]
    pub model_files: Vec<PathBuf>,
    #[serde(default)]
    pub resolution_conditions: Vec<String>,
    #[serde(default)]
    pub import_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub callback_selector_imports: Vec<CallbackSelectorImport>,
    #[serde(default)]
    pub inputs: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub config_path: PathBuf,
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub config_hash: String,
}

impl Project {
    pub fn load(config_path: impl AsRef<Path>) -> Result<Self> {
        let config_path = config_path.as_ref().canonicalize().with_context(|| {
            format!(
                "failed to locate project configuration {}",
                config_path.as_ref().display()
            )
        })?;
        let root = config_path
            .parent()
            .context("project configuration has no parent directory")?
            .to_path_buf();
        let source = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let config: ProjectConfig = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        if config.schema_version != 1 {
            bail!(
                "unsupported project schema version {}; expected 1",
                config.schema_version
            );
        }
        if config.source_roots.is_empty() {
            bail!("project must configure at least one source root");
        }
        if config
            .source_contains_any
            .iter()
            .any(|term| term.is_empty())
        {
            bail!("source_contains_any terms cannot be empty");
        }
        for (alias, target) in &config.import_aliases {
            if alias.is_empty()
                || target.is_empty()
                || alias.matches('*').count() > 1
                || target.matches('*').count() > 1
            {
                bail!("invalid import alias {alias:?} -> {target:?}");
            }
            if alias.contains('*') != target.contains('*') {
                bail!(
                    "import alias {alias:?} and target {target:?} must both use * or neither use it"
                );
            }
        }
        for selector in &config.callback_selector_imports {
            if selector.module.trim().is_empty() || selector.export.trim().is_empty() {
                bail!("callback selector imports need a module and export name");
            }
        }
        let config_hash = hex::encode(Sha256::digest(source.as_bytes()));
        Ok(Self {
            config_path,
            root,
            config,
            config_hash,
        })
    }

    pub fn resolve_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    pub fn discover_sources(&self) -> Result<Vec<PathBuf>> {
        self.discover_sources_with_filter(true)
    }

    pub fn discover_all_sources(&self) -> Result<Vec<PathBuf>> {
        self.discover_sources_with_filter(false)
    }

    fn discover_sources_with_filter(&self, use_text_filter: bool) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for configured_root in &self.config.source_roots {
            let root = self.resolve_path(configured_root);
            if !root.exists() {
                bail!("source root does not exist: {}", root.display());
            }
            let root = root.canonicalize().with_context(|| {
                format!("failed to canonicalize source root {}", root.display())
            })?;
            if root.is_file() {
                if !is_source_file(&root) {
                    bail!(
                        "source root is not a supported source file: {}",
                        root.display()
                    );
                }
                files.push(root);
                continue;
            }
            for entry in WalkDir::new(&root).follow_links(false) {
                let entry =
                    entry.with_context(|| format!("failed while walking {}", root.display()))?;
                if entry.file_type().is_file() && is_source_file(entry.path()) {
                    if use_text_filter && !self.config.source_contains_any.is_empty() {
                        let source = fs::read_to_string(entry.path()).with_context(|| {
                            format!("failed to read {}", entry.path().display())
                        })?;
                        if !self
                            .config
                            .source_contains_any
                            .iter()
                            .any(|term| source.contains(term))
                        {
                            continue;
                        }
                    }
                    files.push(entry.path().to_path_buf());
                }
            }
        }
        for entry in &self.config.entries {
            let path = self.resolve_path(&entry.module);
            if !is_source_file(&path) {
                bail!(
                    "entry module is not a supported source file: {}",
                    path.display()
                );
            }
            files
                .push(path.canonicalize().with_context(|| {
                    format!("failed to locate entry module {}", path.display())
                })?);
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}

pub(crate) fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    ) && !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".d.ts"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_source_file;

    #[test]
    fn source_filter_excludes_declaration_files() {
        assert!(is_source_file(Path::new("Host.tsx")));
        assert!(!is_source_file(Path::new("types.d.ts")));
        assert!(!is_source_file(Path::new("package.json")));
    }
}
