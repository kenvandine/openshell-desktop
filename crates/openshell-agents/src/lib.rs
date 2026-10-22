//! The coding agents openshell-desktop can install, and how to fetch them.
//!
//! OpenShell's default sandbox image is bare Ubuntu, so the app brings the
//! agent itself: it downloads the agent's native Linux build from the npm
//! registry, checks it against the registry's SHA-512 integrity value, and
//! keeps the binary in a local cache. The app then copies it into the
//! sandbox at `install_path`, which is where OpenShell's provider profile
//! for that agent expects it; the profile only injects credentials into
//! processes running that binary.

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

/// An agent the New Agent dialog offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Where the binary goes in the sandbox, matching the provider profile.
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
    /// The binary's path inside that package's archive; `{triple}` is the
    /// Rust target, such as `x86_64-unknown-linux-musl`.
    entry: &'static str,
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

    fn entry(&self, arch: Arch) -> String {
        self.entry.replace("{triple}", arch.triple())
    }
}

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
        entry: "package/claude",
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
        entry: "package/vendor/{triple}/bin/codex",
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
        entry: "package/bin/opencode",
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

/// A verified agent binary in the local cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub version: String,
    pub path: PathBuf,
    pub size: u64,
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
    let binary = dir.join(agent.id);
    if let Ok(meta) = std::fs::metadata(&binary) {
        return Ok(Download {
            version: manifest.version,
            path: binary,
            size: meta.len(),
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

    let result = extract(&archive.0, &agent.entry(arch), &dir, agent.id);
    let _ = std::fs::remove_file(&archive.0);
    let size = result.map_err(|e| match e {
        Error::MissingEntry { entry, .. } => Error::MissingEntry {
            package: package.clone(),
            entry,
        },
        other => other,
    })?;
    Ok(Download {
        version: manifest.version,
        path: binary,
        size,
    })
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
            codex.entry(Arch::X64),
            "package/vendor/x86_64-unknown-linux-musl/bin/codex"
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

    #[test]
    fn extracts_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pkg.tgz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut builder = tar::Builder::new(gz);
            for (path, data) in [
                ("package/README.md", &b"hi"[..]),
                ("package/claude", b"ELF"),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, path, data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
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
    assert!(download.size > 1_000_000);
    let again = fetch(agent, Arch::host().unwrap(), cache.path(), |_, _| {
        panic!("a cached version must not download again")
    })
    .await
    .unwrap();
    assert_eq!(again, download);
}
