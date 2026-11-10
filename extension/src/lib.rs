use zed_extension_api as zed;

const RELEASE_REPOSITORY: &str = "kidp2h/zed-mybatis-lsp";
const RELEASE_VERSION: &str = "0.2.0";

struct MyBatisNavigation {
    server_path: Option<String>,
}

fn release_asset() -> Result<&'static str, String> {
    let (os, architecture) = zed::current_platform();
    match (os, architecture) {
        (zed::Os::Mac, zed::Architecture::Aarch64) => Ok("mybatis-lsp-darwin-aarch64"),
        (zed::Os::Mac, zed::Architecture::X8664) => Ok("mybatis-lsp-darwin-x86_64"),
        (zed::Os::Linux, zed::Architecture::Aarch64) => Ok("mybatis-lsp-linux-aarch64"),
        (zed::Os::Linux, zed::Architecture::X8664) => Ok("mybatis-lsp-linux-x86_64"),
        (zed::Os::Windows, zed::Architecture::X8664) => Ok("mybatis-lsp-windows-x86_64.exe"),
        _ => Err(format!("Unsupported platform: {os:?} {architecture:?}")),
    }
}

impl zed::Extension for MyBatisNavigation {
    fn new() -> Self where Self: Sized {
        Self { server_path: None }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command, String> {
        if let Some(command) = &self.server_path {
            return Ok(zed::Command { command: command.clone(), args: vec![], env: vec![] });
        }

        if let Some(command) = worktree.which("mybatis-lsp") {
            self.server_path = Some(command.clone());
            return Ok(zed::Command { command, args: vec![], env: vec![] });
        }

        let asset = release_asset()?;
        let file_name = format!("mybatis-lsp-{RELEASE_VERSION}-{asset}");
        let download_url = format!(
            "https://github.com/{RELEASE_REPOSITORY}/releases/download/v{RELEASE_VERSION}/{asset}"
        );

        if !std::path::Path::new(&file_name).exists() {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );
            if let Err(error) = zed::download_file(
                &download_url,
                &file_name,
                zed::DownloadedFileType::Uncompressed,
            ) {
                let message = format!("Failed to download {asset}: {error}");
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Failed(message.clone()),
                );
                return Err(message);
            }
            if let Err(error) = zed::make_file_executable(&file_name) {
                let message = format!("Failed to make {file_name} executable: {error}");
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Failed(message.clone()),
                );
                return Err(message);
            }
        }

        self.server_path = Some(file_name.clone());
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::None,
        );
        let command = file_name;
        Ok(zed::Command { command, args: vec![], env: vec![] })
    }
}

zed::register_extension!(MyBatisNavigation);
