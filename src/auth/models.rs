use super::{Provider, Store, read_yaml, save_yaml};
use crate::models::{ModelThinking, Protocol, ValidatedModel, ValidatedProvider};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    pub(crate) providers: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub(crate) auth: Provider,
    pub(crate) generation: String,
    pub(crate) protocol: Protocol,
    pub(crate) models: BTreeMap<String, Model>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Model {
    pub(crate) model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) protocol: Option<Protocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_window: Option<u64>,
}

impl Catalog {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let catalog: Self = read_yaml(path, "subscription models")?.unwrap_or_default();
        for (id, entry) in &catalog.providers {
            ensure!(
                id == entry.auth.id() && !entry.generation.is_empty(),
                "订阅模型来源无效"
            );
            ensure!(entry.protocol == entry.auth.protocol(), "订阅模型协议无效");
            ensure!(!entry.models.is_empty(), "订阅模型列表为空");
            for (id, model) in &entry.models {
                ensure!(
                    entry.auth == Provider::GithubCopilot
                        || model
                            .protocol
                            .is_none_or(|protocol| protocol == entry.protocol),
                    "订阅模型协议无效"
                );
                ensure!(
                    !id.trim().is_empty()
                        && !id.chars().any(char::is_control)
                        && !model.model.trim().is_empty()
                        && !model.model.chars().any(char::is_control)
                        && model.context_window.is_none_or(|size| size > 0),
                    "订阅模型配置无效"
                );
            }
        }
        Ok(catalog)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        save_yaml(path, self)
    }

    pub(crate) fn validated(self, store: &Store) -> Vec<ValidatedProvider> {
        self.providers
            .into_iter()
            .flat_map(|(id, entry)| {
                let Some(credential) = store.credentials.get(&entry.auth) else {
                    return Vec::new();
                };
                if entry.generation != credential.generation {
                    return Vec::new();
                }
                // 复用同名供应商分组，让模型选择与请求路由沿用统一的协议解析。
                let mut providers: Vec<ValidatedProvider> = Vec::new();
                for (model_id, model) in entry.models {
                    let protocol = model.protocol.unwrap_or(entry.protocol);
                    let index = match providers.iter().position(|item| item.protocol == protocol) {
                        Some(index) => index,
                        None => {
                            providers.push(ValidatedProvider {
                                id: id.clone(),
                                protocol,
                                api_key: String::new(),
                                auth: Some(entry.auth),
                                base_url: None,
                                models: Vec::new(),
                            });
                            providers.len() - 1
                        }
                    };
                    providers[index].models.push(ValidatedModel {
                        id: model_id,
                        model: model.model,
                        cost: None,
                        context_window: model.context_window,
                        thinking: if matches!(
                            entry.auth,
                            Provider::KimiCode | Provider::Glm | Provider::Zai
                        ) {
                            ModelThinking::without_reasoning()
                        } else {
                            ModelThinking::default()
                        },
                    });
                }
                providers
            })
            .collect()
    }
}
