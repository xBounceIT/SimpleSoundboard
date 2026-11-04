use serde::Deserialize;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use ureq::Agent;

/// GitHub repository that publishes the Windows setups.
const REPO: &str = "xBounceIT/SimpleSoundboard";
/// Version compiled into this build, kept in step with Cargo.toml and the tag.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!("SimpleSoundboard/", env!("CARGO_PKG_VERSION"));
/// Largest setup accepted from GitHub, to reject an HTML error page early.
const MAX_SETUP_BYTES: u64 = 512 * 1024 * 1024;

/// A release newer than the running build, ready to be shown in the toast.
#[derive(Clone, Debug)]
pub struct AvailableUpdate {
    pub version: String,
    pub release_url: String,
    pub setup_url: String,
    pub setup_name: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

fn latest_release_url() -> String {
    if let Ok(url) = std::env::var("SIMPLE_SOUNDBOARD_UPDATE_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

fn http_agent(timeout_secs: u64) -> Agent {
    Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
        .into()
}

/// Ask GitHub for the latest published release.
///
/// A missing release (404) simply means there is nothing to offer yet; network
/// and API failures are returned so the caller can log them without bothering
/// the user with a startup error.
pub fn check() -> Result<Option<AvailableUpdate>, String> {
    let agent = http_agent(10);
    let mut response = match agent.get(&latest_release_url()).call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(error) => return Err(format!("GitHub is unreachable: {error}")),
    };
    let body = response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_string()
        .map_err(|error| format!("Could not read the GitHub response: {error}"))?;
    let release: GithubRelease =
        serde_json::from_str(&body).map_err(|error| format!("Invalid GitHub response: {error}"))?;

    let version = release.tag_name.trim_start_matches(['v', 'V']).to_string();
    if !is_newer(&version, CURRENT_VERSION) {
        return Ok(None);
    }

    let asset = release
        .assets
        .iter()
        .find(|asset| is_setup_asset(&asset.name))
        .ok_or_else(|| format!("Release v{version} has no Windows setup"))?;
    let setup_name = Path::new(&asset.name)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Invalid setup name".to_string())?
        .to_string();

    Ok(Some(AvailableUpdate {
        version,
        release_url: release.html_url,
        setup_url: asset.browser_download_url.clone(),
        setup_name,
    }))
}

/// Download the setup next to the other temporary files and make sure it is a
/// Windows executable before the installer is launched.
pub fn download(update: &AvailableUpdate) -> Result<PathBuf, String> {
    let directory = std::env::temp_dir().join("SimpleSoundboard");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create the temporary folder: {error}"))?;
    let path = directory.join(&update.setup_name);

    let agent = http_agent(600);
    let mut response = agent
        .get(&update.setup_url)
        .call()
        .map_err(|error| format!("Setup download failed: {error}"))?;
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(MAX_SETUP_BYTES)
        .reader();
    let mut file =
        File::create(&path).map_err(|error| format!("Could not write the setup: {error}"))?;
    io::copy(&mut reader, &mut file).map_err(|error| format!("Download interrupted: {error}"))?;
    drop(file);

    if !is_windows_executable(&path).unwrap_or(false) {
        let _ = fs::remove_file(&path);
        return Err("The downloaded file is not a valid Windows setup".to_string());
    }

    Ok(path)
}

fn is_windows_executable(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut dos_header = [0u8; 0x40];
    file.read_exact(&mut dos_header)?;
    if &dos_header[0..2] != b"MZ" {
        return Ok(false);
    }

    let pe_offset = u32::from_le_bytes([
        dos_header[0x3c],
        dos_header[0x3d],
        dos_header[0x3e],
        dos_header[0x3f],
    ]) as u64;
    if !(0x40..=16 * 1024 * 1024).contains(&pe_offset) {
        return Ok(false);
    }

    file.seek(SeekFrom::Start(pe_offset))?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)?;
    Ok(&signature == b"PE\0\0")
}

/// Launch the downloaded Inno Setup in silent mode. The caller quits the app
/// right after this succeeds so the installer can replace the executable.
pub fn run_installer(setup: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(setup)
            .args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CLOSEAPPLICATIONS",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not launch the setup: {error}"))
    }
    #[cfg(not(windows))]
    {
        let _ = setup;
        Err("Automatic updates are only available on Windows".to_string())
    }
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

fn is_setup_asset(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("simplesoundboard-setup") && name.ends_with(".exe")
}

#[cfg(test)]
mod tests {
    use super::{is_newer, is_setup_asset, is_windows_executable, parse_version};
    use std::fs;

    #[test]
    fn versions_compare_numerically_and_strip_the_tag_prefix() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert_eq!(parse_version("v1.2.3-beta.1"), Some((1, 2, 3)));
    }

    #[test]
    fn windows_executables_need_a_pe_header() {
        let path =
            std::env::temp_dir().join(format!("simple-soundboard-pe-{}.bin", std::process::id()));
        let mut bytes = vec![0u8; 0x80];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        fs::write(&path, &bytes).expect("write fake executable");
        assert!(is_windows_executable(&path).expect("read fake executable"));

        bytes[0x40] = b'X';
        fs::write(&path, &bytes).expect("write broken executable");
        assert!(!is_windows_executable(&path).expect("read broken executable"));

        fs::write(&path, b"MZ").expect("write short executable");
        assert!(!is_windows_executable(&path).unwrap_or(false));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn setup_assets_are_recognised_by_name() {
        assert!(is_setup_asset("SimpleSoundboard-Setup-0.2.0.exe"));
        assert!(is_setup_asset("simplesoundboard-setup.EXE"));
        assert!(!is_setup_asset("SimpleSoundboard.zip"));
        assert!(!is_setup_asset("other-Setup-0.2.0.exe"));
    }
}
