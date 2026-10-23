//! The coding agents openshell-desktop can install, and how to fetch them.
//!
//! OpenShell's default sandbox image is bare Ubuntu, so the app brings the
//! agent itself: it downloads the agent's native Linux build from the npm
//! registry, checks it against the registry's SHA-512 integrity value, and
//! keeps the build in a local cache. The app then copies it into the
//! sandbox following the agent's [`Install::plan`], at paths OpenShell's
//! provider profile for that agent allows; the profile only injects
//! credentials into processes running those binaries.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha512};

const REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} is not supported on this machine's architecture")]
    UnsupportedArch(String),
    #[error("download failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{package} has no {entry} in its archive")]
    MissingEntry { package: String, entry: String },
    #[error("{0} failed its integrity check; the download was discarded")]
    Integrity(String),
    #[error("unexpected npm metadata: {0}")]
    Metadata(String),
}

/// Processor architectures with native agent builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    /// This machine's; sandboxes are local containers, so the same.
    pub fn host() -> Option<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Arch::X64),
            "aarch64" => Some(Arch::Arm64),
            _ => None,
        }
    }

    fn npm(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        }
    }

    fn triple(self) -> &'static str {
        match self {
            Arch::X64 => "x86_64-unknown-linux-musl",
            Arch::Arm64 => "aarch64-unknown-linux-musl",
        }
    }
}

/// How the agent's terminal starts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Run the binary as is.
    Direct,
    /// Write the `~/.codex/auth.json` Codex reads its sign-in from, made of
    /// the provider's handles, then run the binary.
    CodexSignIn,
}

/// What of the npm package the sandbox gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One executable at this path in the archive, copied to the agent's
    /// `install_path`. `{triple}` is the Rust target, such as
    /// `x86_64-unknown-linux-musl`.
    Binary(&'static str),
    /// A whole directory of the archive, for agents that need files beside
    /// their executable. It is copied to `dir`, and `install_path` becomes a
    /// small script running `dir/exe`. Both paths may hold `{triple}`.
    Tree {
        root: &'static str,
        dir: &'static str,
        exe: &'static str,
        /// Options the launcher always passes.
        args: &'static str,
    },
}

/// An agent the New Agent dialog offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// The command in the sandbox's PATH that runs the agent, matching the
    /// provider profile.
    pub install_path: &'static str,
    /// Provider profiles whose accounts this agent can use, best first.
    pub profiles: &'static [&'static str],
    /// Environment for the agent's terminal.
    pub environment: &'static [(&'static str, &'static str)],
    pub launch: Launch,
    /// The npm package with the native build; `{arch}` is `x64` or `arm64`.
    package: &'static str,
    /// The version or dist-tag of that package to fetch, with `{arch}` too.
    tag: &'static str,
    layout: Layout,
}

impl Agent {
    pub fn command(&self) -> Vec<String> {
        match self.launch {
            Launch::Direct => vec![self.install_path.to_owned()],
            Launch::CodexSignIn => vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                codex_sign_in_script(self.install_path, unix_now()),
            ],
        }
    }

    fn package(&self, arch: Arch) -> String {
        self.package.replace("{arch}", arch.npm())
    }

    fn tag(&self, arch: Arch) -> String {
        self.tag.replace("{arch}", arch.npm())
    }

    /// The command that succeeds in the sandbox once this agent is fully
    /// installed. A tree install leaves a marker as its last step, so one
    /// cut short, or a bare binary from an older version of the app, gets
    /// installed again.
    pub fn installed_check(&self, arch: Arch) -> Vec<String> {
        match self.layout {
            Layout::Binary(_) => vec!["test".into(), "-x".into(), self.install_path.into()],
            Layout::Tree { dir, .. } => vec![
                "test".into(),
                "-f".into(),
                format!("{}/{MARKER}", dir.replace("{triple}", arch.triple())),
            ],
        }
    }
}

/// The file a finished tree install leaves in its directory.
const MARKER: &str = ".openshell-desktop-installed";

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A shell script that writes Codex's sign-in file and runs `binary`.
///
/// The file holds the handles the gateway put in the environment, not real
/// tokens; the proxy swaps them on requests to OpenAI. `chatgptAuthTokens`
/// tells Codex the tokens are managed elsewhere, so it never tries to renew
/// them itself (the gateway does). Codex also wants an ID token to read the
/// account from, which is never sent anywhere, so it gets an unsigned
/// stand-in, as in OpenShell's own Codex example. Without an account in the
/// environment the file is left alone and Codex asks the user to sign in.
fn codex_sign_in_script(binary: &str, now: u64) -> String {
    let b64 = |json: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let id_token = format!(
        "{}.{}.placeholder",
        b64(r#"{"alg":"none","typ":"JWT"}"#),
        b64(&format!(
            r#"{{"iss":"https://auth.openai.com","aud":"codex","sub":"openshell-desktop","email":"codex@openshell.local","iat":{now},"exp":{}}}"#,
            now + 365 * 24 * 3600
        )),
    );
    format!(
        r#"if [ -n "$CODEX_AUTH_ACCESS_TOKEN" ]; then
  mkdir -p "$HOME/.codex"
  umask 077
  cat > "$HOME/.codex/auth.json" <<EOF
{{"auth_mode":"chatgptAuthTokens","OPENAI_API_KEY":null,"tokens":{{"id_token":"{id_token}","access_token":"$CODEX_AUTH_ACCESS_TOKEN","refresh_token":"","account_id":"$CODEX_AUTH_ACCOUNT_ID"}},"last_refresh":"$(date -u +%Y-%m-%dT%H:%M:%SZ)"}}
EOF
fi
exec {binary}"#
    )
}

pub const CATALOG: &[Agent] = &[
    Agent {
        id: "claude-code",
        name: "Claude Code",
        description: "Anthropic's coding agent",
        install_path: "/usr/local/bin/claude",
        profiles: &["claude-code"],
        // The sandbox's /usr is read-only, so it cannot update itself; the
        // app installs new versions instead.
        environment: &[("DISABLE_AUTOUPDATER", "1")],
        launch: Launch::Direct,
        package: "@anthropic-ai/claude-code-linux-{arch}",
        tag: "latest",
        layout: Layout::Binary("package/claude"),
    },
    Agent {
        id: "codex",
        name: "Codex",
        description: "OpenAI's coding agent, with your ChatGPT account",
        install_path: "/usr/local/bin/codex",
        profiles: &["codex"],
        environment: &[],
        launch: Launch::CodexSignIn,
        // The main package is a Node launcher; each native build is the same
        // package published under a per-platform version and dist-tag.
        package: "@openai/codex",
        tag: "linux-{arch}",
        // Codex refuses to run without the rest of its package (its
        // manifest, rg, bwrap and helpers) beside it. It goes where npm
        // would put it, which the Codex profile allows.
        layout: Layout::Tree {
            root: "package/vendor/{triple}",
            dir: "/usr/local/lib/node_modules/@openai/codex/vendor/{triple}",
            exe: "bin/codex",
            // By default Codex copies itself into ~/.codex and runs its
            // server from there, a path the profile does not allow, so the
            // gateway would neither let it out nor give it the account.
            // Without auto-start it runs that server in its own process.
            args: " -c features.daemon_auto_start=false",
        },
    },
    Agent {
        id: "opencode",
        name: "OpenCode",
        description: "Open source coding agent, here with OpenRouter models",
        install_path: "/usr/local/bin/opencode",
        profiles: &["openrouter"],
        environment: &[],
        launch: Launch::Direct,
        package: "opencode-linux-{arch}",
        tag: "latest",
        layout: Layout::Binary("package/bin/opencode"),
    },
];

pub fn find(id: &str) -> Option<&'static Agent> {
    CATALOG.iter().find(|agent| agent.id == id)
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    dist: Dist,
}

#[derive(Debug, Deserialize)]
struct Dist {
    tarball: String,
    integrity: String,
}

/// A verified agent build in the local cache: one file, or a directory for
/// a [`Layout::Tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub version: String,
    pub path: PathBuf,
}

/// One step of copying an agent into a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Make this directory unless it exists.
    Dir(String),
    /// Copy a file from the cache.
    File {
        source: PathBuf,
        target: String,
        mode: u32,
        size: u64,
    },
    /// Write these bytes.
    Text {
        target: String,
        mode: u32,
        content: String,
    },
}

impl Download {
    /// What to copy into the sandbox, in order: directories before what goes
    /// in them, and the marker that says the install is complete last.
    pub fn plan(&self, agent: &Agent, arch: Arch) -> Result<Vec<Step>, Error> {
        let Layout::Tree { dir, exe, args, .. } = agent.layout else {
            return Ok(vec![Step::File {
                source: self.path.clone(),
                target: agent.install_path.to_owned(),
                mode: 0o755,
                size: std::fs::metadata(&self.path)?.len(),
            }]);
        };
        let dir = dir.replace("{triple}", arch.triple());
        // Its ancestors may be missing, such as node_modules; `/` is not.
        let mut steps: Vec<Step> = Path::new(&dir)
            .ancestors()
            .skip(1)
            .filter(|path| path.parent().is_some())
            .map(|path| Step::Dir(path.display().to_string()))
            .collect();
        steps.reverse();
        steps.push(Step::Dir(dir.clone()));
        plan_tree(&self.path, &dir, &mut steps)?;
        steps.push(Step::Text {
            target: agent.install_path.to_owned(),
            mode: 0o755,
            content: format!("#!/bin/sh\nexec {dir}/{exe}{args} \"$@\"\n"),
        });
        steps.push(Step::Text {
            target: format!("{dir}/{MARKER}"),
            mode: 0o644,
            content: format!("{}\n", self.version),
        });
        Ok(steps)
    }
}

/// Adds the contents of `source` to `steps`, as copies under `target`.
fn plan_tree(source: &Path, target: &str, steps: &mut Vec<Step>) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries = std::fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let meta = entry.metadata()?;
        let path = format!("{target}/{}", entry.file_name().to_string_lossy());
        if meta.is_dir() {
            steps.push(Step::Dir(path.clone()));
            plan_tree(&entry.path(), &path, steps)?;
        } else {
            steps.push(Step::File {
                source: entry.path(),
                target: path,
                mode: meta.permissions().mode() & 0o777,
                size: meta.len(),
            });
        }
    }
    Ok(())
}

/// Fetches the latest build of `agent` into `cache`, unless that version is
/// already there. `progress` gets bytes received and the total when known.
pub async fn fetch(
    agent: &Agent,
    arch: Arch,
    cache: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<Download, Error> {
    let package = agent.package(arch);
    let client = reqwest::Client::builder()
        .user_agent(concat!("openshell-desktop/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let manifest: Manifest = client
        .get(format!(
            "{REGISTRY}/{}/{}",
            package.replace('/', "%2f"),
            agent.tag(arch)
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let dir = cache.join(format!("{}-{}", agent.id, manifest.version));
    let path = dir.join(agent.id);
    if path.exists() {
        return Ok(Download {
            version: manifest.version,
            path,
        });
    }

    let expected = parse_integrity(&manifest.dist.integrity)?;
    std::fs::create_dir_all(cache)?;
    let mut archive = tempfile_in(cache)?;
    let mut hasher = Sha512::new();
    let mut response = client
        .get(&manifest.dist.tarball)
        .send()
        .await?
        .error_for_status()?;
    let total = response.content_length();
    let mut received = 0;
    while let Some(chunk) = response.chunk().await? {
        hasher.update(&chunk);
        archive.1.write_all(&chunk)?;
        received += chunk.len() as u64;
        progress(received, total);
    }
    if hasher.finalize().as_slice() != expected.as_slice() {
        let _ = std::fs::remove_file(&archive.0);
        return Err(Error::Integrity(format!("{package} {}", manifest.version)));
    }
    drop(archive.1);

    let result = match agent.layout {
        Layout::Binary(entry) => extract(
            &archive.0,
            &entry.replace("{triple}", arch.triple()),
            &dir,
            agent.id,
        )
        .map(drop),
        Layout::Tree { root, .. } => extract_tree(
            &archive.0,
            &root.replace("{triple}", arch.triple()),
            &dir,
            agent.id,
        ),
    };
    let _ = std::fs::remove_file(&archive.0);
    result.map_err(|e| match e {
        Error::MissingEntry { entry, .. } => Error::MissingEntry {
            package: package.clone(),
            entry,
        },
        other => other,
    })?;
    Ok(Download {
        version: manifest.version,
        path,
    })
}

/// Unpacks everything under `root` in the gzipped tarball at `archive` into
/// directory `dir/name`, keeping file modes, through a temporary name so a
/// partial tree never looks cached. Links and other special entries, which
/// npm packages do not have, are left out.
fn extract_tree(archive: &Path, root: &str, dir: &Path, name: &str) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let partial = dir.join(format!(".{name}.partial"));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial)?;
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut found = false;
    for item in tar.entries()? {
        let mut item = item?;
        let path = item.path()?.into_owned();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        found = true;
        // Never write outside the tree, whatever the archive says.
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            continue;
        }
        let out = partial.join(relative);
        match item.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&out)?,
            tar::EntryType::Regular => {
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::File::create(&out)?;
                std::io::copy(&mut item, &mut file)?;
                let mode = item.header().mode()? & 0o777;
                file.set_permissions(std::fs::Permissions::from_mode(mode))?;
                file.sync_all()?;
            }
            _ => {}
        }
    }
    if !found {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(Error::MissingEntry {
            package: String::new(),
            entry: root.to_owned(),
        });
    }
    std::fs::rename(&partial, dir.join(name))?;
    Ok(())
}

/// The SHA-512 digest from an npm `sha512-<base64>` integrity string.
fn parse_integrity(integrity: &str) -> Result<Vec<u8>, Error> {
    integrity
        .split_whitespace()
        .find_map(|item| item.strip_prefix("sha512-"))
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .ok_or_else(|| Error::Metadata(format!("no sha512 integrity in {integrity:?}")))
}

fn tempfile_in(dir: &Path) -> Result<(PathBuf, std::fs::File), Error> {
    let path = dir.join(format!(".download-{}", std::process::id()));
    Ok((path.clone(), std::fs::File::create(&path)?))
}

/// Copies `entry` out of the gzipped tarball at `archive` to `dir/name`,
/// through a temporary name so a partial file never looks cached.
fn extract(archive: &Path, entry: &str, dir: &Path, name: &str) -> Result<u64, Error> {
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for item in tar.entries()? {
        let mut item = item?;
        if item.path()?.as_ref() != Path::new(entry) {
            continue;
        }
        std::fs::create_dir_all(dir)?;
        let partial = dir.join(format!(".{name}.partial"));
        let mut out = std::fs::File::create(&partial)?;
        let size = std::io::copy(&mut item, &mut out)?;
        out.sync_all()?;
        std::fs::rename(&partial, dir.join(name))?;
        return Ok(size);
    }
    Err(Error::MissingEntry {
        package: String::new(),
        entry: entry.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_name_the_architecture() {
        let claude = find("claude-code").unwrap();
        assert_eq!(
            claude.package(Arch::X64),
            "@anthropic-ai/claude-code-linux-x64"
        );
        assert_eq!(
            find("opencode").unwrap().package(Arch::Arm64),
            "opencode-linux-arm64"
        );
        let codex = find("codex").unwrap();
        assert_eq!(codex.package(Arch::X64), "@openai/codex");
        assert_eq!(codex.tag(Arch::Arm64), "linux-arm64");
        assert_eq!(
            codex.installed_check(Arch::X64),
            [
                "test",
                "-f",
                "/usr/local/lib/node_modules/@openai/codex/vendor/x86_64-unknown-linux-musl/.openshell-desktop-installed"
            ]
        );
        assert_eq!(
            find("opencode").unwrap().installed_check(Arch::X64),
            ["test", "-x", "/usr/local/bin/opencode"]
        );
    }

    #[test]
    fn codex_gets_a_sign_in_file_made_of_handles() {
        let home = tempfile::tempdir().unwrap();
        let script = codex_sign_in_script("/bin/true", 1_700_000_000);
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .env("HOME", home.path())
            .env("CODEX_AUTH_ACCESS_TOKEN", "openshell:handle:access")
            .env("CODEX_AUTH_ACCOUNT_ID", "openshell:handle:account")
            .status()
            .unwrap();
        assert!(status.success());
        let auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.path().join(".codex/auth.json")).unwrap())
                .unwrap();
        assert_eq!(auth["auth_mode"], "chatgptAuthTokens");
        assert_eq!(auth["tokens"]["access_token"], "openshell:handle:access");
        assert_eq!(auth["tokens"]["account_id"], "openshell:handle:account");
        assert_eq!(auth["tokens"]["refresh_token"], "");
        let claims = auth["tokens"]["id_token"]
            .as_str()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap()
            .to_owned();
        let claims: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(claims)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["iat"], 1_700_000_000);

        // Without an account, Codex is left to sign in by itself.
        let bare = tempfile::tempdir().unwrap();
        std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .env("HOME", bare.path())
            .env_remove("CODEX_AUTH_ACCESS_TOKEN")
            .status()
            .unwrap();
        assert!(!bare.path().join(".codex").exists());
    }

    #[test]
    fn install_paths_match_the_bundled_profiles() {
        // The upstream profiles allow these binaries; anywhere else and the
        // gateway never injects the account's key.
        assert_eq!(
            find("claude-code").unwrap().install_path,
            "/usr/local/bin/claude"
        );
        assert_eq!(
            find("opencode").unwrap().install_path,
            "/usr/local/bin/opencode"
        );
        assert_eq!(find("codex").unwrap().install_path, "/usr/local/bin/codex");
    }

    #[test]
    fn parses_npm_integrity() {
        let digest = Sha512::digest(b"hello");
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );
        assert_eq!(parse_integrity(&integrity).unwrap(), digest.to_vec());
        assert!(parse_integrity("sha1-abc").is_err());
    }

    /// A gzipped tarball of `files`, as (path, mode, content).
    fn tarball(path: &Path, files: &[(&str, u32, &[u8])]) {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        for (name, mode, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extracts_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pkg.tgz");
        tarball(
            &archive,
            &[
                ("package/README.md", 0o644, b"hi"),
                ("package/claude", 0o755, b"ELF"),
            ],
        );
        let out = dir.path().join("claude-code-1.0");
        assert_eq!(
            extract(&archive, "package/claude", &out, "claude-code").unwrap(),
            3
        );
        assert_eq!(std::fs::read(out.join("claude-code")).unwrap(), b"ELF");
        assert!(matches!(
            extract(&archive, "package/missing", &out, "x"),
            Err(Error::MissingEntry { .. })
        ));
    }

    #[test]
    fn codex_gets_its_whole_package_and_a_launcher() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("codex.tgz");
        let root = "package/vendor/x86_64-unknown-linux-musl";
        tarball(
            &archive,
            &[
                ("package/bin/codex.js", 0o755, b"node"),
                (&format!("{root}/codex-package.json"), 0o644, b"{}"),
                (&format!("{root}/bin/codex"), 0o755, b"ELF"),
                (&format!("{root}/codex-path/rg"), 0o755, b"ELF rg"),
            ],
        );
        let out = dir.path().join("codex-0.1");
        extract_tree(&archive, root, &out, "codex").unwrap();
        let tree = out.join("codex");
        assert_eq!(std::fs::read(tree.join("bin/codex")).unwrap(), b"ELF");
        let mode = |path: &str| {
            std::fs::metadata(tree.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("codex-path/rg"), 0o755);
        assert_eq!(mode("codex-package.json"), 0o644);
        assert!(!tree.join("codex.js").exists());

        let codex = find("codex").unwrap();
        let download = Download {
            version: "0.1".into(),
            path: tree.clone(),
        };
        let steps = download.plan(codex, Arch::X64).unwrap();
        let dir = "/usr/local/lib/node_modules/@openai/codex/vendor/x86_64-unknown-linux-musl";
        let dirs: Vec<&str> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Dir(path) => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            dirs,
            [
                "/usr",
                "/usr/local",
                "/usr/local/lib",
                "/usr/local/lib/node_modules",
                "/usr/local/lib/node_modules/@openai",
                "/usr/local/lib/node_modules/@openai/codex",
                "/usr/local/lib/node_modules/@openai/codex/vendor",
                dir,
                &format!("{dir}/bin"),
                &format!("{dir}/codex-path"),
            ]
        );
        assert!(steps.contains(&Step::File {
            source: tree.join("codex-path/rg"),
            target: format!("{dir}/codex-path/rg"),
            mode: 0o755,
            size: 6,
        }));
        assert_eq!(
            steps[steps.len() - 2],
            Step::Text {
                target: "/usr/local/bin/codex".into(),
                mode: 0o755,
                content: format!(
                    "#!/bin/sh\nexec {dir}/bin/codex -c features.daemon_auto_start=false \"$@\"\n"
                ),
            }
        );
        assert_eq!(
            steps.last().unwrap(),
            &Step::Text {
                target: format!("{dir}/.openshell-desktop-installed"),
                mode: 0o644,
                content: "0.1\n".into(),
            }
        );

        assert!(matches!(
            extract_tree(&archive, "package/vendor/missing", &out, "x"),
            Err(Error::MissingEntry { .. })
        ));
    }
}

/// Downloads the real OpenCode build; run with `cargo test -- --ignored`.
#[cfg(test)]
#[tokio::test]
#[ignore = "downloads about 75 MB from the npm registry"]
async fn fetches_opencode_from_npm() {
    let cache = tempfile::tempdir().unwrap();
    let agent = find("opencode").unwrap();
    let download = fetch(agent, Arch::host().unwrap(), cache.path(), |_, _| {})
        .await
        .unwrap();
    assert!(std::fs::metadata(&download.path).unwrap().len() > 1_000_000);
    let again = fetch(agent, Arch::host().unwrap(), cache.path(), |_, _| {
        panic!("a cached version must not download again")
    })
    .await
    .unwrap();
    assert_eq!(again, download);
}
