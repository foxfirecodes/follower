use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFile {
    pub schema_version: u32,
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Model {
    CallbackFactory(CallbackFactoryModel),
}

impl Model {
    pub fn id(&self) -> &str {
        match self {
            Self::CallbackFactory(model) => &model.id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackFactoryModel {
    pub id: String,
    pub r#match: SymbolMatcher,
    pub returned_property: Vec<String>,
    pub retains_returned_callback: Option<bool>,
    pub invokes_returned_callback_during_call: Option<bool>,
    pub captures: std::collections::BTreeMap<String, CaptureSource>,
    pub on_invoke: Vec<ModeledOperation>,
    pub evidence: ModelEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolMatcher {
    pub project: String,
    pub module: String,
    pub export: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureSource {
    Argument { index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModeledOperation {
    Effect {
        name: String,
        arguments: Vec<ModelValue>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelValue {
    Capture { name: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEvidence {
    pub kind: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct LoadedModels {
    pub models: Vec<Model>,
    pub content_hash: String,
}

impl LoadedModels {
    pub fn find(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id() == id)
    }
}

pub fn load_model_files(root: &Path, files: &[std::path::PathBuf]) -> Result<LoadedModels> {
    let mut models = Vec::new();
    let mut hasher = Sha256::new();
    for configured_path in files {
        let path = if configured_path.is_absolute() {
            configured_path.clone()
        } else {
            root.join(configured_path)
        };
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read model file {}", path.display()))?;
        hasher.update(&bytes);
        let file: ModelFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse model file {}", path.display()))?;
        if file.schema_version != 1 {
            bail!(
                "unsupported model schema version {} in {}",
                file.schema_version,
                path.display()
            );
        }
        models.extend(file.models);
    }
    validate_models(&models)?;
    Ok(LoadedModels {
        models,
        content_hash: hex::encode(hasher.finalize()),
    })
}

fn validate_models(models: &[Model]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for model in models {
        if model.id().trim().is_empty() {
            bail!("model id cannot be empty");
        }
        if !ids.insert(model.id()) {
            bail!("duplicate model id: {}", model.id());
        }
        match model {
            Model::CallbackFactory(factory) => {
                if factory.returned_property.is_empty() {
                    bail!(
                        "callback factory {} has an empty returned_property",
                        factory.id
                    );
                }
                for operation in &factory.on_invoke {
                    let ModeledOperation::Effect { arguments, .. } = operation;
                    for argument in arguments {
                        let ModelValue::Capture { name } = argument;
                        if !factory.captures.contains_key(name) {
                            bail!(
                                "callback factory {} references unknown capture {name}",
                                factory.id
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
