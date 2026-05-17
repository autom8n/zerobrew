use std::collections::HashMap;

use zb_core::{Error, select_bottle};

use super::{Installer, OutdatedPackage};

impl Installer {
    pub async fn is_outdated(&self, name: &str) -> Result<Option<OutdatedPackage>, Error> {
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;

        if let Some(token) = name.strip_prefix("cask:") {
            let cask = self.api_client.get_cask(token).await?;
            return cask_outdated(name, installed.version, installed.store_key, &cask);
        }

        let formula = self.api_client.get_formula(name).await?;
        let is_source = installed.store_key.starts_with("source:");

        if is_source {
            let current_version = formula.effective_version();
            if installed.version == current_version {
                Ok(None)
            } else {
                Ok(Some(OutdatedPackage {
                    name: name.to_string(),
                    installed_version: installed.version,
                    installed_sha256: installed.store_key,
                    current_version,
                    current_sha256: String::new(),
                    is_source_build: true,
                }))
            }
        } else {
            let bottle = select_bottle(&formula)?;
            if installed.store_key == bottle.sha256 {
                Ok(None)
            } else {
                Ok(Some(OutdatedPackage {
                    name: name.to_string(),
                    installed_version: installed.version,
                    installed_sha256: installed.store_key,
                    current_version: formula.effective_version(),
                    current_sha256: bottle.sha256,
                    is_source_build: false,
                }))
            }
        }
    }

    pub async fn check_outdated(&self) -> Result<(Vec<OutdatedPackage>, Vec<String>), Error> {
        let installed = self.db.list_installed()?;
        if installed.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let installed_formula_names: std::collections::HashSet<&str> = installed
            .iter()
            .filter(|k| !k.name.starts_with("cask:"))
            .map(|k| k.name.as_str())
            .collect();

        let bulk_raw = self.api_client.get_all_formulas_raw().await?;
        let bulk_values: Vec<serde_json::Value> = serde_json::from_str(&bulk_raw)
            .map_err(Error::network("failed to parse bulk formula JSON"))?;

        let mut bulk_map: HashMap<String, zb_core::Formula> = HashMap::new();
        for val in bulk_values {
            let name = match val.get("name").and_then(|n| n.as_str()) {
                Some(n) if installed_formula_names.contains(n) => n.to_string(),
                _ => continue,
            };
            if let Ok(f) = serde_json::from_value(val) {
                bulk_map.insert(name, f);
            }
        }

        let mut outdated = Vec::new();
        let mut warnings = Vec::new();

        for keg in &installed {
            if let Some(token) = keg.name.strip_prefix("cask:") {
                match self.api_client.get_cask(token).await {
                    Ok(cask) => match cask_outdated(
                        &keg.name,
                        keg.version.clone(),
                        keg.store_key.clone(),
                        &cask,
                    ) {
                        Ok(Some(pkg)) => outdated.push(pkg),
                        Ok(None) => {}
                        Err(e) => warnings.push(format!("{}: {}", keg.name, e)),
                    },
                    Err(e) => warnings.push(format!("{}: {}", keg.name, e)),
                }
                continue;
            }

            let is_tap = keg.name.contains('/');

            let formula = if is_tap || !bulk_map.contains_key(&keg.name) {
                match self.api_client.get_formula(&keg.name).await {
                    Ok(f) => f,
                    Err(e) => {
                        warnings.push(format!("{}: {}", keg.name, e));
                        continue;
                    }
                }
            } else {
                bulk_map.remove(&keg.name).unwrap()
            };

            let is_source = keg.store_key.starts_with("source:");

            if is_source {
                let current_version = formula.effective_version();
                if keg.version != current_version {
                    outdated.push(OutdatedPackage {
                        name: keg.name.clone(),
                        installed_version: keg.version.clone(),
                        installed_sha256: keg.store_key.clone(),
                        current_version,
                        current_sha256: String::new(),
                        is_source_build: true,
                    });
                }
            } else {
                match select_bottle(&formula) {
                    Ok(bottle) => {
                        if keg.store_key != bottle.sha256 {
                            outdated.push(OutdatedPackage {
                                name: keg.name.clone(),
                                installed_version: keg.version.clone(),
                                installed_sha256: keg.store_key.clone(),
                                current_version: formula.effective_version(),
                                current_sha256: bottle.sha256,
                                is_source_build: false,
                            });
                        }
                    }
                    Err(e) => warnings.push(format!("{}: {}", keg.name, e)),
                }
            }
        }

        outdated.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((outdated, warnings))
    }

    pub async fn suggest_formulas(&self, query: &str, limit: usize) -> Result<Vec<String>, Error> {
        self.api_client.suggest_formulas(query, limit).await
    }
}

fn cask_outdated(
    name: &str,
    installed_version: String,
    installed_sha256: String,
    cask: &serde_json::Value,
) -> Result<Option<OutdatedPackage>, Error> {
    let current_version = cask
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("failed to parse cask JSON for '{name}': missing version"),
        })?
        .to_string();
    let current_sha256 = cask
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    if installed_version == current_version && installed_sha256 == current_sha256 {
        return Ok(None);
    }

    Ok(Some(OutdatedPackage {
        name: name.to_string(),
        installed_version,
        installed_sha256,
        current_version,
        current_sha256,
        is_source_build: false,
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    use super::super::test_support::get_test_bottle_tag;

    fn formula_json(name: &str, version: &str, sha256: &str) -> String {
        let tag = get_test_bottle_tag();
        format!(
            r#"{{
                "name": "{}",
                "versions": {{ "stable": "{}" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "https://example.com/{}-{}.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            name, version, tag, name, version, tag, sha256
        )
    }

    async fn test_installer() -> (Installer, MockServer, TempDir) {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_cask_base_url(format!("{}/cask", mock_server.uri()));
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix,
            root.join("locks"),
        );
        (installer, mock_server, tmp)
    }

    #[tokio::test]
    async fn suggest_formulas_returns_matches_from_api_client() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bulk = r#"[
            {"name":"python"},
            {"name":"pytest"},
            {"name":"pypy"}
        ]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix,
            root.join("locks"),
        );

        let suggestions = installer.suggest_formulas("pythn", 3).await.unwrap();
        assert_eq!(suggestions.first().map(String::as_str), Some("python"));
    }

    #[tokio::test]
    async fn is_outdated_checks_casks_against_cask_api() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("cask:demo", "1.0.0", &"a".repeat(64))
                .unwrap();
            tx.commit().unwrap();
        }

        let current_sha = "b".repeat(64);
        Mock::given(method("GET"))
            .and(path("/cask/demo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{
                    "token": "demo",
                    "version": "1.1.0",
                    "url": "https://example.com/demo.zip",
                    "sha256": "{current_sha}",
                    "artifacts": [{{"app": ["Demo.app"]}}]
                }}"#
            )))
            .mount(&mock_server)
            .await;

        let outdated = installer.is_outdated("cask:demo").await.unwrap().unwrap();
        assert_eq!(outdated.name, "cask:demo");
        assert_eq!(outdated.current_version, "1.1.0");
        assert_eq!(outdated.current_sha256, current_sha);
    }

    #[tokio::test]
    async fn is_outdated_returns_none_when_sha256_matches() {
        let (mut installer, mock_server, _tmp) = test_installer().await;
        let sha = "abc123def456";

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.1", sha).unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json("jq", "1.7.1", sha)),
            )
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn is_outdated_returns_some_when_sha256_differs() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.0", "old_sha256").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                "jq",
                "1.7.1",
                "new_sha256",
            )))
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap().unwrap();
        assert_eq!(result.name, "jq");
        assert_eq!(result.installed_version, "1.7.0");
        assert_eq!(result.current_version, "1.7.1");
        assert!(!result.is_source_build);
    }

    #[tokio::test]
    async fn is_outdated_errors_for_not_installed() {
        let (installer, _mock_server, _tmp) = test_installer().await;

        let err = installer.is_outdated("jq").await.unwrap_err();
        assert!(matches!(err, zb_core::Error::NotInstalled { .. }));
    }

    #[tokio::test]
    async fn is_outdated_source_build_compares_version_only() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.1", "source:jq:1.7.1").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                "jq",
                "1.7.1",
                "irrelevant",
            )))
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn is_outdated_source_build_detects_new_version() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.6", "source:jq:1.6").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                "jq",
                "1.7.1",
                "irrelevant",
            )))
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap().unwrap();
        assert_eq!(result.installed_version, "1.6");
        assert_eq!(result.current_version, "1.7.1");
        assert!(result.is_source_build);
    }

    #[tokio::test]
    async fn check_outdated_empty_when_nothing_installed() {
        let (installer, _mock_server, _tmp) = test_installer().await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert!(outdated.is_empty());
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn check_outdated_continues_on_network_failure() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("good", "1.0.0", "old_sha").unwrap();
            tx.record_install("bad", "1.0.0", "old_sha").unwrap();
            tx.commit().unwrap();
        }

        let bulk = format!("[{}]", formula_json("good", "2.0.0", "new_sha"));
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/formula/bad.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].name, "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad"));
    }

    #[tokio::test]
    async fn check_outdated_warns_on_missing_bottle() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("nobottle", "1.0.0", "old_sha").unwrap();
            tx.commit().unwrap();
        }

        let bulk = r#"[{
            "name": "nobottle",
            "versions": { "stable": "2.0.0" },
            "dependencies": [],
            "bottle": { "stable": { "files": {} } }
        }]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert!(outdated.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nobottle"));
    }
}
