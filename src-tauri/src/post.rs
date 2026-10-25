//! What happens to a file after it finishes: virus scan and archive extraction.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Windows' built-in scanner. `%f` is replaced by the downloaded file.
pub const DEFENDER_COMMAND: &str = r#""C:\Program Files\Windows Defender\MpCmdRun.exe" -Scan -ScanType 3 -File "%f" -DisableRemediation"#;

/// Split a command line into program and arguments. Double quotes group
/// words; there is no other escaping (Windows paths are full of backslashes).
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in cmd.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    parts.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        parts.push(cur);
    }
    parts
}

fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// What a command's placeholders are filled from.
#[derive(Default)]
pub struct Vars {
    pub file: PathBuf,
    pub url: String,
    pub referrer: String,
    pub size: Option<u64>,
    /// Seconds from the first start to the finish.
    pub seconds: Option<u64>,
    pub category: String,
    pub queue: String,
}

impl Vars {
    pub fn of(item: &crate::manager::Item) -> Self {
        let referrer = item.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("referer")).map(|(_, v)| v.clone());
        Vars {
            file: item.save_path.clone(),
            url: item.url.clone(),
            referrer: referrer.unwrap_or_default(),
            size: item.size,
            seconds: item.started_at.zip(item.finished_at).map(|(s, f)| f.saturating_sub(s)),
            category: item.category.clone(),
            queue: item.queue.clone(),
        }
    }

    fn value(&self, key: char) -> Option<String> {
        let lossy = |p: Option<&std::ffi::OsStr>| p.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Some(match key {
            'f' => self.file.to_string_lossy().into_owned(),
            'd' => lossy(self.file.parent().map(Path::as_os_str)),
            'n' => lossy(self.file.file_name()),
            'b' => lossy(self.file.file_stem()),
            'e' => lossy(self.file.extension()),
            'u' => self.url.clone(),
            'h' => url::Url::parse(&self.url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default(),
            'r' => self.referrer.clone(),
            's' => self.size.map(size_text).unwrap_or_default(),
            't' => self.seconds.map(|s| format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)).unwrap_or_default(),
            'c' => self.category.clone(),
            'q' => self.queue.clone(),
            _ => return None,
        })
    }
}

/// `1.5 MB`-style size.
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// Fill in the placeholders (`%f`, `%n`, ... see [`Vars::value`]). A letter
/// followed by another letter (`%userprofile%`, `%date%`) is left for cmd.
pub fn expand(text: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let Some(&key) = chars.peek().filter(|_| c == '%') else {
            out.push(c);
            continue;
        };
        let mut rest = chars.clone();
        rest.next();
        let word = rest.peek().is_some_and(|n| n.is_alphanumeric() || *n == '_');
        match vars.value(key).filter(|_| !word) {
            Some(v) => {
                out.push_str(&v);
                chars.next();
            }
            None => out.push(c),
        }
    }
    out
}

/// Run a user's command line in Command Prompt, detached and visible.
pub fn run_command(command: &str, vars: &Vars) -> Result<(), String> {
    let command = expand(command.trim(), vars);
    if command.is_empty() {
        return Err("no command set".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // raw_arg: cmd.exe parses its own command line; Rust's quoting would break it.
        Command::new("cmd").arg("/C").raw_arg(&command).spawn().map(drop).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Command::new("sh").arg("-c").arg(&command).spawn().map(drop).map_err(|e| e.to_string())
    }
}

/// Run `command` with its placeholders filled in (`%f` appended if absent). Blocking.
pub fn scan(command: &str, file: &Path) -> Result<(), String> {
    let mut parts = split_command(command);
    if parts.is_empty() {
        return Ok(());
    }
    if !parts.iter().any(|p| p.contains("%f")) {
        parts.push("%f".into());
    }
    let vars = Vars { file: file.to_path_buf(), ..Vars::default() };
    let parts: Vec<String> = parts.iter().map(|p| expand(p, &vars)).collect();
    let status = hidden(&parts[0]).args(&parts[1..]).status().map_err(|e| format!("Could not start the virus scanner: {e}"))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(format!("The virus scanner reported a problem (exit code {code})")),
        None => Err("The virus scanner was interrupted".into()),
    }
}

/// Archive types Windows' bundled `tar` (libarchive) can unpack.
const ARCHIVES: &[&str] = &[".zip", ".7z", ".rar", ".tar", ".tgz", ".tbz2", ".txz", ".tar.gz", ".tar.bz2", ".tar.xz"];

/// The folder an archive extracts into (`dir/name` for `dir/name.tar.gz`),
/// or `None` if the file is not an archive.
pub fn extract_dir(file: &Path) -> Option<PathBuf> {
    let name = file.file_name()?.to_string_lossy().to_string();
    let lower = name.to_ascii_lowercase();
    let ext = ARCHIVES.iter().filter(|e| lower.ends_with(*e)).max_by_key(|e| e.len())?;
    let stem = &name[..name.len() - ext.len()];
    (!stem.is_empty()).then(|| file.with_file_name(stem))
}

/// `dir`, or `dir (1)`, `dir (2)`, ... if it already exists.
pub fn unique_dir(dir: &Path) -> PathBuf {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (0..)
        .map(|n| if n == 0 { dir.to_path_buf() } else { dir.with_file_name(format!("{name} ({n})")) })
        .find(|p| !p.exists())
        .expect("unbounded search")
}

/// Unpack an archive into `into`. Blocking.
pub fn extract(file: &Path, into: &Path) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| format!("Could not create {}: {e}", into.display()))?;
    // Full path, so a `tar.exe` planted in the working directory is never used.
    let tar = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .unwrap_or_else(|| "tar".into());
    let out = hidden(tar)
        .arg("-xf")
        .arg(file)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|e| format!("Could not start tar: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(format!("Could not extract: {}", msg.lines().next().unwrap_or("unknown error").trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_commands() {
        assert_eq!(
            split_command(r#""C:\Program Files\x.exe" -a "%f" b"#),
            vec![r"C:\Program Files\x.exe", "-a", "%f", "b"]
        );
        assert_eq!(split_command("  scan.exe   "), vec!["scan.exe"]);
        assert_eq!(split_command(r#"a "" b"#), vec!["a", "", "b"]);
        assert!(split_command("").is_empty());
    }

    #[test]
    fn command_placeholders() {
        let vars = Vars {
            file: PathBuf::from(r"C:\dl\My File.zip"),
            url: "https://cdn.example.com/a?b=1".into(),
            size: Some(1_572_864),
            seconds: Some(3725),
            category: "Compressed".into(),
            queue: "Main".into(),
            ..Vars::default()
        };
        assert_eq!(
            expand(r#"copy "%f" D:\b\%n & echo %u"#, &vars),
            r#"copy "C:\dl\My File.zip" D:\b\My File.zip & echo https://cdn.example.com/a?b=1"#
        );
        assert_eq!(expand("%d|%b|%e|%h|%s|%t|%c|%q|%r.", &vars), r"C:\dl|My File|zip|cdn.example.com|1.5 MB|1:02:05|Compressed|Main|.");
        // cmd's own variables and unknown letters are left alone.
        assert_eq!(
            expand(r#"echo %n >> "%userprofile%\l.txt" %date% %x 100%"#, &vars),
            r#"echo My File.zip >> "%userprofile%\l.txt" %date% %x 100%"#
        );
        assert_eq!(size_text(500), "500 B");
    }

    #[test]
    fn archive_folders() {
        let d = Path::new(r"C:\dl");
        assert_eq!(extract_dir(&d.join("pack.ZIP")), Some(d.join("pack")));
        assert_eq!(extract_dir(&d.join("src-1.2.tar.gz")), Some(d.join("src-1.2")));
        assert_eq!(extract_dir(&d.join("movie.mkv")), None);
        assert_eq!(extract_dir(&d.join(".zip")), None);
    }

    #[test]
    #[cfg(windows)]
    fn extracts_a_zip_with_system_tar() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("hello.txt"), "hi").unwrap();
        let zip = dir.path().join("pack.zip");
        let made = hidden(std::env::var_os("SystemRoot").map(|r| PathBuf::from(r).join("System32").join("tar.exe")).unwrap())
            .args(["-a", "-cf"])
            .arg(&zip)
            .arg("-C")
            .arg(&src)
            .arg("hello.txt")
            .status()
            .unwrap();
        assert!(made.success());
        let into = extract_dir(&zip).unwrap();
        extract(&zip, &into).unwrap();
        assert_eq!(std::fs::read_to_string(into.join("hello.txt")).unwrap(), "hi");
    }
}
