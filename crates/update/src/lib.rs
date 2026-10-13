//! Keeps the programs released as plain binaries current: asks GitHub for
//! the latest release and replaces the programs next to the running one
//! with the ones in its archive. A build that a package manager installs
//! leaves this crate out (the front ends' `self-update` feature).
//!
//! Nothing here is signed: an update is as trustworthy as the HTTPS
//! connection to GitHub it came over.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPOSITORY: &str = "https://github.com/ccptr/SoundBridge";
/// Both are replaced, whichever of them is running, so they stay the same
/// version; one that is not installed is not added.
const PROGRAMS: [&str; 2] = ["soundbridge", "soundbridge-desktop"];
/// Set (to anything) where something else keeps the programs current: a
/// package made from the released binaries starts them with it.
const DISABLED: &str = "SOUNDBRIDGE_NO_UPDATE";
/// How often [`auto`] asks.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The archives are some tens of megabytes; this only stops a runaway download.
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

/// A release version, as in the tag `v1.2.3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version([u32; 3]);

impl Version {
    /// The version of the running program.
    pub fn current() -> Version {
        env!("CARGO_PKG_VERSION").parse().expect("the package version is three numbers")
    }
}

impl std::str::FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Version, String> {
        let mut numbers = s.strip_prefix('v').unwrap_or(s).split('.').map(str::parse);
        let mut next = || numbers.next()?.ok();
        let version = (|| Some(Version([next()?, next()?, next()?])))();
        version.filter(|_| numbers.next().is_none()).ok_or_else(|| format!("'{s}' is not a version"))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

pub enum Outcome {
    /// Nothing newer has been released.
    Current(Version),
    /// The programs were replaced; the running one is still the old version.
    Updated(Version),
}

/// Checks for a newer release and installs it. Blocks for the download.
pub fn update() -> Result<Outcome, String> {
    if !available() {
        return Err(format!("this copy does not update itself ({DISABLED} is set)"));
    }
    update_from(Version::current(), &program_dir()?)
}

/// False for a copy that something else keeps current, which never updates
/// itself.
pub fn available() -> bool {
    std::env::var_os(DISABLED).is_none_or(|value| value.is_empty())
}

fn update_from(current: Version, dir: &Path) -> Result<Outcome, String> {
    let platform = platform().ok_or("releases have no programs for this platform")?;
    let latest = latest()?;
    if latest <= current {
        return Ok(Outcome::Current(current));
    }
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    let url = format!("{REPOSITORY}/releases/download/v{latest}/soundbridge-v{latest}-{platform}.{extension}");
    let archive = agent(ureq::config::Config::builder())
        .get(&url)
        .call()
        .and_then(|response| response.into_body().into_with_config().limit(MAX_ARCHIVE_BYTES).read_to_vec())
        .map_err(|e| format!("could not download {url}: {e}"))?;
    install(&archive, dir)?;
    Ok(Outcome::Updated(latest))
}

/// The name releases give this platform's archive, if they have one.
fn platform() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else {
        None
    }
}

fn agent(config: ureq::config::ConfigBuilder<ureq::typestate::AgentScope>) -> ureq::Agent {
    config
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(concat!("soundbridge/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The latest release, read off where `releases/latest` redirects to. That
/// page is not rate limited the way the API is, and needs no JSON parser.
fn latest() -> Result<Version, String> {
    let url = format!("{REPOSITORY}/releases/latest");
    let config = ureq::config::Config::builder().max_redirects(0).max_redirects_will_error(false);
    let response = agent(config).get(&url).call().map_err(|e| format!("could not reach {url}: {e}"))?;
    let location = response.headers().get("location").and_then(|l| l.to_str().ok());
    location.and_then(tag_version).ok_or_else(|| format!("{url} did not name a release"))
}

/// `.../releases/tag/v1.2.3` to 1.2.3.
fn tag_version(location: &str) -> Option<Version> {
    location.rsplit_once("/releases/tag/")?.1.parse().ok()
}

/// Where the running program is, with links followed: the directory an
/// archive was unpacked into.
fn program_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().and_then(fs::canonicalize);
    let exe = exe.map_err(|e| format!("cannot tell where this program is: {e}"))?;
    Ok(exe.parent().expect("a file is in a directory").to_path_buf())
}

fn file_name(program: &str) -> String {
    format!("{program}{}", std::env::consts::EXE_SUFFIX)
}

/// Replaces the programs in `dir` with the ones in `archive`. They are all
/// unpacked next to the old ones first, so a broken archive or a full disk
/// replaces none of them.
fn install(archive: &[u8], dir: &Path) -> Result<(), String> {
    let names: Vec<String> = PROGRAMS.map(file_name).into_iter().filter(|name| dir.join(name).is_file()).collect();
    let staged = |name: &str| dir.join(format!("{name}.new"));
    let result = (|| {
        let mut found = Vec::new();
        unpack(archive, &mut |name, contents| {
            if names.iter().any(|n| n == name) {
                write_program(&staged(name), contents)?;
                found.push(name.to_string());
            }
            Ok(())
        })?;
        if let Some(missing) = names.iter().find(|name| !found.contains(name)) {
            return Err(std::io::Error::other(format!("the release has no {missing}")));
        }
        names.iter().try_for_each(|name| replace(&staged(name), &dir.join(name)))
    })();
    if result.is_err() {
        for name in &names {
            let _ = fs::remove_file(staged(name));
        }
    }
    result.map_err(|e| format!("could not replace the programs in {}: {e}", dir.display()))
}

fn write_program(path: &Path, contents: &mut dyn Read) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o755);
    let mut file = options.open(path)?;
    std::io::copy(contents, &mut file)?;
    file.sync_all()
}

/// Windows will not overwrite a program that is running, but lets it be
/// renamed; [`clean_up`] removes it once it has exited.
fn replace(new: &Path, program: &Path) -> std::io::Result<()> {
    if cfg!(windows) {
        let old = retired(program);
        let _ = fs::remove_file(&old);
        fs::rename(program, old)?;
    }
    fs::rename(new, program)
}

fn retired(program: &Path) -> PathBuf {
    let mut name = program.as_os_str().to_owned();
    name.push(".old");
    name.into()
}

/// Removes what an earlier update on Windows left behind.
fn clean_up(dir: &Path) {
    for program in PROGRAMS {
        let _ = fs::remove_file(retired(&dir.join(file_name(program))));
    }
}

/// Calls `file` with the name (without directories) and contents of each
/// file in the archive.
#[cfg(not(windows))]
fn unpack(
    archive: &[u8],
    file: &mut dyn FnMut(&str, &mut dyn Read) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            file(name, &mut entry)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn unpack(
    archive: &[u8],
    file: &mut dyn FnMut(&str, &mut dyn Read) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(std::io::Error::other)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(std::io::Error::other)?;
        if !entry.is_file() {
            continue;
        }
        let path = PathBuf::from(entry.name().map_err(std::io::Error::other)?.into_owned());
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            file(name, &mut entry)?;
        }
    }
    Ok(())
}

/// Whether updates install themselves, and when that was last looked into;
/// kept in a small `key=value` file that both programs share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Preferences {
    auto: bool,
    /// Seconds since the epoch.
    checked: u64,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences { auto: true, checked: 0 }
    }
}

impl Preferences {
    fn path() -> Option<PathBuf> {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let dir = if cfg!(target_os = "windows") {
            var("APPDATA")?
        } else if cfg!(target_os = "macos") {
            var("HOME")?.join("Library/Application Support")
        } else {
            var("XDG_CONFIG_HOME").or_else(|| Some(var("HOME")?.join(".config")))?
        };
        Some(dir.join("soundbridge").join("update.conf"))
    }

    fn load() -> Preferences {
        let text = Self::path().and_then(|p| fs::read_to_string(p).ok());
        text.map(|text| Self::parse(&text)).unwrap_or_default()
    }

    /// Best effort: a read-only home directory just means nothing is remembered.
    fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(path, self.serialize());
    }

    fn parse(text: &str) -> Preferences {
        let mut p = Preferences::default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("auto", value)) => p.auto = value != "0",
                Some(("checked", value)) => p.checked = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        p
    }

    fn serialize(&self) -> String {
        format!("auto={}\nchecked={}\n", self.auto as u8, self.checked)
    }

    fn due(&self, now: u64) -> bool {
        // A clock that was set back must not put the next check off for ever.
        now < self.checked || now - self.checked >= CHECK_EVERY.as_secs()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether [`auto`] does anything. It does unless the user turned it off.
pub fn auto_enabled() -> bool {
    Preferences::load().auto
}

pub fn set_auto_enabled(auto: bool) {
    Preferences { auto, ..Preferences::load() }.save();
}

/// Updates in the background if the user has not turned that off and it has
/// not been looked into for a day. `updated` is called, on another thread,
/// only if the programs were replaced; with no network, or no right to write
/// where the programs are, nothing happens and nothing is said.
pub fn auto(updated: impl FnOnce(Version) + Send + 'static) {
    std::thread::spawn(move || {
        let Ok(dir) = program_dir() else { return };
        clean_up(&dir);
        let preferences = Preferences::load();
        if !available() || !preferences.auto || !preferences.due(now()) {
            return;
        }
        let outcome = update_from(Version::current(), &dir);
        // After a failure too: one that lasts, such as programs that cannot
        // be written, should cost a request a day and not one at every start.
        // The switch is read again, in case it was turned meanwhile.
        Preferences { checked: now(), ..Preferences::load() }.save();
        if let Ok(Outcome::Updated(version)) = outcome {
            updated(version);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        let v = |s: &str| s.parse::<Version>();
        assert_eq!(v("v0.1.0"), Ok(Version([0, 1, 0])));
        assert_eq!(v("1.20.3").unwrap().to_string(), "1.20.3");
        assert!(v("v0.10.0").unwrap() > v("v0.9.9").unwrap());
        assert!(v("v1.0.0").unwrap() > v("v0.99.0").unwrap());
        for junk in ["", "v1", "1.2", "1.2.3.4", "1.2.x", "v1.2.3-rc1", "latest"] {
            assert!(v(junk).is_err(), "{junk}");
        }
        assert!(Version::current() >= Version([0, 1, 0]));
    }

    #[test]
    fn reads_the_release_off_a_redirect() {
        let tag = "https://github.com/ccptr/SoundBridge/releases/tag/v0.2.1";
        assert_eq!(tag_version(tag), Some(Version([0, 2, 1])));
        // What GitHub answers when there is no release at all.
        assert_eq!(tag_version("https://github.com/ccptr/SoundBridge/releases"), None);
    }

    #[test]
    fn preferences_round_trip_and_default_to_on() {
        let p = Preferences { auto: false, checked: 1_760_000_000 };
        assert_eq!(Preferences::parse(&p.serialize()), p);
        assert_eq!(Preferences::parse("nonsense\nchecked=soon\n"), Preferences::default());
        assert!(Preferences::default().auto);
    }

    #[test]
    fn checks_once_a_day() {
        let p = Preferences { auto: true, checked: 1_000_000 };
        assert!(!p.due(1_000_000 + 3600));
        assert!(p.due(1_000_000 + 24 * 3600));
        assert!(p.due(999_999));
        assert!(Preferences::default().due(now()));
    }

    #[cfg(not(windows))]
    mod install {
        use super::super::*;

        fn archive(files: &[(&str, &str)]) -> Vec<u8> {
            let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (path, contents) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                // Written as it is: the builder refuses a path with `..` in it.
                header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                header.set_cksum();
                tar.append(&header, contents.as_bytes()).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap()
        }

        fn dir(test: &str, files: &[&str]) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("soundbridge-update-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            for file in files {
                fs::write(dir.join(file), "old").unwrap();
            }
            dir
        }

        fn listing(dir: &Path) -> Vec<(String, String)> {
            let mut files: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .map(|p| (p.file_name().unwrap().to_str().unwrap().to_string(), fs::read_to_string(p).unwrap()))
                .collect();
            files.sort();
            files
        }

        const RELEASE: [(&str, &str); 4] = [
            ("soundbridge-v9.9.9-linux-x86_64/soundbridge", "new cli"),
            ("soundbridge-v9.9.9-linux-x86_64/soundbridge-desktop", "new desktop"),
            ("soundbridge-v9.9.9-linux-x86_64/README.md", "new readme"),
            ("../../soundbridge.new", "escaped"),
        ];

        #[test]
        fn replaces_the_programs_and_nothing_else() {
            let dir = dir("both", &["soundbridge", "soundbridge-desktop", "README.md"]);
            install(&archive(&RELEASE), &dir).unwrap();
            let s = String::from;
            assert_eq!(
                listing(&dir),
                [
                    (s("README.md"), s("old")),
                    (s("soundbridge"), s("new cli")),
                    (s("soundbridge-desktop"), s("new desktop")),
                ]
            );
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("soundbridge")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn does_not_add_a_program_that_was_not_there() {
            let dir = dir("one", &["soundbridge"]);
            install(&archive(&RELEASE), &dir).unwrap();
            assert_eq!(listing(&dir), [("soundbridge".to_string(), "new cli".to_string())]);
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn replaces_nothing_when_the_release_lacks_a_program() {
            let dir = dir("lacking", &["soundbridge", "soundbridge-desktop"]);
            let error = install(&archive(&RELEASE[..1]), &dir).unwrap_err();
            assert!(error.ends_with("the release has no soundbridge-desktop"), "{error}");
            let s = String::from;
            assert_eq!(listing(&dir), [(s("soundbridge"), s("old")), (s("soundbridge-desktop"), s("old"))]);
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn replaces_nothing_when_the_archive_is_broken() {
            let dir = dir("broken", &["soundbridge"]);
            let mut archive = archive(&RELEASE);
            archive.truncate(archive.len() / 2);
            assert!(install(&archive, &dir).is_err());
            assert_eq!(listing(&dir), [("soundbridge".to_string(), "old".to_string())]);
            fs::remove_dir_all(dir).unwrap();
        }

        /// Against the real release page. Run with `cargo test -- --ignored`.
        #[test]
        #[ignore = "needs the network"]
        fn updates_from_github() {
            let dir = dir("github", &["soundbridge", "soundbridge-desktop"]);
            let Outcome::Updated(version) = update_from(Version([0, 0, 0]), &dir).unwrap() else {
                panic!("0.0.0 was taken for current");
            };
            let cli = std::process::Command::new(dir.join("soundbridge")).arg("--version").output().unwrap();
            assert_eq!(String::from_utf8(cli.stdout).unwrap().trim(), format!("soundbridge {version}"));
            assert!(matches!(update_from(version, &dir), Ok(Outcome::Current(_))));
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
