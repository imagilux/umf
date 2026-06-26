//! `umf registry` — manage the operator's unqualified-search registry list.
//!
//! Bare references (`alpine:3.23`) resolve against Docker Hub by default. The
//! registries configured here are tried first, in order, before the `docker.io`
//! fallback, when resolving an unqualified `FROM` / `ADD` / `umf pull`. The list
//! is persisted at `$XDG_CONFIG_HOME/umf/registries.toml`.

use thiserror::Error;
use umf_oci::registry::SearchRegistries;

use crate::cli::RegistryAction;

#[derive(Debug, Error)]
pub(crate) enum CliRegistryError {
    #[error("registries config: {0}")]
    Io(#[from] std::io::Error),
}

pub(crate) fn run_registry(action: &RegistryAction) -> Result<(), CliRegistryError> {
    let mut cfg = SearchRegistries::load();
    match action {
        RegistryAction::Add { registry } => {
            if cfg.add(registry) {
                cfg.save()?;
                println!("Added search registry: {registry}");
            } else {
                println!("Already configured: {registry}");
            }
        }
        RegistryAction::Remove { registry } => {
            if cfg.remove(registry) {
                cfg.save()?;
                println!("Removed search registry: {registry}");
            } else {
                println!("Not configured: {registry}");
            }
        }
        RegistryAction::List => print_list(&cfg),
    }
    Ok(())
}

/// Print the ordered search list, making the implicit `docker.io` fallback and
/// the precedence (top = tried first) explicit.
fn print_list(cfg: &SearchRegistries) {
    if cfg.search.is_empty() {
        println!(
            "No search registries configured. Unqualified references (e.g. `alpine:3.23`) resolve against docker.io only."
        );
        if let Some(path) = SearchRegistries::config_path() {
            println!(
                "Add one with `umf registry add <registry>` (stored at {}).",
                path.display()
            );
        }
        return;
    }
    println!("Search registries (tried in order for an unqualified reference, then docker.io):");
    for (i, reg) in cfg.search.iter().enumerate() {
        println!("  {}. {reg}", i + 1);
    }
    println!("  {}. docker.io (implicit fallback)", cfg.search.len() + 1);
}
