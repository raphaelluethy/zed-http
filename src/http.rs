use std::{env, fs};

use zed_extension_api::{self as zed, settings::LspSettings};

const LSP_BINARY: &str = "zed-http-lsp";
const RELEASE_REPOSITORY: &str = "raphaelluethy/zed-http";
const HTTPYAC_PACKAGE: &str = "httpyac";
const HTTPYAC_VERSION: &str = "6.16.7";
const HTTPYAC_SCRIPT: &str = "node_modules/httpyac/bin/httpyac.js";
// These names form a contract with the language server in http-lsp/src/backend.rs,
// which reads them at startup; they must stay in sync.
const HTTPYAC_NODE_ENV: &str = "ZED_HTTPYAC_NODE";
const HTTPYAC_SCRIPT_ENV: &str = "ZED_HTTPYAC_SCRIPT";
const HTTPYAC_PATH_ENV: &str = "ZED_HTTPYAC_PATH";

struct HttpExtension {
    cached_binary_path: Option<String>,
    did_find_httpyac: bool,
}

struct PlatformAsset {
    archive_name: String,
    binary_name: String,
    file_type: zed::DownloadedFileType,
    make_executable: bool,
}

impl HttpExtension {
    fn language_server_binary_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
        configured_path: Option<String>,
    ) -> zed::Result<String> {
        if let Some(path) = configured_path.filter(|path| !path.is_empty()) {
            return Ok(path);
        }

        if let Some(path) = worktree.which(LSP_BINARY) {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
                return Ok(path.clone());
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = zed::latest_github_release(
            RELEASE_REPOSITORY,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|error| {
            format!(
                "failed to find a {LSP_BINARY} release in {RELEASE_REPOSITORY}: {error}. \
                 Build it with `cargo build --release --package {LSP_BINARY}` and configure \
                 `lsp.{LSP_BINARY}.binary.path` to use a local binary"
            )
        })?;

        let platform_asset = platform_asset()?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == platform_asset.archive_name)
            .ok_or_else(|| {
                format!(
                    "release {} does not contain the required asset {}",
                    release.version, platform_asset.archive_name
                )
            })?;

        let version = sanitize_path_component(&release.version);
        let version_dir = format!("{LSP_BINARY}-{version}");
        let binary_path = format!("{version_dir}/{}", platform_asset.binary_name);

        if !fs::metadata(&binary_path).is_ok_and(|metadata| metadata.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            zed::download_file(&asset.download_url, &version_dir, platform_asset.file_type)
                .map_err(|error| {
                    format!(
                        "failed to download {} from release {}: {error}",
                        platform_asset.archive_name, release.version
                    )
                })?;

            if platform_asset.make_executable {
                zed::make_file_executable(&binary_path)?;
            }

            remove_old_server_versions(&version_dir)?;
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }

    fn httpyac_runtime(
        &mut self,
        language_server_id: &zed::LanguageServerId,
    ) -> zed::Result<(String, String)> {
        let script_exists =
            || fs::metadata(HTTPYAC_SCRIPT).is_ok_and(|metadata| metadata.is_file());

        if !self.did_find_httpyac {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::CheckingForUpdate,
            );

            let needs_install = match zed::npm_package_installed_version(HTTPYAC_PACKAGE) {
                Ok(version) => version.as_deref() != Some(HTTPYAC_VERSION),
                Err(_) if script_exists() => false,
                Err(error) => {
                    return Err(format!(
                        "failed to inspect the installed {HTTPYAC_PACKAGE} package: {error}"
                    ))
                }
            };

            if needs_install {
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Downloading,
                );

                if let Err(error) = zed::npm_install_package(HTTPYAC_PACKAGE, HTTPYAC_VERSION) {
                    if !script_exists() {
                        return Err(format!(
                            "failed to install {HTTPYAC_PACKAGE}@{HTTPYAC_VERSION}: {error}"
                        ));
                    }
                }
            }

            if !script_exists() {
                return Err(format!(
                    "installed {HTTPYAC_PACKAGE}@{HTTPYAC_VERSION} did not contain {HTTPYAC_SCRIPT}"
                ));
            }

            self.did_find_httpyac = true;
        }

        let current_dir = env::current_dir()
            .map_err(|error| format!("failed to locate the extension directory: {error}"))?;
        let script_path = current_dir
            .join(HTTPYAC_SCRIPT)
            .to_string_lossy()
            .into_owned();

        Ok((zed::node_binary_path()?, script_path))
    }
}

impl zed::Extension for HttpExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
            did_find_httpyac: false,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let lsp_settings =
            LspSettings::for_worktree(language_server_id.as_ref(), worktree).unwrap_or_default();
        let httpyac_path = configured_httpyac_path(lsp_settings.settings.as_ref());
        let binary_settings = lsp_settings.binary;
        let configured_path = binary_settings
            .as_ref()
            .and_then(|settings| settings.path.clone());
        let args = binary_settings
            .as_ref()
            .and_then(|settings| settings.arguments.clone())
            .unwrap_or_default();

        let command =
            self.language_server_binary_path(language_server_id, worktree, configured_path)?;

        let mut command_env = worktree.shell_env();
        if let Some(overrides) = binary_settings.and_then(|settings| settings.env) {
            for (key, value) in overrides {
                set_env(&mut command_env, key, value);
            }
        }
        if let Some(path) = httpyac_path {
            set_env(&mut command_env, HTTPYAC_PATH_ENV.into(), path);
        } else {
            let (node_path, httpyac_script) = self.httpyac_runtime(language_server_id)?;
            set_env(&mut command_env, HTTPYAC_NODE_ENV.into(), node_path);
            set_env(&mut command_env, HTTPYAC_SCRIPT_ENV.into(), httpyac_script);
        }

        Ok(zed::Command {
            command,
            args,
            env: command_env,
        })
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        Ok(
            LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                .ok()
                .and_then(|settings| settings.settings),
        )
    }
}

fn platform_asset() -> zed::Result<PlatformAsset> {
    let (os, architecture) = zed::current_platform();
    let target = match (os, architecture) {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
        (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
        (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
        (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
        (os, architecture) => {
            return Err(format!(
                "{LSP_BINARY} does not publish a binary for {os:?}/{architecture:?}"
            ))
        }
    };

    let is_windows = os == zed::Os::Windows;
    Ok(PlatformAsset {
        archive_name: format!(
            "{LSP_BINARY}-{target}.{}",
            if is_windows { "zip" } else { "tar.gz" }
        ),
        binary_name: if is_windows {
            format!("{LSP_BINARY}.exe")
        } else {
            LSP_BINARY.to_owned()
        },
        file_type: if is_windows {
            zed::DownloadedFileType::Zip
        } else {
            zed::DownloadedFileType::GzipTar
        },
        make_executable: !is_windows,
    })
}

fn remove_old_server_versions(active_version_dir: &str) -> zed::Result<()> {
    let entries = fs::read_dir(".")
        .map_err(|error| format!("failed to inspect the extension directory: {error}"))?;

    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to inspect an extension entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if name.starts_with(&format!("{LSP_BINARY}-")) && name != active_version_dir {
            let path = entry.path();
            // Best-effort cleanup; a stale version must never block startup.
            if path.is_dir() {
                fs::remove_dir_all(path).ok();
            } else {
                fs::remove_file(path).ok();
            }
        }
    }

    Ok(())
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn configured_httpyac_path(settings: Option<&zed::serde_json::Value>) -> Option<String> {
    settings
        .and_then(|settings| settings.pointer("/httpyac/path"))
        .and_then(zed::serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

fn set_env(env: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some((_, existing_value)) = env
        .iter_mut()
        .find(|(existing_key, _)| *existing_key == key)
    {
        *existing_value = value;
    } else {
        env.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::{configured_httpyac_path, requires_managed_httpyac};
    use zed_extension_api::serde_json::json;

    #[test]
    fn custom_httpyac_path_does_not_require_managed_installation() {
        let settings = json!({"httpyac": {"path": "/custom/bin/httpyac"}});

        assert!(!requires_managed_httpyac(Some(&settings)));
        assert_eq!(
            configured_httpyac_path(Some(&settings)).as_deref(),
            Some("/custom/bin/httpyac")
        );
    }

    #[test]
    fn empty_httpyac_path_requires_managed_installation() {
        let settings = json!({"httpyac": {"path": ""}});

        assert!(requires_managed_httpyac(Some(&settings)));
        assert_eq!(configured_httpyac_path(Some(&settings)), None);
    }
}

zed::register_extension!(HttpExtension);
