//! Command line, for scripts and other programs:
//!
//! ```text
//! brightdl.exe <url> [--dir <folder>] [--name <file>] [--queue <name>] [--later] [--silent]
//! ```
//!
//! IDM's switches work too: `/d <url> /p <folder> /f <file> /a` (add without
//! starting) and `/n` (no dialog). Without `--silent` / `/n` the Add dialog
//! opens with the link filled in.

use std::path::PathBuf;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub url: String,
    pub dir: Option<PathBuf>,
    pub name: Option<String>,
    pub queue: Option<String>,
    /// Add to the list without starting.
    pub later: bool,
    /// Add straight away instead of showing the Add dialog.
    pub silent: bool,
}

/// Parse the program's arguments (without the program name). `None` when
/// they name no download (e.g. only `--minimized`).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Option<Command> {
    let mut cmd = Command::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_ascii_lowercase().as_str() {
            "/d" | "--url" => cmd.url = args.next()?,
            "/p" | "--dir" => cmd.dir = args.next().map(PathBuf::from),
            "/f" | "--name" => cmd.name = args.next(),
            "--queue" => cmd.queue = args.next(),
            "/a" | "--later" => cmd.later = true,
            "/n" | "--silent" => cmd.silent = true,
            _ if cmd.url.is_empty() && is_link(&arg) => cmd.url = arg,
            _ => {}
        }
    }
    is_link(&cmd.url).then_some(cmd)
}

fn is_link(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "ftp") && u.host().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn brightdl_and_idm_styles() {
        let c = parse(args(&["https://x.example/a.zip", "--dir", r"C:\dl", "--later", "--silent"])).unwrap();
        assert_eq!(c.url, "https://x.example/a.zip");
        assert_eq!(c.dir, Some(PathBuf::from(r"C:\dl")));
        assert!(c.later && c.silent);

        let c = parse(args(&["/d", "ftp://x.example/b.iso", "/p", r"D:\iso", "/f", "b2.iso", "/n"])).unwrap();
        assert_eq!(c.url, "ftp://x.example/b.iso");
        assert_eq!(c.name.as_deref(), Some("b2.iso"));
        assert!(c.silent && !c.later);
    }

    #[test]
    fn ignores_launches_without_a_link() {
        assert_eq!(parse(args(&["--minimized"])), None);
        assert_eq!(parse(args(&[])), None);
        assert_eq!(parse(args(&["/d", "javascript:alert(1)"])), None);
    }
}
