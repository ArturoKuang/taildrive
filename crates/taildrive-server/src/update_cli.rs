use std::{
    env,
    ffi::OsString,
    fs,
    io,
    path::{Path, PathBuf},
    process::Command,
};

use taildrive_core::update::{
    UpdateCheck, UpdateError, UpdateRelease, UpdaterClient, current_build_info,
    default_manifest_url, default_public_key_b64, generate_signing_keypair, manifest_url_for_tag,
    sign_manifest_bytes,
};
use tempfile::TempDir;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub enum CliAction {
    RunServer,
    Handled,
}

pub async fn maybe_run_cli(args: &[OsString]) -> Result<CliAction, BoxError> {
    if args.len() <= 1 {
        return Ok(CliAction::RunServer);
    }

    let first = args[1].to_string_lossy();
    match first.as_ref() {
        "version" | "--version" | "-V" => {
            let info = current_build_info();
            println!(
                "taildrive-server {} (channel={} build={} commit={})",
                info.version, info.channel, info.build_number, info.short_sha
            );
            Ok(CliAction::Handled)
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(CliAction::Handled)
        }
        "update" => {
            let options = UpdateOptions::parse(&args[2..])?;
            run_update(options).await?;
            Ok(CliAction::Handled)
        }
        "release-keygen" => {
            let (private_b64, public_b64) = generate_signing_keypair()?;
            println!("TAILDRIVE_UPDATE_SIGNING_KEY={private_b64}");
            println!("TAILDRIVE_UPDATE_PUBLIC_KEY={public_b64}");
            Ok(CliAction::Handled)
        }
        "release-sign" => {
            let (manifest_path, output_path) = parse_release_sign_args(&args[2..])?;
            let private_key = env::var("TAILDRIVE_UPDATE_SIGNING_KEY")
                .map_err(|_| "TAILDRIVE_UPDATE_SIGNING_KEY environment variable is required")?;
            let manifest_bytes = fs::read(&manifest_path)?;
            let sig = sign_manifest_bytes(&manifest_bytes, &private_key)?;
            fs::write(&output_path, format!("{sig}\n"))?;
            println!("Signed {} -> {}", manifest_path.display(), output_path.display());
            Ok(CliAction::Handled)
        }
        other => Err(format!("unknown argument '{other}'. Run 'taildrive-server --help'.").into()),
    }
}

fn print_help() {
    println!(
        "\
taildrive-server

USAGE:
  taildrive-server                       Start the TailDrive HTTP server
  taildrive-server --version             Print version and build metadata
  taildrive-server update [OPTIONS]      Check for or install releases from main

UPDATE OPTIONS:
  --check                  Only check whether a newer release is available
  --force                  Install even on dev builds or when already up to date
  --to <TAG>               Install a specific release tag (e.g. main-42)
  --restart                Restart the TailDrive service after updating
  --web-dir <PATH>         Also update static web UI directory (defaults to TAILDRIVE_WEB_DIR)
  --launchd-label <LABEL>  macOS LaunchAgent label (default: com.local.taildrive-server)
  --manifest-url <URL>     Override manifest URL
  --public-key <BASE64>    Override Ed25519 public key
  --artifact <KEY>         Override artifact key (default: auto-detected OS/arch)"
    );
}

#[derive(Debug, PartialEq, Eq)]
struct UpdateOptions {
    check_only: bool,
    force: bool,
    tag: Option<String>,
    restart: bool,
    web_dir: Option<PathBuf>,
    launchd_label: String,
    manifest_url: Option<String>,
    public_key: Option<String>,
    artifact_key: Option<String>,
}

impl UpdateOptions {
    fn parse(args: &[OsString]) -> Result<Self, BoxError> {
        let mut options = Self {
            check_only: false,
            force: false,
            tag: None,
            restart: false,
            web_dir: env::var_os("TAILDRIVE_WEB_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            launchd_label: env::var("TAILDRIVE_LAUNCHD_LABEL")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "com.local.taildrive-server".to_owned()),
            manifest_url: None,
            public_key: None,
            artifact_key: None,
        };

        let mut index = 0;
        while index < args.len() {
            let flag = args[index].to_string_lossy();
            match flag.as_ref() {
                "--check" => options.check_only = true,
                "--force" => options.force = true,
                "--restart" => options.restart = true,
                "--to" => {
                    index += 1;
                    options.tag = Some(next_value(args, index, "--to")?);
                }
                "--web-dir" => {
                    index += 1;
                    options.web_dir = Some(PathBuf::from(next_value(args, index, "--web-dir")?));
                }
                "--launchd-label" => {
                    index += 1;
                    options.launchd_label = next_value(args, index, "--launchd-label")?;
                }
                "--manifest-url" => {
                    index += 1;
                    options.manifest_url = Some(next_value(args, index, "--manifest-url")?);
                }
                "--public-key" => {
                    index += 1;
                    options.public_key = Some(next_value(args, index, "--public-key")?);
                }
                "--artifact" => {
                    index += 1;
                    options.artifact_key = Some(next_value(args, index, "--artifact")?);
                }
                other => return Err(format!("unknown update flag '{other}'").into()),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn parse_release_sign_args(args: &[OsString]) -> Result<(PathBuf, PathBuf), BoxError> {
    let mut manifest = None;
    let mut output = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_string_lossy().as_ref() {
            "--manifest" => {
                index += 1;
                manifest = Some(PathBuf::from(next_value(args, index, "--manifest")?));
            }
            "--output" => {
                index += 1;
                output = Some(PathBuf::from(next_value(args, index, "--output")?));
            }
            other => return Err(format!("unknown release-sign flag '{other}'").into()),
        }
        index += 1;
    }
    let manifest = manifest.ok_or("--manifest <path> is required")?;
    let output = output.unwrap_or_else(|| {
        let mut sig = manifest.as_os_str().to_os_string();
        sig.push(".sig");
        PathBuf::from(sig)
    });
    Ok((manifest, output))
}

fn next_value(args: &[OsString], index: usize, flag: &str) -> Result<String, BoxError> {
    args.get(index)
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{flag} requires a value").into())
}

fn default_server_artifact_key() -> Result<&'static str, BoxError> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok("server-darwin-arm64"),
        ("macos", "x86_64") => Ok("server-darwin-x86_64"),
        ("linux", "x86_64") => Ok("server-linux-x86_64"),
        ("linux", "aarch64") => Ok("server-linux-arm64"),
        (os, arch) => Err(format!("unsupported platform for server auto-update: {os}/{arch}").into()),
    }
}

async fn run_update(options: UpdateOptions) -> Result<(), BoxError> {
    let current = current_build_info();
    let artifact_key = match options.artifact_key {
        Some(key) => key,
        None => default_server_artifact_key()?.to_owned(),
    };
    let manifest_url = match (&options.manifest_url, &options.tag) {
        (Some(url), _) => url.clone(),
        (None, Some(tag)) => manifest_url_for_tag(tag),
        (None, None) => default_manifest_url(),
    };
    let public_key = options.public_key.unwrap_or_else(default_public_key_b64);
    let updater = UpdaterClient::new()?;

    let release: UpdateRelease = if options.tag.is_some() || options.force {
        let rel = updater
            .fetch_release_artifact(&artifact_key, &manifest_url, &public_key)
            .await?;
        if options.check_only {
            print_available(&current.version, &rel);
            return Ok(());
        }
        rel
    } else {
        match updater
            .check_for_update(&current, &artifact_key, &manifest_url, &public_key, true)
            .await?
        {
            UpdateCheck::UpToDate { current } => {
                println!(
                    "taildrive-server is already up to date ({}, commit {}).",
                    current.version, current.short_sha
                );
                return Ok(());
            }
            UpdateCheck::Available { release, .. } => {
                if options.check_only {
                    print_available(&current.version, &release);
                    return Ok(());
                }
                *release
            }
        }
    };

    println!(
        "Updating taildrive-server {} -> {} ({})...",
        current.version, release.version, release.short_sha
    );

    let target_exe = env::current_exe()?.canonicalize()?;
    let staging = TempDir::new()?;
    let archive_path = staging.path().join("release.tar.gz");
    updater
        .download_verified_artifact(&release, &archive_path, |_, _| true)
        .await?;

    let extract_dir = staging.path().join("extracted");
    fs::create_dir_all(&extract_dir)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&extract_dir)
        .status()?;
    if !status.success() {
        return Err("failed to extract server update archive".into());
    }

    let staged_binary = extract_dir.join("taildrive-server");
    if !staged_binary.is_file() {
        return Err("update archive is missing taildrive-server binary".into());
    }
    set_executable_permissions(&staged_binary)?;

    // Smoke test the downloaded binary before swapping it into place.
    let smoke = Command::new(&staged_binary).arg("--version").output()?;
    if !smoke.status.success() {
        return Err(format!(
            "downloaded taildrive-server failed --version smoke test: {}",
            String::from_utf8_lossy(&smoke.stderr)
        )
        .into());
    }
    let smoke_version = String::from_utf8_lossy(&smoke.stdout).trim().to_owned();

    atomic_replace_binary(&staged_binary, &target_exe)?;
    println!("Installed {} -> {}", smoke_version, target_exe.display());

    let staged_web = extract_dir.join("web-dist");
    if let Some(web_dir) = &options.web_dir
        && staged_web.is_dir()
    {
        atomic_replace_directory(&staged_web, web_dir)?;
        println!("Updated web assets -> {}", web_dir.display());
    }

    if options.restart {
        restart_service(&options.launchd_label)?;
    }

    Ok(())
}

fn print_available(current_version: &str, release: &UpdateRelease) {
    println!(
        "Update available: {} -> {} (build {}, commit {})",
        current_version, release.version, release.build_number, release.short_sha
    );
    if !release.notes.trim().is_empty() {
        println!("\nRelease notes:\n{}", release.notes.trim());
    }
    println!("\nRelease: {}", release.release_url);
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn atomic_replace_binary(staged_binary: &Path, target_exe: &Path) -> io::Result<()> {
    let parent = target_exe
        .parent()
        .ok_or_else(|| io::Error::other("target executable has no parent directory"))?;
    let temp_target = parent.join(format!(".taildrive-server.new.{}", std::process::id()));
    fs::copy(staged_binary, &temp_target)?;
    set_executable_permissions(&temp_target)?;
    if let Err(error) = fs::rename(&temp_target, target_exe) {
        let _ = fs::remove_file(&temp_target);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn atomic_replace_directory(staged_dir: &Path, target_dir: &Path) -> io::Result<()> {
    let parent = target_dir
        .parent()
        .ok_or_else(|| io::Error::other("target directory has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let pid = std::process::id();
    let temp_dir = parent.join(format!(".web-dist.new.{pid}"));
    let backup_dir = parent.join(format!(".web-dist.bak.{pid}"));
    let _ = fs::remove_dir_all(&temp_dir);
    let _ = fs::remove_dir_all(&backup_dir);

    copy_dir_recursive(staged_dir, &temp_dir)?;

    let had_existing = target_dir.exists();
    if had_existing {
        fs::rename(target_dir, &backup_dir)?;
    }
    if let Err(error) = fs::rename(&temp_dir, target_dir) {
        let _ = fs::remove_dir_all(&temp_dir);
        if had_existing {
            let _ = fs::rename(&backup_dir, target_dir);
        }
        return Err(error);
    }
    if had_existing {
        let _ = fs::remove_dir_all(&backup_dir);
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

fn restart_service(launchd_label: &str) -> Result<(), BoxError> {
    if cfg!(target_os = "macos") {
        let uid_out = Command::new("id").arg("-u").output()?;
        if !uid_out.status.success() {
            return Err("failed to determine current uid for launchctl".into());
        }
        let uid = String::from_utf8_lossy(&uid_out.stdout).trim().to_owned();
        let service = format!("gui/{uid}/{launchd_label}");
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", &service])
            .status()?;
        if !status.success() {
            return Err(format!("launchctl kickstart failed for {service}").into());
        }
        println!("Restarted LaunchAgent {service}");
    } else if cfg!(target_os = "linux") {
        let status = Command::new("systemctl")
            .args(["restart", "taildrive-server"])
            .status()?;
        if !status.success() {
            return Err("systemctl restart taildrive-server failed".into());
        }
        println!("Restarted systemd service taildrive-server");
    } else {
        println!("Update installed. Restart taildrive-server manually on this platform.");
    }
    Ok(())
}

#[allow(dead_code)]
fn _assert_update_error_send(_: UpdateError) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_update_flags() {
        let args: Vec<OsString> = [
            "--check",
            "--force",
            "--to",
            "main-12",
            "--restart",
            "--web-dir",
            "/tmp/web",
            "--launchd-label",
            "com.example.td",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let parsed = UpdateOptions::parse(&args).expect("parse");
        assert!(parsed.check_only);
        assert!(parsed.force);
        assert!(parsed.restart);
        assert_eq!(parsed.tag.as_deref(), Some("main-12"));
        assert_eq!(parsed.web_dir, Some(PathBuf::from("/tmp/web")));
        assert_eq!(parsed.launchd_label, "com.example.td");
    }

    #[test]
    fn atomic_replace_binary_and_directory_work() {
        let root = TempDir::new().expect("tempdir");
        let staged = root.path().join("staged-bin");
        let target = root.path().join("taildrive-server");
        fs::write(&target, b"old-binary").expect("write old");
        fs::write(&staged, b"new-binary").expect("write new");

        atomic_replace_binary(&staged, &target).expect("replace bin");
        assert_eq!(fs::read(&target).expect("read"), b"new-binary");

        let staged_web = root.path().join("staged-web");
        let target_web = root.path().join("web-dist");
        fs::create_dir_all(staged_web.join("assets")).expect("mkdir");
        fs::write(staged_web.join("index.html"), b"new index").expect("write");
        fs::write(staged_web.join("assets/app.js"), b"console.log(2)").expect("write");
        fs::create_dir_all(&target_web).expect("mkdir");
        fs::write(target_web.join("index.html"), b"old index").expect("write");

        atomic_replace_directory(&staged_web, &target_web).expect("replace dir");
        assert_eq!(
            fs::read_to_string(target_web.join("index.html")).expect("read"),
            "new index"
        );
        assert_eq!(
            fs::read_to_string(target_web.join("assets/app.js")).expect("read"),
            "console.log(2)"
        );
    }
}
