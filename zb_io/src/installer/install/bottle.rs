use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::warn;
use zb_core::{Error, InstallMethod, formula_token};

use crate::cellar::link::Linker;
use crate::cellar::materialize::Cellar;
use crate::installer::cask::{CaskInstallOptions, CaskInstaller, resolve_cask_with_options};
use crate::network::download::{DownloadProgressCallback, DownloadRequest, DownloadResult};
use crate::progress::InstallProgress;
use crate::storage::store::Store;

use super::{Installer, MAX_CORRUPTION_RETRIES, PlannedInstall};

const CASK_APPS_DIR: &str = "Applications";
const CASK_FONTS_DIR: &str = "Fonts";
const CASK_PKGS_DIR: &str = "Packages";
const CASK_INSTALLERS_DIR: &str = "Installers";

impl Installer {
    pub(super) async fn process_bottle_item(
        &mut self,
        item: &PlannedInstall,
        download: &DownloadResult,
        download_progress: &Option<DownloadProgressCallback>,
        link: bool,
        report: &impl Fn(InstallProgress),
    ) -> Result<(), Error> {
        let InstallMethod::Bottle(ref bottle) = item.method else {
            unreachable!()
        };
        let install_name = &item.install_name;
        let formula_name = &item.formula.name;
        let version = item.formula.effective_version();
        let store_key = &bottle.sha256;

        report(InstallProgress::UnpackStarted {
            name: formula_name.clone(),
        });

        let store_entry = self
            .extract_with_retry(download, &item.formula, bottle, download_progress.clone())
            .await?;

        let keg_path = self
            .cellar
            .materialize(formula_name, &version, &store_entry)?;

        report(InstallProgress::UnpackCompleted {
            name: formula_name.clone(),
        });

        let tx = self.db.transaction().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        tx.record_install(install_name, &version, store_key)
            .inspect_err(|_| {
                Self::cleanup_materialized(&self.cellar, formula_name, &version);
            })?;

        tx.commit().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        if let Err(e) = self.linker.link_opt(&keg_path) {
            warn!(formula = %install_name, error = %e, "failed to create opt link");
        }
        for alias in &item.formula.aliases {
            if let Err(e) = self.linker.link_opt_alias(alias, &keg_path) {
                warn!(formula = %install_name, alias = %alias, error = %e, "failed to create opt alias link");
            }
        }

        run_builtin_post_install(&self.prefix, install_name, &keg_path)?;

        if link && !item.formula.is_keg_only() {
            report(InstallProgress::LinkStarted {
                name: formula_name.clone(),
            });
            match self.linker.link_keg(&keg_path) {
                Ok(linked_files) => {
                    report(InstallProgress::LinkCompleted {
                        name: formula_name.clone(),
                    });
                    self.record_linked_files(install_name, &version, &linked_files);
                }
                Err(e) => {
                    let _ = self.linker.unlink_keg(&keg_path);
                    report(InstallProgress::InstallCompleted {
                        name: formula_name.clone(),
                    });
                    return Err(e);
                }
            }
        } else if link && item.formula.is_keg_only() {
            let reason = match &item.formula.keg_only {
                zb_core::KegOnly::Reason(s) => s.clone(),
                _ if formula_name.contains('@') => "versioned formula".to_string(),
                _ => "keg-only formula".to_string(),
            };
            report(InstallProgress::LinkSkipped {
                name: formula_name.clone(),
                reason,
            });
        }

        report(InstallProgress::InstallCompleted {
            name: formula_name.clone(),
        });

        Ok(())
    }

    async fn extract_with_retry(
        &self,
        download: &DownloadResult,
        formula: &zb_core::Formula,
        bottle: &zb_core::SelectedBottle,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<std::path::PathBuf, Error> {
        let mut blob_path = download.blob_path.clone();
        let mut last_error = None;

        for attempt in 0..MAX_CORRUPTION_RETRIES {
            match self.store.ensure_entry(&bottle.sha256, &blob_path) {
                Ok(entry) => return Ok(entry),
                Err(Error::StoreCorruption { message }) => {
                    self.downloader.remove_blob(&bottle.sha256);

                    if attempt + 1 < MAX_CORRUPTION_RETRIES {
                        warn!(
                            formula = %formula.name,
                            attempt = attempt + 2,
                            max_retries = MAX_CORRUPTION_RETRIES,
                            "corrupted download detected; retrying"
                        );

                        let request = DownloadRequest {
                            url: bottle.url.clone(),
                            sha256: bottle.sha256.clone(),
                            name: formula.name.clone(),
                        };

                        match self
                            .downloader
                            .download_single(request, progress.clone())
                            .await
                        {
                            Ok(new_path) => {
                                blob_path = new_path;
                            }
                            Err(e) => {
                                last_error = Some(e);
                                break;
                            }
                        }
                    } else {
                        last_error = Some(Error::StoreCorruption {
                            message: format!(
                                "{message}\n\nFailed after {MAX_CORRUPTION_RETRIES} attempts. The download may be corrupted at the source."
                            ),
                        });
                    }
                }
                Err(e) => {
                    last_error = Some(e);
                    break;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::StoreCorruption {
            message: "extraction failed with unknown error".to_string(),
        }))
    }

    fn record_linked_files(
        &mut self,
        name: &str,
        version: &str,
        linked_files: &[crate::cellar::link::LinkedFile],
    ) {
        if let Ok(tx) = self.db.transaction() {
            let mut ok = true;
            for linked in linked_files {
                if tx
                    .record_linked_file(
                        name,
                        version,
                        &linked.link_path.to_string_lossy(),
                        &linked.target_path.to_string_lossy(),
                    )
                    .is_err()
                {
                    ok = false;
                    break;
                }
            }
            if ok {
                let _ = tx.commit();
            }
        }
    }

    pub(super) fn cleanup_failed_install(
        linker: &Linker,
        cellar: &Cellar,
        name: &str,
        version: &str,
        keg_path: &Path,
        unlink: bool,
    ) {
        if unlink && let Err(e) = linker.unlink_keg(keg_path) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to clean up links after install error"
            );
        }

        if unlink && let Err(e) = uninstall_cask_apps(keg_path) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove installed apps after install error"
            );
        }

        if let Err(e) = cellar.remove_keg(name, version) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove keg after install error"
            );
        }
    }

    pub(super) async fn install_single_cask_with_options(
        &mut self,
        token: &str,
        options: CaskInstallOptions,
    ) -> Result<(), Error> {
        let cask_json = self.api_client.get_cask(token).await?;
        self.install_single_cask_from_json_with_options(token, cask_json, options)
            .await
    }

    pub(super) async fn install_single_cask_from_json_with_options(
        &mut self,
        token: &str,
        cask_json: serde_json::Value,
        options: CaskInstallOptions,
    ) -> Result<(), Error> {
        let cask = resolve_cask_with_options(token, &cask_json, options)?;

        let blob_path = self
            .downloader
            .download_single(
                DownloadRequest {
                    url: cask.url.clone(),
                    sha256: cask.sha256.clone(),
                    name: cask.install_name.clone(),
                },
                None,
            )
            .await?;

        let keg_path = self.cellar.keg_path(&cask.install_name, &cask.version);
        let mut cleanup = FailedInstallGuard::new(
            &self.linker,
            &self.cellar,
            &cask.install_name,
            &cask.version,
            &keg_path,
            options.link_binaries || options.link_apps || options.link_fonts,
        );

        if crate::extraction::is_archive(&blob_path)? {
            let extracted = self.store.ensure_entry(&cask.sha256, &blob_path)?;
            stage_cask_artifacts(&extracted, &keg_path, &cask)?;
        } else if is_dmg_cask(&blob_path, &cask) {
            let extracted = ensure_dmg_store_entry(&self.store, &cask.sha256, &blob_path)?;
            stage_cask_artifacts(&extracted, &keg_path, &cask)?;
        } else if is_raw_pkg_cask(&cask) {
            stage_raw_cask_pkg(&blob_path, &keg_path, &cask)?;
        } else {
            let stored = ensure_raw_cask_store_entry(&self.store, &cask.sha256, &blob_path, &cask)?;
            copy_path_recursive(&stored, &keg_path)?;
        }

        install_cask_pkgs(&keg_path, &cask)?;
        install_cask_installers(&keg_path, &cask)?;

        let should_link_keg = options.link_binaries
            && (!cask.binaries.is_empty()
                || !cask.appimages.is_empty()
                || !cask.artifacts.is_empty());
        let linked_files = if should_link_keg {
            self.linker.link_keg(&keg_path)?
        } else {
            Vec::new()
        };
        if options.link_apps {
            link_cask_apps(&keg_path, self.app_dir(), &cask, options.force)?;
        }
        if options.link_fonts {
            link_cask_fonts(&keg_path, self.font_dir(), &cask, options.force)?;
        }

        let linked_files = if should_link_keg {
            linked_files
        } else {
            Vec::new()
        };

        let tx = self.db.transaction()?;
        tx.record_install(&cask.install_name, &cask.version, &cask.sha256)?;
        for linked in &linked_files {
            tx.record_linked_file(
                &cask.install_name,
                &cask.version,
                &linked.link_path.to_string_lossy(),
                &linked.target_path.to_string_lossy(),
            )?;
        }
        tx.commit()?;

        cleanup.disarm();
        Ok(())
    }
}

pub(super) fn dependency_cellar_path(
    cellar: &Cellar,
    installed_name: &str,
    version: &str,
) -> String {
    cellar
        .keg_path(formula_token(installed_name), version)
        .display()
        .to_string()
}

struct FailedInstallGuard<'a> {
    linker: &'a Linker,
    cellar: &'a Cellar,
    name: &'a str,
    version: &'a str,
    keg_path: &'a Path,
    unlink: bool,
    armed: bool,
}

impl<'a> FailedInstallGuard<'a> {
    fn new(
        linker: &'a Linker,
        cellar: &'a Cellar,
        name: &'a str,
        version: &'a str,
        keg_path: &'a Path,
        unlink: bool,
    ) -> Self {
        Self {
            linker,
            cellar,
            name,
            version,
            keg_path,
            unlink,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FailedInstallGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            Installer::cleanup_failed_install(
                self.linker,
                self.cellar,
                self.name,
                self.version,
                self.keg_path,
                self.unlink,
            );
        }
    }
}

fn stage_cask_binaries(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.binaries.is_empty() {
        return Ok(());
    }

    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    for binary in &cask.binaries {
        let source =
            resolve_cask_binary_source_path(extracted_root, keg_path, cask, &binary.source)?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' binary source '{}' not found",
                    cask.token, binary.source
                ),
            });
        }

        let target = bin_dir.join(&binary.target);
        if target.exists() {
            fs::remove_file(&target)
                .map_err(Error::store("failed to replace existing cask binary"))?;
        }

        fs::copy(&source, &target).map_err(|e| Error::StoreCorruption {
            message: format!("failed to stage cask binary '{}': {e}", binary.target),
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target)
                .map_err(Error::store("failed to read staged cask binary metadata"))?
                .permissions();
            if perms.mode() & 0o111 == 0 {
                perms.set_mode(0o755);
                fs::set_permissions(&target, perms)
                    .map_err(Error::store("failed to make staged cask binary executable"))?;
            }
        }
    }

    Ok(())
}

fn stage_raw_cask_binary(
    blob_path: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if !cask.apps.is_empty()
        || !cask.fonts.is_empty()
        || !cask.pkgs.is_empty()
        || !cask.installers.is_empty()
        || !cask.appimages.is_empty()
        || !cask.artifacts.is_empty()
        || cask.binaries.len() != 1
    {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' has unsupported raw download layout; expected exactly 1 binary artifact and no app/font artifacts",
                cask.token
            ),
        });
    }

    let binary = &cask.binaries[0];
    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    let target = bin_dir.join(&binary.target);
    if target.exists() {
        fs::remove_file(&target).map_err(Error::store("failed to replace existing cask binary"))?;
    }

    fs::copy(blob_path, &target).map_err(|e| Error::StoreCorruption {
        message: format!("failed to stage cask binary '{}': {e}", binary.target),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .map_err(Error::store("failed to make staged cask binary executable"))?;
    }

    Ok(())
}

fn stage_cask_artifacts(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    stage_cask_apps(extracted_root, keg_path, cask)?;
    stage_cask_fonts(extracted_root, keg_path, cask)?;
    stage_cask_pkgs(extracted_root, keg_path, cask)?;
    stage_cask_installers(extracted_root, keg_path, cask)?;
    stage_cask_appimages(extracted_root, keg_path, cask)?;
    stage_cask_generic_artifacts(extracted_root, keg_path, cask)?;
    stage_cask_binaries(extracted_root, keg_path, cask)?;
    Ok(())
}

fn is_raw_pkg_cask(cask: &crate::installer::cask::ResolvedCask) -> bool {
    cask.binaries.is_empty()
        && cask.apps.is_empty()
        && cask.fonts.is_empty()
        && cask.installers.is_empty()
        && cask.appimages.is_empty()
        && cask.artifacts.is_empty()
        && cask.pkgs.len() == 1
}

fn is_dmg_cask(blob_path: &Path, cask: &crate::installer::cask::ResolvedCask) -> bool {
    blob_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dmg"))
        || cask
            .url
            .split(['?', '#'])
            .next()
            .is_some_and(|url_path| url_path.to_ascii_lowercase().ends_with(".dmg"))
}

#[cfg(target_os = "macos")]
fn ensure_dmg_store_entry(
    store: &Store,
    store_key: &str,
    dmg_path: &Path,
) -> Result<PathBuf, Error> {
    let entry_path = store.entry_path(store_key);
    if entry_path.exists() {
        return Ok(entry_path);
    }

    let store_dir = entry_path.parent().ok_or_else(|| Error::StoreCorruption {
        message: format!("store entry '{}' has no parent", entry_path.display()),
    })?;
    let extracted = tempfile::tempdir_in(store_dir)
        .map_err(Error::store("failed to create dmg store entry"))?;
    let mount = DmgMount::attach(dmg_path)?;

    for mount_point in &mount.mount_points {
        for entry in
            fs::read_dir(mount_point).map_err(Error::store("failed to read dmg mount point"))?
        {
            let entry = entry.map_err(Error::store("failed to read dmg entry"))?;
            copy_path_recursive(&entry.path(), &extracted.path().join(entry.file_name()))?;
        }
    }

    persist_temp_store_entry(extracted, &entry_path)
}

#[cfg(not(target_os = "macos"))]
fn ensure_dmg_store_entry(
    _store: &Store,
    _store_key: &str,
    _dmg_path: &Path,
) -> Result<PathBuf, Error> {
    Err(Error::InvalidArgument {
        message: "dmg casks are only supported on macOS".to_string(),
    })
}

fn ensure_raw_cask_store_entry(
    store: &Store,
    store_key: &str,
    blob_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<PathBuf, Error> {
    let entry_path = store.entry_path(store_key);
    if entry_path.exists() {
        return Ok(entry_path);
    }

    let store_dir = entry_path.parent().ok_or_else(|| Error::StoreCorruption {
        message: format!("store entry '{}' has no parent", entry_path.display()),
    })?;
    let staged = tempfile::tempdir_in(store_dir)
        .map_err(Error::store("failed to create cask store entry"))?;
    stage_raw_cask_binary(blob_path, staged.path(), cask)?;
    persist_temp_store_entry(staged, &entry_path)
}

fn persist_temp_store_entry(
    temp_dir: tempfile::TempDir,
    entry_path: &Path,
) -> Result<PathBuf, Error> {
    let tmp_path = temp_dir.keep();
    match fs::rename(&tmp_path, entry_path) {
        Ok(()) => Ok(entry_path.to_path_buf()),
        Err(_) if entry_path.exists() => {
            let _ = fs::remove_dir_all(&tmp_path);
            Ok(entry_path.to_path_buf())
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp_path);
            Err(Error::StoreCorruption {
                message: format!("failed to rename store entry: {e}"),
            })
        }
    }
}

fn run_builtin_post_install(
    prefix: &Path,
    install_name: &str,
    keg_path: &Path,
) -> Result<(), Error> {
    match formula_token(install_name) {
        "ca-certificates" => install_ca_certificates_bundle(prefix, keg_path),
        _ => Ok(()),
    }
}

fn install_ca_certificates_bundle(prefix: &Path, keg_path: &Path) -> Result<(), Error> {
    let source = keg_path
        .join("share")
        .join("ca-certificates")
        .join("cacert.pem");
    if !source.exists() {
        return Ok(());
    }

    let target_dir = prefix.join("etc").join("ca-certificates");
    fs::create_dir_all(&target_dir)
        .map_err(Error::store("failed to create ca-certificates dir"))?;
    fs::copy(&source, target_dir.join("cert.pem"))
        .map_err(Error::store("failed to install ca-certificates bundle"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
struct DmgMount {
    mount_points: Vec<PathBuf>,
    _mount_root: tempfile::TempDir,
}

#[cfg(target_os = "macos")]
impl DmgMount {
    fn attach(dmg_path: &Path) -> Result<Self, Error> {
        let mount_root =
            tempfile::tempdir().map_err(Error::store("failed to create dmg mount dir"))?;
        let output = Command::new("hdiutil")
            .args(["attach", "-plist", "-nobrowse", "-readonly", "-mountrandom"])
            .arg(mount_root.path())
            .arg(dmg_path)
            .output()
            .map_err(Error::exec("failed to run hdiutil attach"))?;

        if !output.status.success() {
            return Err(Error::ExecutionError {
                message: format!(
                    "hdiutil attach failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            });
        }

        let plist = String::from_utf8_lossy(&output.stdout);
        let mount_points = parse_hdiutil_mount_points(&plist);
        if mount_points.is_empty() {
            return Err(Error::ExecutionError {
                message: "hdiutil attach returned no mount points".to_string(),
            });
        }

        Ok(Self {
            mount_points,
            _mount_root: mount_root,
        })
    }
}

#[cfg(target_os = "macos")]
impl Drop for DmgMount {
    fn drop(&mut self) {
        for mount_point in self.mount_points.iter().rev() {
            let _ = Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(mount_point)
                .status();
        }
    }
}

fn parse_hdiutil_mount_points(plist: &str) -> Vec<PathBuf> {
    let mut mount_points = Vec::new();
    let mut rest = plist;

    while let Some(key_idx) = rest.find("<key>mount-point</key>") {
        rest = &rest[key_idx + "<key>mount-point</key>".len()..];
        let Some(start_idx) = rest.find("<string>") else {
            break;
        };
        rest = &rest[start_idx + "<string>".len()..];
        let Some(end_idx) = rest.find("</string>") else {
            break;
        };
        mount_points.push(PathBuf::from(xml_unescape(&rest[..end_idx])));
        rest = &rest[end_idx + "</string>".len()..];
    }

    mount_points
}

fn xml_unescape(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn stage_cask_apps(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.apps.is_empty() {
        return Ok(());
    }

    let apps_dir = keg_path.join(CASK_APPS_DIR);
    fs::create_dir_all(&apps_dir).map_err(Error::store("failed to create cask app dir"))?;

    for app in &cask.apps {
        let source = resolve_relative_cask_path(extracted_root, cask, &app.source, "app")?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' app source '{}' not found in archive",
                    cask.token, app.source
                ),
            });
        }

        let target = apps_dir.join(&app.target);
        if target.symlink_metadata().is_ok() {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask app"))?;
        }

        copy_path_recursive(&source, &target)?;
    }

    Ok(())
}

fn stage_cask_fonts(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.fonts.is_empty() {
        return Ok(());
    }

    let fonts_dir = keg_path.join(CASK_FONTS_DIR);
    fs::create_dir_all(&fonts_dir).map_err(Error::store("failed to create cask font dir"))?;

    for font in &cask.fonts {
        let source = resolve_relative_cask_path(extracted_root, cask, &font.source, "font")?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' font source '{}' not found in archive",
                    cask.token, font.source
                ),
            });
        }

        let target = fonts_dir.join(&font.target);
        if target.symlink_metadata().is_ok() {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask font"))?;
        }

        copy_path_recursive(&source, &target)?;
    }

    Ok(())
}

fn stage_cask_pkgs(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.pkgs.is_empty() {
        return Ok(());
    }

    let pkgs_dir = keg_path.join(CASK_PKGS_DIR);
    fs::create_dir_all(&pkgs_dir).map_err(Error::store("failed to create cask pkg dir"))?;

    for pkg in &cask.pkgs {
        let source = resolve_relative_cask_path(extracted_root, cask, &pkg.source, "pkg")?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' pkg source '{}' not found in archive",
                    cask.token, pkg.source
                ),
            });
        }

        let target = pkgs_dir.join(basename_path(&pkg.source)?);
        if target.symlink_metadata().is_ok() {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask pkg"))?;
        }

        copy_path_recursive(&source, &target)?;
    }

    Ok(())
}

fn stage_raw_cask_pkg(
    blob_path: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if !is_raw_pkg_cask(cask) {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' has unsupported raw pkg layout; expected exactly 1 pkg artifact and no app/font/binary artifacts",
                cask.token
            ),
        });
    }

    let pkgs_dir = keg_path.join(CASK_PKGS_DIR);
    fs::create_dir_all(&pkgs_dir).map_err(Error::store("failed to create cask pkg dir"))?;
    let target = pkgs_dir.join(basename_path(&cask.pkgs[0].source)?);
    if target.symlink_metadata().is_ok() {
        remove_path_any(&target).map_err(Error::store("failed to replace existing cask pkg"))?;
    }
    fs::copy(blob_path, target).map_err(Error::store("failed to stage raw cask pkg"))?;
    Ok(())
}

fn stage_cask_appimages(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.appimages.is_empty() {
        return Ok(());
    }

    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    for appimage in &cask.appimages {
        let source =
            resolve_relative_cask_path(extracted_root, cask, &appimage.source, "appimage")?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' appimage source '{}' not found in archive",
                    cask.token, appimage.source
                ),
            });
        }

        let target = bin_dir.join(&appimage.target);
        if target.symlink_metadata().is_ok() {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask appimage"))?;
        }
        copy_path_recursive(&source, &target)?;
        make_executable(&target)?;
    }

    Ok(())
}

fn stage_cask_generic_artifacts(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.artifacts.is_empty() {
        return Ok(());
    }

    for artifact in &cask.artifacts {
        let source =
            resolve_relative_cask_path(extracted_root, cask, &artifact.source, "artifact")?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' artifact source '{}' not found in archive",
                    cask.token, artifact.source
                ),
            });
        }

        let target = keg_path.join(&artifact.target);
        if target.symlink_metadata().is_ok() {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask artifact"))?;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(Error::store("failed to create cask artifact parent"))?;
        }
        copy_path_recursive(&source, &target)?;
    }

    Ok(())
}

fn stage_cask_installers(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.installers.is_empty() {
        return Ok(());
    }

    for installer in &cask.installers {
        let source = match installer {
            CaskInstaller::Manual { source } => source,
            CaskInstaller::Script { executable, .. } => executable,
        };
        stage_cask_installer_source(extracted_root, keg_path, cask, source)?;
    }

    Ok(())
}

fn stage_cask_installer_source(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
    source: &str,
) -> Result<(), Error> {
    let (stage_root, executable_relative) = installer_stage_root(source);
    let resolved_stage_root =
        resolve_relative_cask_path(extracted_root, cask, &stage_root, "installer")?;
    if !resolved_stage_root.exists() {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' installer source '{}' not found in archive",
                cask.token, source
            ),
        });
    }

    let target = keg_path.join(CASK_INSTALLERS_DIR).join(&stage_root);
    if target.symlink_metadata().is_ok() {
        remove_path_any(&target)
            .map_err(Error::store("failed to replace existing cask installer"))?;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(Error::store("failed to create cask installer dir"))?;
    }
    copy_path_recursive(&resolved_stage_root, &target)?;

    let executable = keg_path.join(CASK_INSTALLERS_DIR).join(executable_relative);
    if executable.is_file() {
        make_executable(&executable)?;
    }

    Ok(())
}

fn installer_stage_root(source: &str) -> (String, String) {
    let mut accumulated = Vec::new();
    for component in Path::new(source).components() {
        let component = component.as_os_str().to_string_lossy().to_string();
        accumulated.push(component.clone());
        if component.ends_with(".app") {
            return (accumulated.join("/"), source.to_string());
        }
    }

    (source.to_string(), source.to_string())
}

fn resolve_cask_binary_source_path(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
    source: &str,
) -> Result<PathBuf, Error> {
    if source.starts_with("$APPDIR") {
        let relative = source
            .strip_prefix("$APPDIR/")
            .or_else(|| source.strip_prefix("$APPDIR"))
            .unwrap_or(source);
        return resolve_relative_cask_path(&keg_path.join(CASK_APPS_DIR), cask, relative, "binary");
    }

    if Path::new(source).is_absolute() {
        for app in &cask.apps {
            let app_prefix = format!("/{}/", app.target);
            if let Some(idx) = source.find(&app_prefix) {
                let relative = &source[idx + 1..];
                return resolve_relative_cask_path(
                    &keg_path.join(CASK_APPS_DIR),
                    cask,
                    relative,
                    "binary",
                );
            }
        }
    }

    resolve_relative_cask_path(extracted_root, cask, source, "binary")
}

fn resolve_relative_cask_path(
    root: &Path,
    cask: &crate::installer::cask::ResolvedCask,
    source: &str,
    artifact_kind: &str,
) -> Result<PathBuf, Error> {
    let mut normalized = source.to_string();
    let caskroom_prefix = format!("$HOMEBREW_PREFIX/Caskroom/{}/{}/", cask.token, cask.version);
    if let Some(stripped) = normalized.strip_prefix(&caskroom_prefix) {
        normalized = stripped.to_string();
    }

    let source_path = Path::new(&normalized);
    if source_path.is_absolute() {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' {artifact_kind} source '{}' must be a relative path",
                cask.token, source
            ),
        });
    }

    for component in source_path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' {artifact_kind} source '{}' cannot contain '..'",
                    cask.token, source
                ),
            });
        }
    }

    Ok(root.join(source_path))
}

fn copy_path_recursive(src: &Path, dst: &Path) -> Result<(), Error> {
    let metadata =
        fs::symlink_metadata(src).map_err(Error::store("failed to read source metadata"))?;

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(src).map_err(Error::store("failed to read source symlink"))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dst)
            .map_err(Error::store("failed to create symlink"))?;
        #[cfg(not(unix))]
        fs::copy(src, dst).map_err(Error::store("failed to copy symlink as file"))?;
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(dst).map_err(Error::store("failed to create target directory"))?;
        for entry in fs::read_dir(src).map_err(Error::store("failed to read source directory"))? {
            let entry = entry.map_err(Error::store("failed to read source directory entry"))?;
            copy_path_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }

    fs::copy(src, dst).map_err(Error::store("failed to copy file"))?;
    Ok(())
}

fn install_cask_pkgs(
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.pkgs.is_empty() {
        return Ok(());
    }

    let pkgs_dir = keg_path.join(CASK_PKGS_DIR);
    for pkg in &cask.pkgs {
        run_cask_pkg_installer(&pkgs_dir.join(basename_path(&pkg.source)?))?;
    }
    Ok(())
}

fn install_cask_installers(
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.installers.is_empty() {
        return Ok(());
    }

    for installer in &cask.installers {
        match installer {
            CaskInstaller::Manual { source } => {
                let path = keg_path.join(CASK_INSTALLERS_DIR).join(source);
                println!(
                    "Cask {} only provides a manual installer. To complete installation, open {}",
                    cask.token,
                    path.display()
                );
            }
            CaskInstaller::Script {
                executable,
                args,
                sudo,
            } => {
                let path = keg_path.join(CASK_INSTALLERS_DIR).join(executable);
                run_cask_script_installer(&path, args, *sudo)?;
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn run_cask_pkg_installer(pkg_path: &Path) -> Result<(), Error> {
    run_cask_pkg_installer_command("/usr/sbin/installer", pkg_path)
}

#[cfg(not(target_os = "macos"))]
fn run_cask_pkg_installer(_pkg_path: &Path) -> Result<(), Error> {
    Err(Error::InvalidArgument {
        message: "pkg casks are only supported on macOS".to_string(),
    })
}

#[cfg(target_os = "macos")]
fn run_cask_pkg_installer_command(installer: &str, pkg_path: &Path) -> Result<(), Error> {
    let status = Command::new(installer)
        .args(["-pkg"])
        .arg(pkg_path)
        .args(["-target", "/"])
        .status()
        .map_err(Error::exec("failed to run cask pkg installer"))?;

    if status.success() {
        Ok(())
    } else {
        Err(Error::ExecutionError {
            message: format!(
                "cask pkg installer failed for '{}' with status {status}",
                pkg_path.display()
            ),
        })
    }
}

fn basename_path(path: &str) -> Result<String, Error> {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("invalid cask path '{path}'"),
        })
}

fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(Error::store("failed to read cask installer metadata"))?
            .permissions();
        if perms.mode() & 0o111 == 0 {
            perms.set_mode(0o755);
            fs::set_permissions(path, perms)
                .map_err(Error::store("failed to make cask installer executable"))?;
        }
    }
    Ok(())
}

fn run_cask_script_installer(executable: &Path, args: &[String], sudo: bool) -> Result<(), Error> {
    let status = cask_script_installer_command(executable, args, sudo)
        .status()
        .map_err(Error::exec("failed to run cask installer script"))?;

    if status.success() {
        Ok(())
    } else {
        Err(Error::ExecutionError {
            message: format!(
                "cask installer script failed for '{}' with status {status}",
                executable.display()
            ),
        })
    }
}

fn cask_script_installer_command(executable: &Path, args: &[String], sudo: bool) -> Command {
    if sudo {
        let mut command = Command::new("sudo");
        command.arg(executable).args(args);
        command
    } else {
        let mut command = Command::new(executable);
        command.args(args);
        command
    }
}

fn link_cask_apps(
    keg_path: &Path,
    app_dir: &Path,
    cask: &crate::installer::cask::ResolvedCask,
    force: bool,
) -> Result<(), Error> {
    if cask.apps.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(app_dir).map_err(Error::store("failed to create app directory"))?;
    let staged_apps_dir = keg_path.join(CASK_APPS_DIR);

    for app in &cask.apps {
        let source = staged_apps_dir.join(&app.target);
        let target = app_dir.join(&app.target);

        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' app '{}' was not staged correctly",
                    cask.token, app.target
                ),
            });
        }

        if target.symlink_metadata().is_ok() && force {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask app"))?;
        } else if target.symlink_metadata().is_ok() {
            return Err(Error::LinkConflict {
                conflicts: vec![zb_core::ConflictedLink {
                    path: target,
                    owned_by: None,
                }],
            });
        }

        move_cask_app_to_target(&source, &target)?;
    }

    Ok(())
}

fn link_cask_fonts(
    keg_path: &Path,
    font_dir: &Path,
    cask: &crate::installer::cask::ResolvedCask,
    force: bool,
) -> Result<(), Error> {
    if cask.fonts.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(font_dir).map_err(Error::store("failed to create font directory"))?;
    let staged_fonts_dir = keg_path.join(CASK_FONTS_DIR);

    for font in &cask.fonts {
        let source = staged_fonts_dir.join(&font.target);
        let target = font_dir.join(&font.target);

        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' font '{}' was not staged correctly",
                    cask.token, font.target
                ),
            });
        }

        if target.symlink_metadata().is_ok() && force {
            remove_path_any(&target)
                .map_err(Error::store("failed to replace existing cask font"))?;
        } else if target.symlink_metadata().is_ok() {
            return Err(Error::LinkConflict {
                conflicts: vec![zb_core::ConflictedLink {
                    path: target,
                    owned_by: None,
                }],
            });
        }

        move_cask_artifact_to_target(&source, &target, "font")?;
    }

    Ok(())
}

fn move_cask_app_to_target(source: &Path, target: &Path) -> Result<(), Error> {
    move_cask_artifact_to_target(source, target, "app")
}

fn move_cask_artifact_to_target(source: &Path, target: &Path, kind: &str) -> Result<(), Error> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create cask {kind} parent directory: {e}"),
        })?;
    }

    match fs::rename(source, target) {
        Ok(()) => {}
        Err(_) => {
            copy_path_recursive(source, target)?;
            remove_path_any(source).map_err(|e| Error::StoreCorruption {
                message: format!("failed to remove staged cask {kind} after copy: {e}"),
            })?;
        }
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(target, source).map_err(|e| Error::StoreCorruption {
        message: format!("failed to create staged cask {kind} symlink: {e}"),
    })?;

    Ok(())
}

pub(super) fn uninstall_cask_apps(keg_path: &Path) -> Result<(), Error> {
    uninstall_moved_cask_artifacts(&keg_path.join(CASK_APPS_DIR), "app")
}

pub(super) fn uninstall_cask_fonts(keg_path: &Path) -> Result<(), Error> {
    uninstall_moved_cask_artifacts(&keg_path.join(CASK_FONTS_DIR), "font")
}

fn uninstall_moved_cask_artifacts(artifact_dir: &Path, kind: &str) -> Result<(), Error> {
    if !artifact_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(artifact_dir).map_err(|e| Error::StoreCorruption {
        message: format!("failed to read cask {kind} dir: {e}"),
    })? {
        let entry = entry.map_err(|e| Error::StoreCorruption {
            message: format!("failed to read cask {kind} entry: {e}"),
        })?;
        let staged_path = entry.path();
        if !staged_path.is_symlink() {
            continue;
        }

        let target = resolve_staged_cask_target(&staged_path)?;
        if target.exists() {
            remove_path_any(&target).map_err(|e| Error::StoreCorruption {
                message: format!("failed to remove installed cask {kind}: {e}"),
            })?;
        }
    }

    Ok(())
}

fn resolve_staged_cask_target(staged_path: &Path) -> Result<PathBuf, Error> {
    let target = fs::read_link(staged_path).map_err(Error::store("failed to read app symlink"))?;
    Ok(if target.is_relative() {
        staged_path.parent().unwrap_or(Path::new("")).join(target)
    } else {
        target
    })
}

fn remove_path_any(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::cellar::Cellar;
    use crate::storage::db::Database;

    use super::*;

    #[test]
    fn dependency_cellar_path_uses_formula_token_for_tap_name() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();
        let path = dependency_cellar_path(&cellar, "hashicorp/tap/terraform", "1.10.0");

        assert!(path.ends_with("cellar/terraform/1.10.0"));
    }

    #[test]
    fn dependency_cellar_path_keeps_core_formula_name() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();
        let path = dependency_cellar_path(&cellar, "openssl@3", "3.3.2");

        assert!(path.ends_with("cellar/openssl@3/3.3.2"));
    }

    #[test]
    fn dependency_cellar_path_uses_name_from_db_record() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();

        let db_path = tmp.path().join("zb.sqlite3");
        let mut db = Database::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.record_install("hashicorp/tap/terraform", "1.10.0", "store-key")
            .unwrap();
        tx.commit().unwrap();

        let keg = db.get_installed("hashicorp/tap/terraform").unwrap();
        let path = dependency_cellar_path(&cellar, &keg.name, &keg.version);

        assert!(path.ends_with("cellar/terraform/1.10.0"));
    }

    #[test]
    fn stage_raw_cask_binary_copies_and_marks_executable() {
        let tmp = TempDir::new().unwrap();
        let blob_path = tmp.path().join("claude");
        fs::write(&blob_path, b"#!/bin/sh\necho hello").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:claude-code".to_string(),
            token: "claude-code".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/claude".to_string(),
            sha256: "aaa".to_string(),
            binaries: vec![crate::installer::cask::CaskBinary {
                source: "claude".to_string(),
                target: "claude".to_string(),
            }],
            apps: vec![],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        stage_raw_cask_binary(&blob_path, &keg_path, &cask).unwrap();

        let target = keg_path.join("bin/claude");
        assert!(target.exists());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "#!/bin/sh\necho hello"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755);
        }
    }

    #[test]
    fn stage_raw_cask_binary_rejects_multiple_binaries() {
        let tmp = TempDir::new().unwrap();
        let blob_path = tmp.path().join("blob");
        fs::write(&blob_path, b"data").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:multi".to_string(),
            token: "multi".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/multi".to_string(),
            sha256: "bbb".to_string(),
            binaries: vec![
                crate::installer::cask::CaskBinary {
                    source: "a".to_string(),
                    target: "a".to_string(),
                },
                crate::installer::cask::CaskBinary {
                    source: "b".to_string(),
                    target: "b".to_string(),
                },
            ],
            apps: vec![],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        let err = stage_raw_cask_binary(&blob_path, &keg_path, &cask).unwrap_err();
        assert!(err.to_string().contains("unsupported raw download layout"));
    }

    #[test]
    fn stage_cask_artifacts_supports_appdir_binary_sources() {
        let tmp = TempDir::new().unwrap();
        let extracted_root = tmp.path().join("extract");
        let app_binary_dir =
            extracted_root.join("Visual Studio Code.app/Contents/Resources/app/bin");
        fs::create_dir_all(&app_binary_dir).unwrap();
        fs::write(app_binary_dir.join("code"), b"#!/bin/sh\necho code").unwrap();
        fs::write(
            extracted_root.join("Visual Studio Code.app/Contents/Info.plist"),
            b"plist",
        )
        .unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:visual-studio-code".to_string(),
            token: "visual-studio-code".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/vscode.zip".to_string(),
            sha256: "ccc".to_string(),
            binaries: vec![crate::installer::cask::CaskBinary {
                source: "$APPDIR/Visual Studio Code.app/Contents/Resources/app/bin/code"
                    .to_string(),
                target: "code".to_string(),
            }],
            apps: vec![crate::installer::cask::CaskApp {
                source: "Visual Studio Code.app".to_string(),
                target: "Visual Studio Code.app".to_string(),
            }],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        stage_cask_artifacts(&extracted_root, &keg_path, &cask).unwrap();

        assert_eq!(
            fs::read_to_string(keg_path.join("bin/code")).unwrap(),
            "#!/bin/sh\necho code"
        );
        assert!(
            keg_path
                .join("Applications/Visual Studio Code.app/Contents/Info.plist")
                .exists()
        );
    }

    #[test]
    fn stage_cask_artifacts_supports_resolved_appdir_binary_sources() {
        let tmp = TempDir::new().unwrap();
        let extracted_root = tmp.path().join("extract");
        let app_binary_dir = extracted_root.join("OmniWM.app/Contents/MacOS");
        fs::create_dir_all(&app_binary_dir).unwrap();
        fs::write(app_binary_dir.join("omniwmctl"), b"#!/bin/sh\necho omni").unwrap();
        fs::write(
            extracted_root.join("OmniWM.app/Contents/Info.plist"),
            b"plist",
        )
        .unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:omniwm".to_string(),
            token: "omniwm".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/omniwm.zip".to_string(),
            sha256: "fff".to_string(),
            binaries: vec![crate::installer::cask::CaskBinary {
                source: "/Applications/OmniWM.app/Contents/MacOS/omniwmctl".to_string(),
                target: "omniwmctl".to_string(),
            }],
            apps: vec![crate::installer::cask::CaskApp {
                source: "OmniWM.app".to_string(),
                target: "OmniWM.app".to_string(),
            }],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        stage_cask_artifacts(&extracted_root, &keg_path, &cask).unwrap();

        assert_eq!(
            fs::read_to_string(keg_path.join("bin/omniwmctl")).unwrap(),
            "#!/bin/sh\necho omni"
        );
    }

    #[test]
    fn stage_cask_artifacts_stages_pkg_sources() {
        let tmp = TempDir::new().unwrap();
        let extracted_root = tmp.path().join("extract");
        fs::create_dir_all(extracted_root.join("Nested")).unwrap();
        fs::write(extracted_root.join("Nested/Test.pkg"), b"pkg").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:pkg-test".to_string(),
            token: "pkg-test".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/pkg-test.zip".to_string(),
            sha256: "abc".to_string(),
            binaries: vec![],
            apps: vec![],
            fonts: vec![],
            pkgs: vec![crate::installer::cask::CaskPkg {
                source: "Nested/Test.pkg".to_string(),
            }],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        stage_cask_artifacts(&extracted_root, &keg_path, &cask).unwrap();

        assert_eq!(
            fs::read(keg_path.join("Packages/Test.pkg")).unwrap(),
            b"pkg"
        );
    }

    #[test]
    fn stage_cask_artifacts_stages_installer_app_roots() {
        let tmp = TempDir::new().unwrap();
        let extracted_root = tmp.path().join("extract");
        let executable = extracted_root.join("Install.app/Contents/MacOS/install");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"#!/bin/sh\nexit 0").unwrap();
        fs::write(
            extracted_root.join("Install.app/Contents/Info.plist"),
            b"plist",
        )
        .unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:installer-test".to_string(),
            token: "installer-test".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/installer-test.zip".to_string(),
            sha256: "abc".to_string(),
            binaries: vec![],
            apps: vec![],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![crate::installer::cask::CaskInstaller::Script {
                executable: "Install.app/Contents/MacOS/install".to_string(),
                args: vec![],
                sudo: false,
            }],
            appimages: vec![],
            artifacts: vec![],
        };

        stage_cask_artifacts(&extracted_root, &keg_path, &cask).unwrap();

        assert!(
            keg_path
                .join("Installers/Install.app/Contents/Info.plist")
                .exists()
        );
        assert!(
            keg_path
                .join("Installers/Install.app/Contents/MacOS/install")
                .exists()
        );
    }

    #[test]
    fn stage_cask_artifacts_stages_appimages_and_generic_artifacts() {
        let tmp = TempDir::new().unwrap();
        let extracted_root = tmp.path().join("extract");
        fs::create_dir_all(extracted_root.join("share")).unwrap();
        fs::write(extracted_root.join("Tool.AppImage"), b"appimage").unwrap();
        fs::write(extracted_root.join("share/data.txt"), b"data").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:linux-tool".to_string(),
            token: "linux-tool".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/linux-tool.tar.gz".to_string(),
            sha256: "abc".to_string(),
            binaries: vec![],
            apps: vec![],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![crate::installer::cask::CaskAppImage {
                source: "Tool.AppImage".to_string(),
                target: "linux-tool".to_string(),
            }],
            artifacts: vec![crate::installer::cask::CaskGenericArtifact {
                source: "share".to_string(),
                target: "share/linux-tool".to_string(),
            }],
        };

        stage_cask_artifacts(&extracted_root, &keg_path, &cask).unwrap();

        assert_eq!(
            fs::read(keg_path.join("bin/linux-tool")).unwrap(),
            b"appimage"
        );
        assert_eq!(
            fs::read(keg_path.join("share/linux-tool/data.txt")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn parses_hdiutil_mount_points() {
        let plist = r#"
        <plist version="1.0">
        <dict>
          <key>system-entities</key>
          <array>
            <dict>
              <key>mount-point</key>
              <string>/private/tmp/homebrew-dmg/Test &amp; App</string>
            </dict>
          </array>
        </dict>
        </plist>
        "#;

        let mount_points = parse_hdiutil_mount_points(plist);
        assert_eq!(mount_points.len(), 1);
        assert_eq!(
            mount_points[0],
            PathBuf::from("/private/tmp/homebrew-dmg/Test & App")
        );
    }

    #[test]
    fn ca_certificates_post_install_writes_prefix_bundle() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");
        let keg_path = tmp.path().join("Cellar/ca-certificates/2026-03-19");
        let source = keg_path.join("share/ca-certificates/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"certs").unwrap();

        run_builtin_post_install(&prefix, "ca-certificates", &keg_path).unwrap();

        assert_eq!(
            fs::read(prefix.join("etc/ca-certificates/cert.pem")).unwrap(),
            b"certs"
        );
    }

    #[test]
    fn link_cask_apps_moves_app_and_leaves_keg_symlink() {
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("keg");
        let app_dir = tmp.path().join("Applications");
        let staged_app = keg_path.join("Applications/Test.app");
        fs::create_dir_all(staged_app.join("Contents")).unwrap();
        fs::write(staged_app.join("Contents/Info.plist"), b"plist").unwrap();

        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:test".to_string(),
            token: "test".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/test.zip".to_string(),
            sha256: "ddd".to_string(),
            binaries: vec![],
            apps: vec![crate::installer::cask::CaskApp {
                source: "Test.app".to_string(),
                target: "Test.app".to_string(),
            }],
            fonts: vec![],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        link_cask_apps(&keg_path, &app_dir, &cask, false).unwrap();

        assert!(app_dir.join("Test.app/Contents/Info.plist").exists());
        assert!(staged_app.is_symlink());
        assert_eq!(
            resolve_staged_cask_target(&staged_app).unwrap(),
            app_dir.join("Test.app")
        );

        uninstall_cask_apps(&keg_path).unwrap();
        assert!(!app_dir.join("Test.app").exists());
    }

    #[test]
    fn link_cask_fonts_moves_font_and_leaves_keg_symlink() {
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("keg");
        let font_dir = tmp.path().join("Fonts");
        let staged_font = keg_path.join("Fonts/Test-Regular.otf");
        fs::create_dir_all(staged_font.parent().unwrap()).unwrap();
        fs::write(&staged_font, b"font").unwrap();

        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:font-test".to_string(),
            token: "font-test".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/font.zip".to_string(),
            sha256: "eee".to_string(),
            binaries: vec![],
            apps: vec![],
            fonts: vec![crate::installer::cask::CaskFont {
                source: "Test-Regular.otf".to_string(),
                target: "Test-Regular.otf".to_string(),
            }],
            pkgs: vec![],
            installers: vec![],
            appimages: vec![],
            artifacts: vec![],
        };

        link_cask_fonts(&keg_path, &font_dir, &cask, false).unwrap();

        assert_eq!(
            fs::read_to_string(font_dir.join("Test-Regular.otf")).unwrap(),
            "font"
        );
        assert!(staged_font.is_symlink());

        uninstall_cask_fonts(&keg_path).unwrap();
        assert!(!font_dir.join("Test-Regular.otf").exists());
    }
}
