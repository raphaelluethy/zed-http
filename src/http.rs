use std::{env, fs, path::Path};

use zed_extension_api::{self as zed, settings::LspSettings, LanguageServerInstallationStatus};

const SERVER_ID: &str = "zed-http-lsp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_REPOSITORY: &str = "raphaelluethy/zed-http";
/// Marks the directory a download is extracted into before it is renamed into place.
const STAGING: &str = "staging";

struct HttpExtension {
    cached_server_path: Option<String>,
}

struct PlatformAsset {
    target: String,
    archive_name: String,
    binary_name: String,
    file_type: zed::DownloadedFileType,
    make_executable: bool,
}

impl HttpExtension {
    fn server_binary_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        configured_path: Option<String>,
    ) -> zed::Result<String> {
        if let Some(path) = configured_path.filter(|path| !path.is_empty()) {
            return Ok(path);
        }
        let asset = server_asset()?;
        if let Some(path) = &self.cached_server_path {
            if is_installed(path) {
                return Ok(path.clone());
            }
        }

        let version_directory = install_directory(&asset.target, SERVER_VERSION);
        let binary_path = format!("{version_directory}/{}", asset.binary_name);
        if !is_installed(&binary_path) {
            match download_server(language_server_id, &asset, &version_directory) {
                Ok(()) => remove_old_servers(&version_directory, &asset.target),
                Err(error) => {
                    // Keep working offline or when a release is not yet published by falling
                    // back to a previously downloaded server.
                    if let Some(previous) = previously_installed_server(&asset) {
                        zed::set_language_server_installation_status(
                            language_server_id,
                            &LanguageServerInstallationStatus::None,
                        );
                        self.cached_server_path = Some(previous.clone());
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
        self.cached_server_path = Some(binary_path.clone());
        Ok(binary_path)
    }

    fn server_command(
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
        let command = self.server_binary_path(language_server_id, configured_path)?;
        record_server_path(Path::new("active-servers"), &worktree.root_path(), &command)?;

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
            cached_server_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        match language_server_id.as_ref() {
            SERVER_ID => {
                remove_kulala_install();
                self.server_command(language_server_id, worktree)
            }
            other => Err(format!("unknown HTTP language server {other}")),
        }
    }
}

fn server_asset() -> zed::Result<PlatformAsset> {
    let (os, architecture) = zed::current_platform();
    server_asset_for(os, architecture)
}

fn server_asset_for(os: zed::Os, architecture: zed::Architecture) -> zed::Result<PlatformAsset> {
    let target = match (os, architecture) {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
        (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
        (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
        (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
        (os, architecture) => {
            return Err(format!(
                "{SERVER_ID} does not publish a binary for {os:?}/{architecture:?}"
            ))
        }
    };
    let is_windows = os == zed::Os::Windows;

    Ok(PlatformAsset {
        target: target.to_owned(),
        archive_name: format!(
            "{SERVER_ID}-{target}.{}",
            if is_windows { "zip" } else { "tar.gz" }
        ),
        binary_name: if is_windows {
            format!("{SERVER_ID}.exe")
        } else {
            SERVER_ID.to_owned()
        },
        file_type: if is_windows {
            zed::DownloadedFileType::Zip
        } else {
            zed::DownloadedFileType::GzipTar
        },
        make_executable: !is_windows,
    })
}

fn download_server(
    language_server_id: &zed::LanguageServerId,
    asset: &PlatformAsset,
    version_directory: &str,
) -> zed::Result<()> {
    let unavailable = |reason: String| {
        format!(
            "the HTTP language server is unavailable because {reason}. To use a local build of \
             {SERVER_ID}, set `lsp.{SERVER_ID}.binary.path`."
        )
    };
    let tag = format!("v{SERVER_VERSION}");
    let release = zed::github_release_by_tag_name(RELEASE_REPOSITORY, &tag).map_err(|error| {
        unavailable(format!(
            "the {SERVER_ID} {tag} release could not be found ({error})"
        ))
    })?;
    let archive = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset.archive_name)
        .ok_or_else(|| {
            unavailable(format!(
                "the {SERVER_ID} {tag} release has no {} archive",
                asset.archive_name
            ))
        })?;

    zed::set_language_server_installation_status(
        language_server_id,
        &LanguageServerInstallationStatus::Downloading,
    );
    // Extract into a staging directory and rename it into place only once the binary is
    // complete, so an interrupted download is never mistaken for an installation.
    let staging = format!("{SERVER_ID}-{STAGING}-{}", asset.target);
    remove_path(&staging);
    zed::download_file(&archive.download_url, &staging, asset.file_type).map_err(|error| {
        remove_path(&staging);
        unavailable(format!(
            "downloading {} failed ({error})",
            asset.archive_name
        ))
    })?;
    let staged_binary = format!("{staging}/{}", asset.binary_name);
    if !fs::metadata(&staged_binary).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    {
        remove_path(&staging);
        return Err(unavailable(format!(
            "{} did not contain {}",
            asset.archive_name, asset.binary_name
        )));
    }
    if asset.make_executable {
        zed::make_file_executable(&staged_binary)?;
    }
    remove_path(version_directory);
    fs::rename(&staging, version_directory).map_err(|error| {
        remove_path(&staging);
        unavailable(format!("installing {version_directory} failed ({error})"))
    })?;
    Ok(())
}

fn record_server_path(directory: &Path, root: &str, binary: &str) -> zed::Result<()> {
    if root.contains(['\r', '\n']) || binary.contains(['\r', '\n']) {
        return Err("HTTP adapter paths cannot contain line breaks".into());
    }
    let key = root
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let target = directory.join(format!("{key:016x}.path"));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    for attempt in 0..32u32 {
        let temporary = directory.join(format!("{key:016x}-{stamp:x}-{attempt:x}.tmp"));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        };
        let written =
            std::io::Write::write_all(&mut file, format!("{root}\n{binary}\n").as_bytes());
        drop(file);
        let result = written.and_then(|()| fs::rename(&temporary, &target));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result.map_err(|error| error.to_string());
    }
    Err("could not create an HTTP adapter record".to_owned())
}

fn is_installed(binary_path: &str) -> bool {
    fs::metadata(binary_path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

/// Installs are kept per target triple, because Zed running natively and under Rosetta share
/// the extension directory: `zed-http-lsp-<target>-<version>`.
fn install_directory(target: &str, version: &str) -> String {
    format!("{SERVER_ID}-{target}-{version}")
}

/// Splits an install directory name into its target (absent for installs made before 0.0.4)
/// and numeric version. Staging directories and other names yield `None`.
fn parse_install(name: &str) -> Option<(Option<&str>, Vec<u64>)> {
    let rest = name.strip_prefix(&format!("{SERVER_ID}-"))?;
    if rest.starts_with(&format!("{STAGING}-")) {
        return None;
    }
    let (target, version) = match rest.rsplit_once('-') {
        Some((target, version)) => (Some(target), version),
        None => (None, rest),
    };
    let version = version
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    Some((target, version))
}

fn installed_server_directories() -> Vec<String> {
    let Ok(entries) = fs::read_dir(".") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| parse_install(name).is_some())
        .collect()
}

fn previously_installed_server(asset: &PlatformAsset) -> Option<String> {
    installed_server_directories()
        .into_iter()
        .filter_map(|directory| {
            let (target, version) = parse_install(&directory)?;
            let binary_path = format!("{directory}/{}", asset.binary_name);
            (target == Some(asset.target.as_str()) && is_installed(&binary_path))
                .then_some((version, binary_path))
        })
        .max()
        .map(|(_, binary_path)| binary_path)
}

/// Installs to delete after `active` was installed: everything for this target (or from before
/// targets were recorded) except the newest previous one, which a language server started
/// before the update may still be running and spawning script workers from. Other targets'
/// installs belong to another Zed (native or Rosetta) and are left alone.
fn stale_installs(names: &[String], active: &str, target: &str) -> Vec<String> {
    let mut previous: Vec<(Vec<u64>, &String)> = names
        .iter()
        .filter(|name| name.as_str() != active)
        .filter_map(|name| {
            let (install_target, version) = parse_install(name)?;
            install_target
                .is_none_or(|install_target| install_target == target)
                .then_some((version, name))
        })
        .collect();
    previous.sort();
    previous.pop();
    previous.into_iter().map(|(_, name)| name.clone()).collect()
}

fn remove_old_servers(active_version_directory: &str, target: &str) {
    let names = installed_server_directories();
    for name in stale_installs(&names, active_version_directory, target) {
        remove_path(&name);
    }
}

fn remove_path(name: &str) {
    let path = Path::new(name);
    if path.is_dir() {
        fs::remove_dir_all(path).ok();
    } else if path.exists() {
        fs::remove_file(path).ok();
    }
}

/// Earlier versions installed Kulala LS from npm into the extension's work directory.
fn remove_kulala_install() {
    if Path::new("node_modules/@mistweaverco").exists() {
        for name in ["node_modules", "package.json", "package-lock.json"] {
            remove_path(name);
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
        let mac = server_asset_for(zed::Os::Mac, zed::Architecture::Aarch64).unwrap();
        assert_eq!(mac.archive_name, "zed-http-lsp-aarch64-apple-darwin.tar.gz");
        assert_eq!(mac.binary_name, "zed-http-lsp");
        assert!(mac.make_executable);

        let windows = server_asset_for(zed::Os::Windows, zed::Architecture::X8664).unwrap();
        assert_eq!(
            windows.archive_name,
            "zed-http-lsp-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(windows.binary_name, "zed-http-lsp.exe");
        assert!(!windows.make_executable);

        let windows_arm = server_asset_for(zed::Os::Windows, zed::Architecture::Aarch64).unwrap();
        assert_eq!(
            windows_arm.archive_name,
            "zed-http-lsp-aarch64-pc-windows-msvc.zip"
        );
        assert_eq!(windows_arm.binary_name, "zed-http-lsp.exe");
    }

    #[test]
    fn parses_install_directories() {
        assert_eq!(
            install_directory("aarch64-apple-darwin", "0.0.4"),
            "zed-http-lsp-aarch64-apple-darwin-0.0.4"
        );
        assert_eq!(
            parse_install("zed-http-lsp-aarch64-apple-darwin-0.0.10"),
            Some((Some("aarch64-apple-darwin"), vec![0, 0, 10]))
        );
        assert_eq!(
            parse_install("zed-http-lsp-0.0.3"),
            Some((None, vec![0, 0, 3]))
        );
        assert_eq!(
            parse_install("zed-http-lsp-staging-aarch64-apple-darwin"),
            None
        );
        assert_eq!(parse_install("kulala-ls"), None);
    }

    #[test]
    fn keeps_the_active_and_previous_install_for_the_target() {
        let names: Vec<String> = [
            "zed-http-lsp-0.0.3",
            "zed-http-lsp-aarch64-apple-darwin-0.0.9",
            "zed-http-lsp-aarch64-apple-darwin-0.0.10",
            "zed-http-lsp-aarch64-apple-darwin-0.0.11",
            "zed-http-lsp-x86_64-apple-darwin-0.0.4",
            "zed-http-lsp-staging-aarch64-apple-darwin",
        ]
        .map(str::to_owned)
        .to_vec();
        let mut stale = stale_installs(
            &names,
            "zed-http-lsp-aarch64-apple-darwin-0.0.11",
            "aarch64-apple-darwin",
        );
        stale.sort();
        // 0.0.10 is kept for a language server that may still be running it; the Rosetta
        // (x86_64) install belongs to another Zed.
        assert_eq!(
            stale,
            vec![
                "zed-http-lsp-0.0.3",
                "zed-http-lsp-aarch64-apple-darwin-0.0.9"
            ]
        );
    }

    #[test]
    fn records_the_selected_server_per_worktree() {
        let directory = env::temp_dir().join(format!(
            "zed-http-record-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        record_server_path(&directory, "/workspace/one", "/abs/path/zed-http-lsp").unwrap();
        record_server_path(
            &directory,
            "/workspace/two",
            "zed-http-lsp-target-1/zed-http-lsp",
        )
        .unwrap();
        let entries: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(entries.len(), 2);
        let mut contents: Vec<String> = entries
            .iter()
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect();
        contents.sort();
        assert_eq!(
            contents,
            vec![
                "/workspace/one\n/abs/path/zed-http-lsp\n".to_owned(),
                "/workspace/two\nzed-http-lsp-target-1/zed-http-lsp\n".to_owned(),
            ]
        );
        assert!(!directory.join("x.tmp").exists());

        record_server_path(&directory, "/workspace/one", "/abs/newer").unwrap();
        let contents: Vec<String> = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect();
        assert!(contents.contains(&"/workspace/one\n/abs/newer\n".to_owned()));
        assert!(fs::read_dir(&directory).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .unwrap()
            != "tmp"));

        assert!(record_server_path(&directory, "/bad\nroot", "/abs").is_err());
        assert!(record_server_path(&directory, "/ok", "binary\r\npath").is_err());
        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn concurrent_records_never_clobber_each_others_writes() {
        let directory = env::temp_dir().join(format!(
            "zed-http-record-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let directory = directory.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    record_server_path(&directory, "/shared/root", &format!("/adapter/{index}"))
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let entries: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let contents = fs::read_to_string(entries[0].path()).unwrap();
        let binary = contents
            .strip_prefix("/shared/root\n")
            .unwrap()
            .strip_suffix('\n')
            .unwrap();
        assert!(
            (0..8).any(|index| binary == format!("/adapter/{index}")),
            "{contents}"
        );
        assert!(fs::read_dir(&directory).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .unwrap()
            != "tmp"));

        record_server_path(&directory, "/other", "/other-adapter").unwrap();
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn release_manifests_share_the_extension_version() {
        let version_line = format!("version = \"{SERVER_VERSION}\"");
        assert!(include_str!("../extension.toml").contains(&version_line));
        assert!(include_str!("../http-lsp/Cargo.toml").contains(&version_line));
        assert!(include_str!("../Cargo.toml").contains(&version_line));
    }
}

zed::register_extension!(HttpExtension);
