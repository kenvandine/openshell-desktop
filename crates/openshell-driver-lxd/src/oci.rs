//! Imports OCI images into LXD.
//!
//! Canonical LXD has no OCI support (its image registries speak only
//! simplestreams and LXD), so the driver bridges it: pull the image's
//! manifest, config and layers from the registry, flatten the layers into one
//! root filesystem the way overlayfs would, and upload the result to LXD as a
//! unified image tarball. The LXD image is aliased by manifest digest, so each
//! image is pulled once and every sandbox from it shares the same LXD image.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client as Registry, Reference};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::lxd::{self, LxdError};

#[derive(Debug)]
pub enum ImageError {
    InvalidReference(String),
    Registry(String),
    Unpack(String),
    Lxd(LxdError),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::InvalidReference(e) => write!(f, "invalid image reference: {e}"),
            ImageError::Registry(e) => write!(f, "registry error: {e}"),
            ImageError::Unpack(e) => write!(f, "could not unpack image: {e}"),
            ImageError::Lxd(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<LxdError> for ImageError {
    fn from(error: LxdError) -> Self {
        ImageError::Lxd(error)
    }
}

/// The parts of an OCI image config the driver uses to launch the workload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RuntimeConfig {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportedImage {
    /// LXD image alias, derived from the manifest digest.
    pub alias: String,
    pub manifest_digest: String,
    pub config: RuntimeConfig,
}

#[derive(Deserialize)]
struct ImageConfigFile {
    #[serde(default)]
    config: RuntimeConfig,
}

pub struct Importer {
    registry: Registry,
    staging_dir: PathBuf,
    /// Serialises imports so concurrent creates of one image pull it once.
    lock: Mutex<()>,
}

impl Importer {
    pub fn new(staging_dir: impl Into<PathBuf>) -> Self {
        Self {
            registry: Registry::default(),
            staging_dir: staging_dir.into(),
            lock: Mutex::new(()),
        }
    }

    /// Resolves `reference` to a manifest digest and makes sure LXD holds a
    /// matching image, importing it if needed.
    pub async fn ensure(
        &self,
        lxd: &lxd::Client,
        reference: &str,
    ) -> Result<ImportedImage, ImageError> {
        let reference: Reference = reference
            .parse()
            .map_err(|e| ImageError::InvalidReference(format!("{reference}: {e}")))?;
        let auth = RegistryAuth::Anonymous;
        let (manifest, digest, config) = self
            .registry
            .pull_manifest_and_config(&reference, &auth)
            .await
            .map_err(|e| ImageError::Registry(e.to_string()))?;
        let config: ImageConfigFile = serde_json::from_str(&config)
            .map_err(|e| ImageError::Registry(format!("bad image config: {e}")))?;
        let alias = alias_for_digest(&digest);
        let image = ImportedImage {
            alias: alias.clone(),
            manifest_digest: digest,
            config: config.config,
        };

        let _guard = self.lock.lock().await;
        if lxd.image_alias(&alias).await?.is_some() {
            return Ok(image);
        }

        std::fs::create_dir_all(&self.staging_dir)
            .map_err(|e| ImageError::Unpack(format!("{}: {e}", self.staging_dir.display())))?;
        let staging = tempfile::tempdir_in(&self.staging_dir)
            .map_err(|e| ImageError::Unpack(format!("{}: {e}", self.staging_dir.display())))?;

        let mut layers = Vec::with_capacity(manifest.layers.len());
        for (index, layer) in manifest.layers.iter().enumerate() {
            let path = staging.path().join(format!("layer-{index}"));
            let file = tokio::fs::File::create(&path)
                .await
                .map_err(|e| ImageError::Unpack(e.to_string()))?;
            self.registry
                .pull_blob(&reference, layer, file)
                .await
                .map_err(|e| ImageError::Registry(format!("layer {}: {e}", layer.digest)))?;
            layers.push(path);
        }

        let tarball = staging.path().join("image.tar");
        let description = format!("OpenShell OCI image {reference}");
        let output = tarball.clone();
        tokio::task::spawn_blocking(move || {
            let file = File::create(&output).map_err(|e| e.to_string())?;
            write_unified_tarball(&layers, &description, BufWriter::new(file))
        })
        .await
        .map_err(|e| ImageError::Unpack(e.to_string()))?
        .map_err(ImageError::Unpack)?;

        lxd.import_image(&tarball, &alias).await?;
        Ok(image)
    }
}

impl Importer {
    /// Returns one regular file from the image's merged filesystem, caching
    /// it on disk by manifest digest. The runtime image that ships
    /// `openshell-sandbox` is a single binary, so this avoids importing it
    /// into LXD at all.
    pub async fn extract_file(
        &self,
        reference: &str,
        path: &str,
    ) -> Result<ExtractedFile, ImageError> {
        let reference: Reference = reference
            .parse()
            .map_err(|e| ImageError::InvalidReference(format!("{reference}: {e}")))?;
        let auth = RegistryAuth::Anonymous;
        let (manifest, digest, _) = self
            .registry
            .pull_manifest_and_config(&reference, &auth)
            .await
            .map_err(|e| ImageError::Registry(e.to_string()))?;

        let cache = self.staging_dir.join("files").join(format!(
            "{}-{}",
            alias_for_digest(&digest),
            path.trim_matches('/').replace('/', "_")
        ));
        if let Ok(content) = std::fs::read(&cache) {
            return Ok(ExtractedFile {
                manifest_digest: digest,
                content,
            });
        }

        let _guard = self.lock.lock().await;
        std::fs::create_dir_all(&self.staging_dir)
            .map_err(|e| ImageError::Unpack(format!("{}: {e}", self.staging_dir.display())))?;
        let staging = tempfile::tempdir_in(&self.staging_dir)
            .map_err(|e| ImageError::Unpack(format!("{}: {e}", self.staging_dir.display())))?;
        let mut layers = Vec::with_capacity(manifest.layers.len());
        for (index, layer) in manifest.layers.iter().enumerate() {
            let layer_path = staging.path().join(format!("layer-{index}"));
            let file = tokio::fs::File::create(&layer_path)
                .await
                .map_err(|e| ImageError::Unpack(e.to_string()))?;
            self.registry
                .pull_blob(&reference, layer, file)
                .await
                .map_err(|e| ImageError::Registry(format!("layer {}: {e}", layer.digest)))?;
            layers.push(layer_path);
        }
        let wanted = path.to_owned();
        let content = tokio::task::spawn_blocking(move || extract_from_layers(&layers, &wanted))
            .await
            .map_err(|e| ImageError::Unpack(e.to_string()))?
            .map_err(ImageError::Unpack)?;

        if let Some(parent) = cache.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cache, &content);
        Ok(ExtractedFile {
            manifest_digest: digest,
            content,
        })
    }
}

/// A file read out of an OCI image.
#[derive(Debug, Clone)]
pub struct ExtractedFile {
    pub manifest_digest: String,
    pub content: Vec<u8>,
}

/// Finds `path` in layers merged bottom-first. Only regular files are
/// returned; the topmost layer that mentions the path wins, and a whiteout
/// above it removes the file.
pub fn extract_from_layers(layers: &[PathBuf], path: &str) -> Result<Vec<u8>, String> {
    let wanted = normalize(Path::new(path)).ok_or_else(|| format!("bad path {path}"))?;
    let (parent, name) = split_parent(&wanted);
    let whiteout = join(parent, &format!(".wh.{name}"));
    for layer in layers.iter().rev() {
        let mut archive = open_layer(layer)?;
        let mut found = None;
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let Some(entry_path) = normalize(&entry.path().map_err(|e| e.to_string())?) else {
                continue;
            };
            if entry_path == whiteout {
                return Err(format!("{path} is removed by an image layer"));
            }
            if entry_path == wanted {
                if !entry.header().entry_type().is_file() {
                    return Err(format!("{path} in the image is not a regular file"));
                }
                let mut content = Vec::new();
                entry.read_to_end(&mut content).map_err(|e| e.to_string())?;
                found = Some(content);
            }
        }
        if let Some(content) = found {
            return Ok(content);
        }
    }
    Err(format!("{path} not found in the image"))
}

/// LXD alias for an image with this manifest digest (`sha256:<hex>`).
pub fn alias_for_digest(digest: &str) -> String {
    let hex = digest.split_once(':').map_or(digest, |(_, hex)| hex);
    format!("openshell-oci-{hex}")
}

fn lxd_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "riscv64" => "riscv64",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        other => other,
    }
}

/// Writes an LXD unified image (`metadata.yaml` + `rootfs/`) whose root
/// filesystem is `layers` (bottom first) merged with OCI whiteout semantics.
pub fn write_unified_tarball(
    layers: &[PathBuf],
    description: &str,
    out: impl Write,
) -> Result<(), String> {
    let owners = layer_owners(layers)?;
    let mut builder = tar::Builder::new(out);

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let metadata = format!(
        "architecture: {}\ncreation_date: {created}\nproperties:\n  description: {}\n  os: oci\n",
        lxd_architecture(),
        serde_json::to_string(description).map_err(|e| e.to_string())?,
    );
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(created);
    builder
        .append_data(&mut header, "metadata.yaml", metadata.as_bytes())
        .map_err(|e| e.to_string())?;
    append_dir(&mut builder, "rootfs", 0o755, created)?;

    let mut present = HashSet::new();
    for (index, layer) in layers.iter().enumerate() {
        let mut archive = open_layer(layer)?;
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let Some(path) = normalize(&entry.path().map_err(|e| e.to_string())?) else {
                continue;
            };
            if is_whiteout(&path) || owners.get(&path) != Some(&index) {
                continue;
            }
            let mut header = entry.header().clone();
            let target = format!("rootfs/{path}");
            let kind = header.entry_type();
            if kind.is_hard_link() {
                let link = entry
                    .link_name()
                    .map_err(|e| e.to_string())?
                    .and_then(|link| normalize(&link))
                    .ok_or_else(|| format!("hard link {path} has no target"))?;
                builder
                    .append_link(&mut header, &target, format!("rootfs/{link}"))
                    .map_err(|e| e.to_string())?;
            } else if kind.is_symlink() {
                let link = entry
                    .link_name()
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("symlink {path} has no target"))?
                    .into_owned();
                builder
                    .append_link(&mut header, &target, link)
                    .map_err(|e| e.to_string())?;
            } else {
                builder
                    .append_data(&mut header, &target, &mut entry)
                    .map_err(|e| e.to_string())?;
            }
            present.insert(path);
        }
    }

    // LXC mounts these at start; minimal images may not ship them.
    for (dir, mode) in [
        ("dev", 0o755),
        ("proc", 0o555),
        ("sys", 0o555),
        ("run", 0o755),
        ("tmp", 0o1777),
    ] {
        if !present.contains(dir) {
            append_dir(&mut builder, &format!("rootfs/{dir}"), mode, created)?;
        }
    }
    builder
        .into_inner()
        .and_then(|mut out| out.flush())
        .map_err(|e| e.to_string())
}

/// Decides which layer provides each path of the merged filesystem. Walks the
/// layers top-down: an upper layer's entry shadows lower ones, a whiteout
/// (`.wh.<name>`) deletes a lower path, an opaque marker (`.wh..wh..opq`)
/// hides a directory's lower contents, and an upper non-directory hides
/// everything a lower layer had beneath that path.
pub fn layer_owners(layers: &[PathBuf]) -> Result<HashMap<String, usize>, String> {
    let mut owners: HashMap<String, usize> = HashMap::new();
    let mut deleted: HashSet<String> = HashSet::new();
    let mut opaque: HashSet<String> = HashSet::new();
    let mut files: HashSet<String> = HashSet::new();

    for (index, layer) in layers.iter().enumerate().rev() {
        let mut layer_deleted = Vec::new();
        let mut layer_opaque = Vec::new();
        let mut layer_files = Vec::new();

        let mut archive = open_layer(layer)?;
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let Some(path) = normalize(&entry.path().map_err(|e| e.to_string())?) else {
                continue;
            };
            let (parent, name) = split_parent(&path);
            if name == ".wh..wh..opq" {
                layer_opaque.push(parent.to_owned());
                continue;
            }
            if let Some(hidden) = name.strip_prefix(".wh.") {
                layer_deleted.push(join(parent, hidden));
                continue;
            }
            if hidden_by_upper(&path, &deleted, &opaque, &files) {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = owners.entry(path.clone()) {
                slot.insert(index);
                if !entry.header().entry_type().is_dir() {
                    layer_files.push(path);
                }
            }
        }
        deleted.extend(layer_deleted);
        opaque.extend(layer_opaque);
        files.extend(layer_files);
    }
    Ok(owners)
}

fn hidden_by_upper(
    path: &str,
    deleted: &HashSet<String>,
    opaque: &HashSet<String>,
    files: &HashSet<String>,
) -> bool {
    if deleted.contains(path) {
        return true;
    }
    let mut ancestor = path;
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        if deleted.contains(parent) || opaque.contains(parent) || files.contains(parent) {
            return true;
        }
        ancestor = parent;
    }
    // Top-level entries sit in the root directory, which may itself be opaque.
    opaque.contains("")
}

fn is_whiteout(path: &str) -> bool {
    split_parent(path).1.starts_with(".wh.")
}

fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// Turns a tar path into a clean relative path; `None` for the root itself
/// and for paths that try to escape it.
fn normalize(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            std::path::Component::CurDir | std::path::Component::RootDir => {}
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn append_dir(
    builder: &mut tar::Builder<impl Write>,
    path: &str,
    mode: u32,
    mtime: u64,
) -> Result<(), String> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(mode);
    header.set_mtime(mtime);
    builder
        .append_data(&mut header, path, std::io::empty())
        .map_err(|e| e.to_string())
}

/// Opens a layer blob, decompressing gzip when present. OCI layers are tar,
/// tar+gzip or tar+zstd; zstd is not supported yet.
fn open_layer(path: &Path) -> Result<tar::Archive<Box<dyn Read>>, String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let reader: Box<dyn Read> = match &magic[..read] {
        [0x1f, 0x8b, ..] => Box::new(GzDecoder::new(BufReader::new(file))),
        [0x28, 0xb5, 0x2f, 0xfd] => {
            return Err("zstd-compressed layers are not supported yet".into());
        }
        _ => Box::new(BufReader::new(file)),
    };
    Ok(tar::Archive::new(reader))
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Item<'a> {
        Dir(&'a str),
        File(&'a str, &'a str),
        Symlink(&'a str, &'a str),
    }

    fn layer(dir: &Path, name: &str, items: &[Item], gzip: bool) -> PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).unwrap();
        let out: Box<dyn Write> = if gzip {
            Box::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::fast(),
            ))
        } else {
            Box::new(file)
        };
        let mut builder = tar::Builder::new(out);
        for item in items {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            match item {
                Item::Dir(p) => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    builder
                        .append_data(&mut header, p, std::io::empty())
                        .unwrap();
                }
                Item::File(p, content) => {
                    header.set_size(content.len() as u64);
                    builder
                        .append_data(&mut header, p, content.as_bytes())
                        .unwrap();
                }
                Item::Symlink(p, target) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    builder.append_link(&mut header, p, target).unwrap();
                }
            }
        }
        builder.into_inner().unwrap().flush().unwrap();
        path
    }

    fn unpack(tarball: &[u8]) -> HashMap<String, String> {
        let mut archive = tar::Archive::new(tarball);
        let mut files = HashMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let value = if entry.header().entry_type().is_symlink() {
                format!("-> {}", entry.link_name().unwrap().unwrap().display())
            } else {
                let mut content = String::new();
                entry.read_to_string(&mut content).unwrap();
                content
            };
            files.insert(path, value);
        }
        files
    }

    #[test]
    fn merges_layers_with_whiteouts() {
        let dir = tempfile::tempdir().unwrap();
        let base = layer(
            dir.path(),
            "base",
            &[
                Item::Dir("etc/"),
                Item::File("etc/passwd", "root:x:0:0::/root:/bin/sh\n"),
                Item::File("etc/motd", "old"),
                Item::Dir("opt/app/"),
                Item::File("opt/app/stale", "stale"),
                Item::Dir("var/cache/"),
                Item::File("var/cache/a", "cached"),
                Item::Symlink("bin", "usr/bin"),
            ],
            true,
        );
        let upper = layer(
            dir.path(),
            "upper",
            &[
                Item::File("etc/.wh.motd", ""),
                Item::File(
                    "etc/passwd",
                    "root:x:0:0::/root:/bin/sh\nagent:x:1000:1000::/home/agent:/bin/sh\n",
                ),
                Item::Dir("opt/app/"),
                Item::File("opt/app/.wh..wh..opq", ""),
                Item::File("opt/app/fresh", "fresh"),
                Item::File("var/cache", "now a file"),
            ],
            false,
        );

        let mut out = Vec::new();
        write_unified_tarball(&[base, upper], "test image", &mut out).unwrap();
        let files = unpack(&out);

        assert!(files["metadata.yaml"].contains("os: oci"));
        assert!(files["rootfs/etc/passwd"].contains("agent:x:1000"));
        assert!(!files.contains_key("rootfs/etc/motd"));
        assert!(!files.contains_key("rootfs/etc/.wh.motd"));
        assert!(!files.contains_key("rootfs/opt/app/stale"));
        assert_eq!(files["rootfs/opt/app/fresh"], "fresh");
        assert_eq!(files["rootfs/var/cache"], "now a file");
        assert!(!files.contains_key("rootfs/var/cache/a"));
        assert_eq!(files["rootfs/bin"], "-> usr/bin");
        for dir in [
            "rootfs/proc",
            "rootfs/sys",
            "rootfs/dev",
            "rootfs/tmp",
            "rootfs/run",
        ] {
            assert!(files.contains_key(dir), "{dir} missing");
        }
    }

    #[test]
    fn extracts_files_from_the_topmost_layer() {
        let dir = tempfile::tempdir().unwrap();
        let base = layer(
            dir.path(),
            "base",
            &[
                Item::File("openshell-sandbox", "v1"),
                Item::File("other", "x"),
            ],
            true,
        );
        let upper = layer(
            dir.path(),
            "upper",
            &[Item::File("openshell-sandbox", "v2")],
            false,
        );
        let removed = layer(dir.path(), "removed", &[Item::File(".wh.other", "")], false);
        let layers = [base, upper, removed];
        assert_eq!(
            extract_from_layers(&layers, "/openshell-sandbox").unwrap(),
            b"v2"
        );
        assert!(extract_from_layers(&layers, "/other").is_err());
        assert!(extract_from_layers(&layers, "/missing").is_err());
    }

    #[test]
    fn rejects_zstd_layers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zstd");
        std::fs::write(&path, [0x28, 0xb5, 0x2f, 0xfd, 0, 0]).unwrap();
        let error = write_unified_tarball(&[path], "x", Vec::new()).unwrap_err();
        assert!(error.contains("zstd"));
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(
            normalize(Path::new("./etc/passwd")).as_deref(),
            Some("etc/passwd")
        );
        assert_eq!(
            normalize(Path::new("/usr/bin/")).as_deref(),
            Some("usr/bin")
        );
        assert_eq!(normalize(Path::new("./")), None);
        assert_eq!(normalize(Path::new("../escape")), None);
    }

    #[test]
    fn aliases_by_digest() {
        assert_eq!(alias_for_digest("sha256:abc123"), "openshell-oci-abc123");
    }
}
