use serde_json::Value;
use zb_core::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskBinary {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskApp {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskFont {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskPkg {
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskAppImage {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskGenericArtifact {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaskInstaller {
    Manual {
        source: String,
    },
    Script {
        executable: String,
        args: Vec<String>,
        sudo: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCask {
    pub install_name: String,
    pub token: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub binaries: Vec<CaskBinary>,
    pub apps: Vec<CaskApp>,
    pub fonts: Vec<CaskFont>,
    pub pkgs: Vec<CaskPkg>,
    pub installers: Vec<CaskInstaller>,
    pub appimages: Vec<CaskAppImage>,
    pub artifacts: Vec<CaskGenericArtifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaskInstallOptions {
    pub link_binaries: bool,
    pub link_apps: bool,
    pub link_fonts: bool,
    pub force: bool,
    pub require_sha: bool,
}

impl Default for CaskInstallOptions {
    fn default() -> Self {
        Self {
            link_binaries: true,
            link_apps: true,
            link_fonts: true,
            force: false,
            require_sha: false,
        }
    }
}

pub fn resolve_cask(token: &str, cask: &Value) -> Result<ResolvedCask, Error> {
    resolve_cask_with_options(token, cask, CaskInstallOptions::default())
}

pub fn resolve_cask_with_options(
    token: &str,
    cask: &Value,
    options: CaskInstallOptions,
) -> Result<ResolvedCask, Error> {
    let mut url = required_string(cask, "url")?;
    let mut sha256 = required_string(cask, "sha256")?;
    let version = required_string(cask, "version")?;

    if let Some(variation) = select_platform_variation(cask) {
        if let Some(variation_url) = variation.get("url").and_then(Value::as_str) {
            url = variation_url.to_string();
        }
        if let Some(variation_sha) = variation.get("sha256").and_then(Value::as_str) {
            sha256 = variation_sha.to_string();
        }
    }

    if sha256 == "no_check" || sha256 == ":no_check" {
        let message = if options.require_sha {
            format!("cask '{token}' uses sha256 no_check but --require-sha was requested")
        } else {
            format!("cask '{token}' uses an unsupported checksum mode: no_check")
        };
        return Err(Error::InvalidArgument { message });
    }

    let binaries = parse_binary_artifacts(cask)?;
    let apps = parse_app_artifacts(cask)?;
    let fonts = parse_font_artifacts(cask)?;
    let pkgs = parse_pkg_artifacts(cask)?;
    let installers = parse_installer_artifacts(cask)?;
    let appimages = parse_appimage_artifacts(cask)?;
    let artifacts = parse_generic_artifacts(cask)?;
    if binaries.is_empty()
        && apps.is_empty()
        && fonts.is_empty()
        && pkgs.is_empty()
        && installers.is_empty()
        && appimages.is_empty()
        && artifacts.is_empty()
    {
        let found = artifact_types(cask);
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{token}' has no supported artifacts (found: {found}); \
                 only casks with 'binary', 'app', 'font', 'pkg', 'installer', 'appimage', and supported 'artifact' artifacts are currently supported"
            ),
        });
    }

    Ok(ResolvedCask {
        install_name: format!("cask:{token}"),
        token: token.to_string(),
        version,
        url,
        sha256,
        binaries,
        apps,
        fonts,
        pkgs,
        installers,
        appimages,
        artifacts,
    })
}

fn required_string(value: &Value, field: &str) -> Result<String, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("failed to parse cask JSON: missing string field '{field}'"),
        })
}

fn select_platform_variation(cask: &Value) -> Option<&Value> {
    let variations = cask.get("variations")?;
    preferred_variation_keys()
        .iter()
        .find_map(|key| variations.get(key))
}

fn preferred_variation_keys() -> &'static [&'static str] {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["x86_64_linux", "arm64_linux"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["arm64_linux", "x86_64_linux"]
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &[
            "arm64_tahoe",
            "arm64_sequoia",
            "arm64_sonoma",
            "arm64_ventura",
            "arm64_monterey",
            "arm64_big_sur",
        ]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &[
            "tahoe", "sequoia", "sonoma", "ventura", "monterey", "big_sur", "catalina",
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        &[]
    }
}

fn artifact_types(cask: &Value) -> String {
    let types: Vec<&str> = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_object())
        .flat_map(|obj| obj.keys())
        .map(String::as_str)
        .collect();

    if types.is_empty() {
        "none".to_string()
    } else {
        types.join(", ")
    }
}

fn parse_binary_artifacts(cask: &Value) -> Result<Vec<CaskBinary>, Error> {
    let mut binaries = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("binary") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            let (source, target) = parse_binary_entry(entry)?;
            binaries.push(CaskBinary { source, target });
        }
    }

    Ok(binaries)
}

fn parse_app_artifacts(cask: &Value) -> Result<Vec<CaskApp>, Error> {
    let mut apps = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("app") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            let (source, target) = parse_app_entry(entry)?;
            apps.push(CaskApp { source, target });
        }
    }

    Ok(apps)
}

fn parse_font_artifacts(cask: &Value) -> Result<Vec<CaskFont>, Error> {
    let mut fonts = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("font") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            let (source, target) = parse_font_entry(entry)?;
            fonts.push(CaskFont { source, target });
        }
    }

    Ok(fonts)
}

fn parse_pkg_artifacts(cask: &Value) -> Result<Vec<CaskPkg>, Error> {
    let mut pkgs = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("pkg") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            let source = parse_pkg_entry(entry)?;
            pkgs.push(CaskPkg { source });
        }
    }

    Ok(pkgs)
}

fn parse_installer_artifacts(cask: &Value) -> Result<Vec<CaskInstaller>, Error> {
    let mut installers = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("installer") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            installers.push(parse_installer_entry(entry)?);
        }
    }

    Ok(installers)
}

fn parse_appimage_artifacts(cask: &Value) -> Result<Vec<CaskAppImage>, Error> {
    let mut appimages = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("appimage") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            let (source, target) = parse_appimage_entry(entry)?;
            appimages.push(CaskAppImage { source, target });
        }
    }

    Ok(appimages)
}

fn parse_generic_artifacts(cask: &Value) -> Result<Vec<CaskGenericArtifact>, Error> {
    let mut generic = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(value) = artifact.get("artifact") else {
            continue;
        };

        for entry in artifact_entries(value)? {
            let (source, target) = parse_generic_artifact_entry(entry)?;
            generic.push(CaskGenericArtifact { source, target });
        }
    }

    Ok(generic)
}

fn artifact_entries(value: &Value) -> Result<Vec<&Value>, Error> {
    let Some(entries) = value.as_array() else {
        return Ok(vec![value]);
    };

    if entries.first().is_some_and(Value::is_string) {
        return Ok(vec![value]);
    }

    Ok(entries.iter().collect())
}

fn parse_binary_entry(entry: &Value) -> Result<(String, String), Error> {
    if let Some(path) = entry.as_str() {
        return Ok((path.to_string(), basename(path)?));
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask binary artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask binary source".to_string(),
        })?;

    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| basename(source).unwrap_or_else(|_| source.to_string()));

    validate_relative_target(&target, "binary")?;

    Ok((source.to_string(), target))
}

fn parse_app_entry(entry: &Value) -> Result<(String, String), Error> {
    if let Some(path) = entry.as_str() {
        return Ok((path.to_string(), basename(path)?));
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask app artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask app source".to_string(),
        })?;

    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| basename(source).unwrap_or_else(|_| source.to_string()));

    validate_relative_target(&target, "app")?;
    Ok((source.to_string(), target))
}

fn parse_font_entry(entry: &Value) -> Result<(String, String), Error> {
    if let Some(path) = entry.as_str() {
        return Ok((path.to_string(), basename(path)?));
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask font artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask font source".to_string(),
        })?;

    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| basename(source).unwrap_or_else(|_| source.to_string()));

    validate_relative_target(&target, "font")?;
    Ok((source.to_string(), target))
}

fn parse_pkg_entry(entry: &Value) -> Result<String, Error> {
    if let Some(path) = entry.as_str() {
        return Ok(path.to_string());
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask pkg artifact shape".to_string(),
    })?;
    array
        .first()
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask pkg source".to_string(),
        })
}

fn parse_installer_entry(entry: &Value) -> Result<CaskInstaller, Error> {
    let object = entry.as_object().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask installer artifact shape".to_string(),
    })?;

    if let Some(manual) = object.get("manual").and_then(Value::as_str) {
        return Ok(CaskInstaller::Manual {
            source: manual.to_string(),
        });
    }

    let Some(script) = object.get("script") else {
        return Err(Error::InvalidArgument {
            message: "unsupported cask installer artifact: expected manual or script".to_string(),
        });
    };

    if let Some(executable) = script.as_str() {
        return Ok(CaskInstaller::Script {
            executable: executable.to_string(),
            args: Vec::new(),
            sudo: false,
        });
    }

    let script = script.as_object().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask installer script shape".to_string(),
    })?;
    let executable = script
        .get("executable")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask installer script executable".to_string(),
        })?;
    let args = script
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();
    let sudo = script.get("sudo").and_then(Value::as_bool).unwrap_or(false);

    Ok(CaskInstaller::Script {
        executable: executable.to_string(),
        args,
        sudo,
    })
}

fn parse_appimage_entry(entry: &Value) -> Result<(String, String), Error> {
    if let Some(path) = entry.as_str() {
        return Ok((path.to_string(), basename(path)?));
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask appimage artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask appimage source".to_string(),
        })?;
    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| basename(source).unwrap_or_else(|_| source.to_string()));

    validate_relative_target(&target, "appimage")?;
    Ok((source.to_string(), target))
}

fn parse_generic_artifact_entry(entry: &Value) -> Result<(String, String), Error> {
    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask artifact source".to_string(),
        })?;
    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask artifact target".to_string(),
        })?;
    let target = target
        .strip_prefix("$HOMEBREW_PREFIX/")
        .ok_or_else(|| Error::InvalidArgument {
            message: format!(
                "unsupported cask artifact target path '{target}'; only $HOMEBREW_PREFIX targets are currently supported"
            ),
        })?;
    validate_safe_relative_path(target, "artifact")?;
    Ok((source.to_string(), target.to_string()))
}

fn validate_relative_target(target: &str, artifact_kind: &str) -> Result<(), Error> {
    if target.contains('/') || target.contains('$') || target.contains('~') {
        return Err(Error::InvalidArgument {
            message: format!("unsupported cask {artifact_kind} target path '{target}'"),
        });
    }
    Ok(())
}

fn validate_safe_relative_path(target: &str, artifact_kind: &str) -> Result<(), Error> {
    let path = std::path::Path::new(target);
    if path.is_absolute() {
        return Err(Error::InvalidArgument {
            message: format!("unsupported cask {artifact_kind} target path '{target}'"),
        });
    }
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(Error::InvalidArgument {
                message: format!("unsupported cask {artifact_kind} target path '{target}'"),
            });
        }
    }
    Ok(())
}

fn basename(path: &str) -> Result<String, Error> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("invalid cask binary path '{path}'"),
        })?;
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cask_uses_platform_variation_url_and_sha() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/darwin.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }],
            "variations": {
                "x86_64_linux": {
                    "url": "https://example.com/linux.zip",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            }
        });

        let _resolved = resolve_cask("test", &cask).unwrap();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(_resolved.url, "https://example.com/linux.zip");
            assert_eq!(
                _resolved.sha256,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            );
        }
    }

    #[test]
    fn resolve_cask_parses_binary_targets() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{
                "binary": [
                    ["bin/tool"],
                    ["bin/tool2", {"target": "tool-two"}]
                ]
            }]
        });

        let resolved = resolve_cask("test", &cask).unwrap();
        assert_eq!(resolved.binaries.len(), 2);
        assert_eq!(resolved.binaries[0].target, "tool");
        assert_eq!(resolved.binaries[1].target, "tool-two");
        assert!(resolved.apps.is_empty());
    }

    #[test]
    fn resolve_cask_parses_homebrew_arg_array_artifact() {
        let cask = serde_json::json!({
            "token": "omniwm",
            "version": "1.0.0",
            "url": "https://example.com/omniwm.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["OmniWM.app"] },
                {
                    "binary": [
                        "/Applications/OmniWM.app/Contents/MacOS/omniwmctl",
                        {"target": "omniwmctl"}
                    ]
                }
            ]
        });

        let resolved = resolve_cask("omniwm", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 1);
        assert_eq!(resolved.binaries.len(), 1);
        assert_eq!(
            resolved.binaries[0].source,
            "/Applications/OmniWM.app/Contents/MacOS/omniwmctl"
        );
        assert_eq!(resolved.binaries[0].target, "omniwmctl");
    }

    #[test]
    fn resolve_cask_parses_app_targets() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                {
                    "binary": [["bin/tool"]],
                    "app": [
                        ["Test.app"],
                        ["Subdir/Other.app", {"target": "Renamed.app"}]
                    ]
                }
            ]
        });

        let resolved = resolve_cask("test", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 2);
        assert_eq!(resolved.apps[0].target, "Test.app");
        assert_eq!(resolved.apps[1].source, "Subdir/Other.app");
        assert_eq!(resolved.apps[1].target, "Renamed.app");
    }

    #[test]
    fn resolve_cask_parses_font_targets() {
        let cask = serde_json::json!({
            "token": "font-test",
            "version": "1.0.0",
            "url": "https://example.com/font.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                {
                    "font": [
                        ["Test-Regular.otf"],
                        ["Subdir/Test-Bold.otf", {"target": "Renamed-Bold.otf"}]
                    ]
                }
            ]
        });

        let resolved = resolve_cask("font-test", &cask).unwrap();
        assert!(resolved.binaries.is_empty());
        assert!(resolved.apps.is_empty());
        assert_eq!(resolved.fonts.len(), 2);
        assert_eq!(resolved.fonts[0].target, "Test-Regular.otf");
        assert_eq!(resolved.fonts[1].source, "Subdir/Test-Bold.otf");
        assert_eq!(resolved.fonts[1].target, "Renamed-Bold.otf");
    }

    #[test]
    fn resolve_cask_missing_required_field_is_invalid_argument() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }]
        });

        let err = resolve_cask("test", &cask).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn resolve_cask_missing_artifacts_array_is_invalid_argument() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });

        let err = resolve_cask("test", &cask).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn resolve_cask_accepts_app_only_casks() {
        let cask = serde_json::json!({
            "token": "ghostty",
            "version": "1.0.0",
            "url": "https://example.com/Ghostty.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Ghostty.app"] },
                { "zap": [{ "trash": ["~/.config/ghostty/"] }] }
            ]
        });

        let resolved = resolve_cask("ghostty", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 1);
        assert!(resolved.binaries.is_empty());
    }

    #[test]
    fn resolve_cask_parses_pkg_only_casks() {
        let cask = serde_json::json!({
            "token": "pkg-only",
            "version": "1.0.0",
            "url": "https://example.com/pkg-only.pkg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "pkg": ["Pkg.pkg"] },
                { "zap": [{ "trash": ["~/Library/Application Support/Pkg"] }] }
            ]
        });

        let resolved = resolve_cask("pkg-only", &cask).unwrap();
        assert_eq!(resolved.pkgs.len(), 1);
        assert_eq!(resolved.pkgs[0].source, "Pkg.pkg");
    }

    #[test]
    fn resolve_cask_parses_installer_scripts() {
        let cask = serde_json::json!({
            "token": "scripted",
            "version": "1.0.0",
            "url": "https://example.com/scripted.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{
                "installer": [{
                    "script": {
                        "executable": "Install.app/Contents/MacOS/install",
                        "args": ["--mode=silent"],
                        "sudo": true
                    }
                }]
            }]
        });

        let resolved = resolve_cask("scripted", &cask).unwrap();
        assert_eq!(
            resolved.installers,
            vec![CaskInstaller::Script {
                executable: "Install.app/Contents/MacOS/install".to_string(),
                args: vec!["--mode=silent".to_string()],
                sudo: true,
            }]
        );
    }

    #[test]
    fn resolve_cask_parses_appimage_and_prefix_artifacts() {
        let cask = serde_json::json!({
            "token": "linux-tool",
            "version": "1.0.0",
            "url": "https://example.com/linux-tool.tar.gz",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "appimage": [["LinuxTool.AppImage", {"target": "linux-tool"}]] },
                { "artifact": ["share", {"target": "$HOMEBREW_PREFIX/share/linux-tool"}] }
            ]
        });

        let resolved = resolve_cask("linux-tool", &cask).unwrap();
        assert_eq!(resolved.appimages.len(), 1);
        assert_eq!(resolved.appimages[0].target, "linux-tool");
        assert_eq!(resolved.artifacts.len(), 1);
        assert_eq!(resolved.artifacts[0].target, "share/linux-tool");
    }

    #[test]
    fn resolve_cask_no_supported_artifacts_lists_found_types() {
        let cask = serde_json::json!({
            "token": "preflight-only",
            "version": "1.0.0",
            "url": "https://example.com/preflight-only.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "preflight": null },
                { "zap": [{ "trash": ["~/Library/Application Support/Installer"] }] }
            ]
        });

        let err = resolve_cask("preflight-only", &cask).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no supported artifacts"), "got: {msg}");
        assert!(msg.contains("preflight"), "got: {msg}");
        assert!(msg.contains("zap"), "got: {msg}");
    }

    #[test]
    fn resolve_cask_require_sha_rejects_no_check() {
        let cask = serde_json::json!({
            "token": "unchecked",
            "version": "1.0.0",
            "url": "https://example.com/unchecked.zip",
            "sha256": "no_check",
            "artifacts": [{ "app": ["Unchecked.app"] }]
        });

        let err = resolve_cask_with_options(
            "unchecked",
            &cask,
            CaskInstallOptions {
                require_sha: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("--require-sha"));
    }
}
