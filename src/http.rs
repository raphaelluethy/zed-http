use std::env;

use zed_extension_api::{self as zed, LanguageServerInstallationStatus};

const KULALA_PACKAGE_NAME: &str = "@mistweaverco/kulala-ls";
const KULALA_SERVER_PATH: &str = "node_modules/@mistweaverco/kulala-ls/cli.cjs";

struct HttpExtension;

impl HttpExtension {
    fn install_language_server(
        &self,
        language_server_id: &zed::LanguageServerId,
    ) -> zed::Result<()> {
        let latest_version = zed::npm_package_latest_version(KULALA_PACKAGE_NAME)?;
        let installed_version = zed::npm_package_installed_version(KULALA_PACKAGE_NAME)?;

        if installed_version.as_deref() != Some(latest_version.as_str()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Downloading,
            );

            if let Err(error) = zed::npm_install_package(KULALA_PACKAGE_NAME, &latest_version) {
                let message =
                    format!("Failed to download {KULALA_PACKAGE_NAME} {latest_version}: {error}");
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
}

impl zed::Extension for HttpExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        self.install_language_server(language_server_id)?;

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
}

zed::register_extension!(HttpExtension);
