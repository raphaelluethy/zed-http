use std::{env, fs, path::Path};

use zed_extension_api::{self as zed, settings::LspSettings, LanguageServerInstallationStatus};

const KULALA_SERVER_ID: &str = "kulala-ls";
const KULALA_PACKAGE_NAME: &str = "@mistweaverco/kulala-ls";
const KULALA_PACKAGE_VERSION: &str = "1.11.1";
const KULALA_SERVER_PATH: &str = "node_modules/@mistweaverco/kulala-ls/cli.cjs";

const EXECUTION_SERVER_ID: &str = "zed-http-lsp";
const EXECUTION_SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_REPOSITORY: &str = "raphaelluethy/zed-http";

struct HttpExtension {
    cached_execution_server_path: Option<String>,
}

struct PlatformAsset {
    archive_name: String,
    binary_name: String,
    file_type: zed::DownloadedFileType,
    make_executable: bool,
}

impl HttpExtension {
    fn install_kulala_language_server(
        &self,
        language_server_id: &zed::LanguageServerId,
    ) -> zed::Result<()> {
        let installed_version = zed::npm_package_installed_version(KULALA_PACKAGE_NAME)?;

        if installed_version.as_deref() != Some(KULALA_PACKAGE_VERSION) {
            zed::set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Downloading,
            );

            if let Err(error) =
                zed::npm_install_package(KULALA_PACKAGE_NAME, KULALA_PACKAGE_VERSION)
            {
                // An older installation is still usable, for example while offline.
                if installed_version.is_some() && Path::new(KULALA_SERVER_PATH).is_file() {
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &LanguageServerInstallationStatus::None,
                    );
                    return Ok(());
                }
                let message = format!(
                    "Failed to download {KULALA_PACKAGE_NAME} {KULALA_PACKAGE_VERSION}: {error}"
                );
                zed::set_language_server_installation_status(
                    language_server_id,
                    &LanguageServerInstallationStatus::Failed(message.clone()),
                );
                return Err(message);
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        Ok(())
    }

    fn kulala_command(
        &self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        self.install_kulala_language_server(language_server_id)?;
        let server_path = env::current_dir()
            .map_err(|error| format!("Failed to locate the extension directory: {error}"))?
            .join(KULALA_SERVER_PATH)
            .to_string_lossy()
            .into_owned();

        Ok(zed::Command {
            command: zed::node_binary_path()?,
            args: vec![server_path, "--stdio".to_owned()],
            env: worktree.shell_env(),
        })
    }

    fn execution_server_binary_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        configured_path: Option<String>,
    ) -> zed::Result<String> {
        if let Some(path) = configured_path.filter(|path| !path.is_empty()) {
            return Ok(path);
        }
        let asset = execution_server_asset()?;
        if let Some(path) = &self.cached_execution_server_path {
            if is_installed(path) {
                return Ok(path.clone());
            }
        }

        let version_directory = format!("{EXECUTION_SERVER_ID}-{EXECUTION_SERVER_VERSION}");
        let binary_path = format!("{version_directory}/{}", asset.binary_name);
        if !is_installed(&binary_path) {
            match download_execution_server(language_server_id, &asset, &version_directory) {
                Ok(()) => remove_old_execution_servers(&version_directory),
                Err(error) => {
                    // Keep request execution working offline or when a release is not yet
                    // published by falling back to a previously downloaded adapter.
                    if let Some(previous) = previously_installed_execution_server(&asset) {
                        zed::set_language_server_installation_status(
                            language_server_id,
                            &LanguageServerInstallationStatus::None,
                        );
                        self.cached_execution_server_path = Some(previous.clone());
                        return Ok(previous);
                    }
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &LanguageServerInstallationStatus::Failed(error.clone()),
                    );
                    return Err(error);
                }
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        self.cached_execution_server_path = Some(binary_path.clone());
        Ok(binary_path)
    }

    fn execution_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let settings =
            LspSettings::for_worktree(language_server_id.as_ref(), worktree).unwrap_or_default();
        let binary_settings = settings.binary;
        let configured_path = binary_settings
            .as_ref()
            .and_then(|settings| settings.path.clone());
        let args = binary_settings
            .as_ref()
            .and_then(|settings| settings.arguments.clone())
            .unwrap_or_default();
        let command = self.execution_server_binary_path(language_server_id, configured_path)?;

        let mut command_env = worktree.shell_env();
        if let Some(overrides) = binary_settings.and_then(|settings| settings.env) {
            for (key, value) in overrides {
                set_env(&mut command_env, key, value);
            }
        }

        Ok(zed::Command {
            command,
            args,
            env: command_env,
        })
    }
}

impl zed::Extension for HttpExtension {
    fn new() -> Self {
        Self {
            cached_execution_server_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        match language_server_id.as_ref() {
            KULALA_SERVER_ID => self.kulala_command(language_server_id, worktree),
            EXECUTION_SERVER_ID => self.execution_server_command(language_server_id, worktree),
            other => Err(format!("unknown HTTP language server {other}")),
        }
    }
}

fn execution_server_asset() -> zed::Result<PlatformAsset> {
    let (os, architecture) = zed::current_platform();
    execution_server_asset_for(os, architecture)
}

fn execution_server_asset_for(
    os: zed::Os,
    architecture: zed::Architecture,
) -> zed::Result<PlatformAsset> {
    let target = match (os, architecture) {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
        (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
        (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
        (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
        (os, architecture) => {
            return Err(format!(
                "{EXECUTION_SERVER_ID} does not publish a binary for {os:?}/{architecture:?}"
            ))
        }
    };
    let is_windows = os == zed::Os::Windows;

    Ok(PlatformAsset {
        archive_name: format!(
            "{EXECUTION_SERVER_ID}-{target}.{}",
            if is_windows { "zip" } else { "tar.gz" }
        ),
        binary_name: if is_windows {
            format!("{EXECUTION_SERVER_ID}.exe")
        } else {
            EXECUTION_SERVER_ID.to_owned()
        },
        file_type: if is_windows {
            zed::DownloadedFileType::Zip
        } else {
            zed::DownloadedFileType::GzipTar
        },
        make_executable: !is_windows,
    })
}

fn download_execution_server(
    language_server_id: &zed::LanguageServerId,
    asset: &PlatformAsset,
    version_directory: &str,
) -> zed::Result<()> {
    let unavailable = |reason: String| {
        format!(
            "sending requests is unavailable because {reason}. Completion and hover from Kulala LS \
             still work. To use a local build of {EXECUTION_SERVER_ID}, set \
             `lsp.{EXECUTION_SERVER_ID}.binary.path`."
        )
    };
    let tag = format!("v{EXECUTION_SERVER_VERSION}");
    let release = zed::github_release_by_tag_name(RELEASE_REPOSITORY, &tag).map_err(|error| {
        unavailable(format!(
            "the {EXECUTION_SERVER_ID} {tag} release could not be found ({error})"
        ))
    })?;
    let archive = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset.archive_name)
        .ok_or_else(|| {
            unavailable(format!(
                "the {EXECUTION_SERVER_ID} {tag} release has no {} archive",
                asset.archive_name
            ))
        })?;

    zed::set_language_server_installation_status(
        language_server_id,
        &LanguageServerInstallationStatus::Downloading,
    );
    zed::download_file(&archive.download_url, version_directory, asset.file_type).map_err(
        |error| {
            unavailable(format!(
                "downloading {} failed ({error})",
                asset.archive_name
            ))
        },
    )?;
    if asset.make_executable {
        zed::make_file_executable(&format!("{version_directory}/{}", asset.binary_name))?;
    }
    Ok(())
}

fn is_installed(binary_path: &str) -> bool {
    Path::new(binary_path).is_file()
}

fn installed_execution_server_directories() -> Vec<String> {
    let Ok(entries) = fs::read_dir(".") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with(&format!("{EXECUTION_SERVER_ID}-")))
        .collect()
}

fn previously_installed_execution_server(asset: &PlatformAsset) -> Option<String> {
    installed_execution_server_directories()
        .into_iter()
        .map(|directory| format!("{directory}/{}", asset.binary_name))
        .filter(|binary_path| is_installed(binary_path))
        .max_by_key(|binary_path| version_key(binary_path))
}

fn version_key(binary_path: &str) -> Vec<u64> {
    binary_path
        .split('/')
        .next()
        .and_then(|directory| directory.strip_prefix(&format!("{EXECUTION_SERVER_ID}-")))
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn remove_old_execution_servers(active_version_directory: &str) {
    for name in installed_execution_server_directories() {
        if name != active_version_directory {
            let path = Path::new(&name);
            if path.is_dir() {
                fs::remove_dir_all(path).ok();
            } else {
                fs::remove_file(path).ok();
            }
        }
    }
}

fn set_env(environment: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some((_, existing_value)) = environment
        .iter_mut()
        .find(|(existing_key, _)| *existing_key == key)
    {
        *existing_value = value;
    } else {
        environment.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_platforms_to_release_archives() {
        let mac = execution_server_asset_for(zed::Os::Mac, zed::Architecture::Aarch64).unwrap();
        assert_eq!(mac.archive_name, "zed-http-lsp-aarch64-apple-darwin.tar.gz");
        assert_eq!(mac.binary_name, "zed-http-lsp");
        assert!(mac.make_executable);

        let windows =
            execution_server_asset_for(zed::Os::Windows, zed::Architecture::X8664).unwrap();
        assert_eq!(
            windows.archive_name,
            "zed-http-lsp-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(windows.binary_name, "zed-http-lsp.exe");
        assert!(!windows.make_executable);

        let windows_arm =
            execution_server_asset_for(zed::Os::Windows, zed::Architecture::Aarch64).unwrap();
        assert_eq!(
            windows_arm.archive_name,
            "zed-http-lsp-aarch64-pc-windows-msvc.zip"
        );
        assert_eq!(windows_arm.binary_name, "zed-http-lsp.exe");
    }

    #[test]
    fn orders_installed_versions_numerically() {
        assert!(
            version_key("zed-http-lsp-0.0.10/zed-http-lsp")
                > version_key("zed-http-lsp-0.0.9/zed-http-lsp")
        );
    }

    #[test]
    fn release_manifests_share_the_extension_version() {
        let version_line = format!("version = \"{EXECUTION_SERVER_VERSION}\"");
        assert!(include_str!("../extension.toml").contains(&version_line));
        assert!(include_str!("../http-lsp/Cargo.toml").contains(&version_line));
        assert!(include_str!("../Cargo.toml").contains(&version_line));
    }
}

zed::register_extension!(HttpExtension);
