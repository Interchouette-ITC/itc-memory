//! Shared configuration and domain primitives for itc-memory.

mod database_config;

use std::path::{Path, PathBuf};

pub use database_config::{DatabaseConfig, DatabaseConfigError};

/// Load the user configuration, or a nearby `.env` when none is configured.
///
/// Existing process environment variables take precedence. The returned directory
/// is the canonical location of the file, including when it is a symlink.
///
/// # Errors
/// Returns an error if an existing configuration cannot be read or parsed.
pub fn load_environment() -> Result<Option<PathBuf>, dotenvy::Error> {
    let user_config =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/itc-memory/.env"));
    let mut loaded = None;
    if let Some(path) = user_config {
        match dotenvy::from_path(&path) {
            Ok(()) => loaded = Some(path),
            Err(error) if error.not_found() => {}
            Err(error) => return Err(error),
        }
    }
    let path = match loaded {
        Some(path) => path,
        None => match dotenvy::dotenv() {
            Ok(path) => path,
            Err(error) if error.not_found() => return Ok(None),
            Err(error) => return Err(error),
        },
    };
    let path = std::fs::canonicalize(path).map_err(dotenvy::Error::Io)?;
    Ok(path.parent().map(Path::to_path_buf))
}

/// Optional local exact-checkout allowlist, compiled into the CLI and MCP binaries.
pub const EMBEDDED_INDEX_ROOTS: &str = include_str!(concat!(env!("OUT_DIR"), "/index-roots.txt"));

/// Resolve a configuration file path relative to the loaded `.env`, if any.
#[must_use]
pub fn resolve_config_path(path: impl AsRef<Path>, config_dir: Option<&Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(dir) = config_dir {
        dir.join(path)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_config_path;
    use std::path::Path;

    #[test]
    fn config_paths_use_the_environment_directory_only_when_relative() {
        let dir = Some(Path::new("/configuration"));
        assert_eq!(
            resolve_config_path("config/models.json", dir),
            Path::new("/configuration/config/models.json")
        );
        assert_eq!(
            resolve_config_path("/elsewhere/models.json", dir),
            Path::new("/elsewhere/models.json")
        );
        assert_eq!(
            resolve_config_path("config/models.json", None),
            Path::new("config/models.json")
        );
    }
}
