use console::style;

pub fn execute(installer: &mut zb_io::Installer, cask_only: bool) -> Result<(), zb_core::Error> {
    let installed = installer.list_installed()?;
    let installed: Vec<_> = installed
        .into_iter()
        .filter(|keg| !cask_only || keg.name.starts_with("cask:"))
        .collect();

    if installed.is_empty() {
        if cask_only {
            println!("No casks installed.");
        } else {
            println!("No formulas installed.");
        }
    } else {
        for keg in installed {
            let name = keg.name.strip_prefix("cask:").unwrap_or(&keg.name);
            println!("{} {}", style(name).bold(), style(&keg.version).dim());
        }
    }

    Ok(())
}
