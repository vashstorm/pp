use super::*;

#[test]
fn auth_store_locking_and_yaml_legacy_migration() {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let mut store = Store::default();
        store
            .credentials
            .insert(Provider::Glm, Credential::api_key("secret-glm".into()));
        store.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut loaded = Store::load(&path).unwrap();
        loaded.credentials.insert(
            Provider::GithubCopilot,
            Credential::api_key("secret-github".into()),
        );
        loaded.save(&path).unwrap();
        let mut loaded = Store::load(&path).unwrap();
        loaded.credentials.remove(&Provider::Glm);
        loaded.save(&path).unwrap();
        assert_eq!(
            Store::load(&path).unwrap().credentials[&Provider::GithubCopilot].access_token,
            "secret-github"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !Store::load(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("secret")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let symlink = dir.path().join("link.json");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        assert!(Store::load(&symlink).is_err());
        assert!(Store::load(dir.path()).is_err());
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let cancellation = CancellationToken::new();
        let lock = StoreLock::acquire(&path, &cancellation).unwrap();
        let waiting = cancellation.clone();
        let other_path = path.clone();
        let handle =
            std::thread::spawn(move || StoreLock::acquire(&other_path, &waiting).err().unwrap());
        cancellation.cancel();
        assert!(handle.join().unwrap().is::<Cancelled>());
        drop(lock);
        assert!(StoreLock::acquire(&path, &CancellationToken::new()).is_ok());
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.yml");
        for text in [
            "credentials: {}\ncredentials: {}",
            "credentials:\n  glm:\n    access_token: [secret-marker]",
        ] {
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let error = load_legacy_store(&path).err().unwrap();
            assert!(!format!("{error:#}").contains("secret-marker"));
        }
        let link = dir.path().join("symlink.yml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Store::default().save(&link).is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(
            &path,
            "unknown: true\ncredentials:\n  glm:\n    generation: gen-1\n    access_token: secret-marker\n    expires_at: 5\n    extra: [1]\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let legacy = load_legacy_store(&path).unwrap();
        assert_eq!(legacy.credentials[&Provider::Glm].generation, "gen-1");
        assert_eq!(
            legacy.credentials[&Provider::Glm].access_token,
            "secret-marker"
        );
        assert_eq!(legacy.credentials[&Provider::Glm].expires_at, Some(5));
        assert!(
            load_legacy_store(&dir.path().join("absent-auth.yml"))
                .unwrap()
                .credentials
                .is_empty()
        );
    }
}

fn subscription(dir: &Path, provider: Provider, ids: &[&str]) {
    let store = Store::load(&dir.join("agent.db")).unwrap();
    let mut catalog = models::Catalog::load(&dir.join("models-sub.yml")).unwrap();
    catalog.providers.insert(
        provider.id().into(),
        models::Entry {
            auth: provider,
            generation: store.credentials[&provider].generation.clone(),
            protocol: provider.protocol(),
            models: ids
                .iter()
                .map(|id| {
                    (
                        (*id).into(),
                        models::Model {
                            model: (*id).into(),
                            protocol: None,
                            context_window: None,
                        },
                    )
                })
                .collect(),
        },
    );
    catalog.save(&dir.join("models-sub.yml")).unwrap();
}

#[test]
fn auth_catalog_bootstrap_merge_and_logout() {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let cancellation = CancellationToken::new();
        let settings = dir.path().join("settings.yml");
        let models = dir.path().join("models.yml");
        fs::write(&settings, "think_level: high\n").unwrap();
        super::service::commit_login(
            dir.path(),
            Provider::Glm,
            Credential::api_key("secret".into()),
            &cancellation,
        )
        .unwrap();
        let store = Store::load(&path).unwrap();
        let mut sub = models::Catalog::default();
        sub.providers.insert(
            "glm".into(),
            models::Entry {
                auth: Provider::Glm,
                generation: store.credentials[&Provider::Glm].generation.clone(),
                protocol: Provider::Glm.protocol(),
                models: BTreeMap::from([(
                    "fixture".into(),
                    models::Model {
                        model: "fixture".into(),
                        protocol: None,
                        context_window: Some(64000),
                    },
                )]),
            },
        );
        sub.save(&dir.path().join("models-sub.yml")).unwrap();
        let catalog = crate::config::ModelCatalog::from_files(&settings, &models).unwrap();
        let config = catalog.resolve(catalog.default_reference()).unwrap();
        assert_eq!(config.auth.unwrap().provider, Provider::Glm);
        assert_eq!(config.base_url.as_str(), Provider::Glm.base_url());
        assert_eq!(
            config
                .thinking
                .effective_default(catalog.default_think_level()),
            pp_core::model::ThinkLevel::None
        );
        fs::write(
            &models,
            "providers:\n  mine:\n    auth: glm\n    models:\n      main:\n        model: glm-custom\n",
        )
        .unwrap();
        let catalog = crate::config::ModelCatalog::from_files(&settings, &models).unwrap();
        assert_eq!(catalog.default_reference(), "mine/main");
        assert_eq!(catalog.resolve("mine/main").unwrap().model, "glm-custom");
    }

    {
        let dir = tempfile::tempdir().unwrap();
        service::commit_login(
            dir.path(),
            Provider::Glm,
            Credential::api_key("subscription-private".into()),
            &CancellationToken::new(),
        )
        .unwrap();
        subscription(dir.path(), Provider::Glm, &["duplicate", "sub-only"]);
        fs::write(dir.path().join("models.yml"), "providers:\n  glm:\n    api_key: manual-private\n    protocol: response\n    base_url: https://example.com/v1\n    models:\n      duplicate:\n        model: manual-model\n      manual-only:\n        model: manual-only\n").unwrap();
        let mut catalog = crate::config::ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        assert_eq!(catalog.choices().len(), 3);
        let manual = catalog.resolve("glm/duplicate").unwrap();
        assert!(manual.auth.is_none());
        assert_eq!(manual.api_key, "manual-private");
        assert_eq!(manual.model, "manual-model");
        assert_eq!(manual.protocol, crate::models::Protocol::Response);
        let sub = catalog.resolve("glm/sub-only").unwrap();
        assert_eq!(sub.auth.unwrap().provider, Provider::Glm);
        assert_eq!(sub.protocol, crate::models::Protocol::Completion);
        assert!(sub.api_key.is_empty());
        catalog.save_default_model("glm/sub-only").unwrap();
        fs::write(dir.path().join("settings.yml"), "debug: true\n").unwrap();
        catalog.reload_models().unwrap();
        assert_eq!(catalog.default_reference(), "glm/sub-only");
        service::logout(dir.path(), Provider::Glm, &CancellationToken::new()).unwrap();
        catalog.reload_models().unwrap();
        assert_eq!(catalog.choices().len(), 2);
        assert!(catalog.resolve("glm/sub-only").is_err());
        assert_eq!(
            catalog.resolve("glm/duplicate").unwrap().api_key,
            "manual-private"
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        service::commit_login(
            dir.path(),
            Provider::Glm,
            Credential::api_key("first-private".into()),
            &token,
        )
        .unwrap();
        subscription(dir.path(), Provider::Glm, &["discovered"]);
        let cached = || {
            models::Catalog::load(&dir.path().join("models-sub.yml"))
                .unwrap()
                .validated(&Store::load(&dir.path().join("agent.db")).unwrap())
        };
        service::commit_login(
            dir.path(),
            Provider::Glm,
            Credential::api_key("first-private".into()),
            &token,
        )
        .unwrap();
        assert_eq!(cached().len(), 1);
        service::commit_login(
            dir.path(),
            Provider::Glm,
            Credential::api_key("second-private".into()),
            &token,
        )
        .unwrap();
        assert!(cached().is_empty());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            service::commit_login(
                dir.path(),
                Provider::Glm,
                Credential::api_key("third-private".into()),
                &cancelled
            )
            .is_err()
        );
        assert_eq!(
            Store::load(&dir.path().join("agent.db"))
                .unwrap()
                .credentials[&Provider::Glm]
                .access_token,
            "second-private"
        );
        assert!(
            crate::config::ModelCatalog::from_files(
                &dir.path().join("settings.yml"),
                &dir.path().join("models.yml")
            )
            .is_err()
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        for provider in [Provider::Glm, Provider::Zai] {
            service::commit_login(
                dir.path(),
                provider,
                Credential::api_key("private-key".into()),
                &token,
            )
            .unwrap();
            subscription(dir.path(), provider, &["model"]);
        }
        service::logout(dir.path(), Provider::Glm, &token).unwrap();
        assert_eq!(service::list(dir.path()).unwrap(), [Provider::Zai]);
        assert_eq!(
            models::Catalog::load(&dir.path().join("models-sub.yml"))
                .unwrap()
                .providers
                .len(),
            1
        );
        fs::write(dir.path().join("models-sub.yml"), "secret-invalid-file").unwrap();
        let result = service::logout(dir.path(), Provider::Zai, &token).unwrap();
        assert!(result.logged.is_empty());
        assert!(result.message.contains("could not be removed"));
        assert!(!result.message.contains("secret"));
    }
}

#[test]
fn auth_provider_constraints_and_discovery_guards() {
    {
        let dir = tempfile::tempdir().unwrap();
        for extra in [
            "    api_key: secret\n",
            "    base_url: https://example.com\n",
            "    protocol: response\n",
            "    protocol: null\n",
            "    auth: null\n",
        ] {
            let models = dir.path().join("models.yml");
            let auth = if extra.contains("auth:") {
                ""
            } else {
                "    auth: glm\n"
            };
            fs::write(&models, format!("providers:\n  test:\n{auth}{extra}    models:\n      main:\n        model: fixture\n")).unwrap();
            let error = crate::models::load_models(&models)
                .err()
                .expect("must reject the configuration");
            assert!(!format!("{error:#}").contains("secret"));
        }
    }

    {
        for url in [
            "https://api.githubcopilot.com",
            "https://api.business.githubcopilot.com/",
        ] {
            assert!(copilot_endpoint(url));
        }
        for url in [
            "http://api.githubcopilot.com",
            "https://api.githubcopilot.com.evil.test",
            "https://api.githubcopilot.com@evil.test",
            "https://evil.test",
            "https://api.githubcopilot.com/path",
            "https://api.githubcopilot.com?x=1",
        ] {
            assert!(!copilot_endpoint(url));
        }
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-sub.yml");
        for provider in Provider::ALL {
            let other = if provider.protocol() == crate::models::Protocol::Completion {
                crate::models::Protocol::Response
            } else {
                crate::models::Protocol::Completion
            };
            let catalog = models::Catalog {
                providers: BTreeMap::from([(
                    provider.id().into(),
                    models::Entry {
                        auth: provider,
                        generation: "fixture".into(),
                        protocol: provider.protocol(),
                        models: BTreeMap::from([(
                            "fixture".into(),
                            models::Model {
                                model: "fixture".into(),
                                protocol: Some(other),
                                context_window: None,
                            },
                        )]),
                    },
                )]),
            };
            catalog.save(&path).unwrap();
            assert_eq!(
                models::Catalog::load(&path).is_ok(),
                matches!(
                    provider,
                    Provider::GithubCopilot
                        | Provider::CommandCode
                        | Provider::OpencodeZen
                        | Provider::OpencodeGo
                )
            );
        }
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::default();
        store
            .credentials
            .insert(Provider::Zai, Credential::api_key("private-marker".into()));
        store.save(&dir.path().join("agent.db")).unwrap();
        let update = service::discover(
            dir.path(),
            Provider::Zai,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(update.message.contains("Authorization needs to be renewed"));
        assert!(!update.message.contains("private-marker"));
        assert_eq!(update.logged, [Provider::Zai]);
        assert!(!dir.path().join("models-sub.yml").exists());
    }
}

#[test]
fn auth_manual_models_commandcode_and_opencode_validation() {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.yml");
        for auth in ["command-code", "commandcode"] {
            for protocol in ["completion", "response"] {
                std::fs::write(&path, format!("providers:\n  custom:\n    auth: {auth}\n    protocol: {protocol}\n    models:\n      main:\n        model: vendor/model\n")).unwrap();
                let providers = crate::models::load_models(&path).unwrap();
                assert_eq!(providers[0].auth, Some(Provider::CommandCode));
                assert_eq!(providers[0].models[0].model, "vendor/model");
                assert_eq!(
                    providers[0].protocol,
                    if protocol == "response" {
                        crate::models::Protocol::Response
                    } else {
                        crate::models::Protocol::Completion
                    }
                );
            }
        }
        assert_eq!(Provider::CommandCode.id(), "command-code");
        assert_eq!(
            Provider::CommandCode.base_url(),
            "https://api.commandcode.ai/provider/v1"
        );
        Credential::api_key("user_fixture".into())
            .validate(Provider::CommandCode)
            .unwrap();
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.yml");
        for (auth, provider, base_url) in [
            (
                "opencode-zen",
                Provider::OpencodeZen,
                "https://opencode.ai/zen/v1",
            ),
            (
                "opencode-go",
                Provider::OpencodeGo,
                "https://opencode.ai/zen/go/v1",
            ),
        ] {
            assert_eq!(provider.id(), auth);
            assert_eq!(provider.base_url(), base_url);
            assert_eq!(provider.key_url(), Some("https://opencode.ai/auth"));

            for protocol in ["completion", "response"] {
                fs::write(
                    &path,
                    format!(
                        "providers:\n  custom:\n    auth: {auth}\n    protocol: {protocol}\n    models:\n      main:\n        model: gpt-5\n"
                    ),
                )
                .unwrap();
                let providers = crate::models::load_models(&path).unwrap();
                assert_eq!(providers[0].auth, Some(provider));
                assert_eq!(
                    providers[0].models[0].thinking,
                    crate::models::ModelThinking::default()
                );
            }
            fs::write(
                &path,
                format!(
                    "providers:\n  custom:\n    auth: {auth}\n    models:\n      main:\n        model: gpt-5\n        thinking:\n          efforts: [low]\n"
                ),
            )
            .unwrap();
            let providers = crate::models::load_models(&path).unwrap();
            assert_eq!(
                providers[0].models[0].thinking.levels().collect::<Vec<_>>(),
                [pp_core::model::ThinkLevel::Low]
            );

            assert!(
                Credential::api_key("sk-fixture".into())
                    .validate(provider)
                    .is_err()
            );
            let mut invalid_session = Credential::api_key("sk-fixture".into());
            invalid_session.device_id = Some("bad\nsession".into());
            assert!(invalid_session.validate(provider).is_err());

            let first = service::api_key_credential(provider, "  sk-fixture  ").unwrap();
            let second = service::api_key_credential(provider, "sk-fixture").unwrap();
            assert_eq!(first.access_token, "sk-fixture");
            assert!(
                first
                    .device_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("pp-"))
            );
            assert_ne!(first.device_id, second.device_id);
            first.validate(provider).unwrap();
            assert!(service::api_key_credential(provider, "   ").is_err());
            assert!(service::api_key_credential(provider, &"x".repeat(16385)).is_err());
        }
    }
}
