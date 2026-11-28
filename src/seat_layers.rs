//! SEAT TOOLCHAIN LAYERS (core#1043, program DES-1043): a seat image carries the toolchains of
//! the repository its unit works in, composed from a fixed catalog on top of the wicked layer.
//!
//! ## What the default seat base already has
//!
//! The default seat base is the checks floor image (`containers/checks/node22-rust.Dockerfile`):
//! node and npm, the pinned Rust toolchain with clippy and rustfmt on Debian's default PATH,
//! python3 with pytest, git and a C toolchain. So `rust` and `node` are detected and reported
//! (their package hosts matter for the egress note), but add nothing to the image.
//!
//! ## What a layer adds
//!
//! `go`, `java` and `python` (uv) each carry an install recipe, inserted into the seat recipe just
//! before its `ARG CLI` line ([`compose`]): the layers are built once per layer set and shared by
//! every CLI's image. Everything comes from Debian's own archive (the source both images already
//! use), except uv, which Debian does not package: its release tarball is fetched over https and
//! checked against the sha256 pinned here for the architecture BEFORE anything of it is unpacked.
//!
//! Every toolchain works on Debian's default PATH with no image `ENV`: landrun hands the CLI only
//! PATH, HOME, TMPDIR, LANG and the variables the carrier sets (src/k8s/seat.rs), never the
//! image's own environment. Each recipe ends by running its tool under `env -i` with that PATH.
//!
//! ## Detection
//!
//! [`detect`] looks for each layer's marker files (regular files; a link is never followed) at the
//! tree's root and in its immediate subdirectories, skipping hidden ones and dependency or build
//! output directories. It reads names only, never a file's content. The tree is the unit's (seat
//! writable): a seat that adds a `go.mod` gets the Go layer on the next unit, which is wanted, and
//! the most it can cause is one image build from this fixed catalog.
//!
//! ## What wins over detection
//!
//! An operator seat base (`WICKED_SEAT_BASE_IMAGE`) is the operator's toolchain: no layer is
//! composed on it. `WICKED_SEAT_IMAGE_<CLI>` is used verbatim, as before. [`LAYERS_ENV`] `= off`
//! turns detection off for the daemon.
//!
//! ## Egress
//!
//! Never widened. Each layer names its package and download hosts; when the daemon's CONNECT
//! allowlist is not open and does not cover them, the unit is told which ones ([`egress_note`]).

use std::collections::BTreeSet;
use std::path::Path;

/// `off` turns layer detection off for this daemon (seats get the plain seat image).
pub(crate) const LAYERS_ENV: &str = "WICKED_SEAT_LAYERS";

/// The most subdirectories of the tree's root [`detect`] reads.
const MAX_SUBDIRS: usize = 256;

/// Directories [`detect`] never looks into: dependency and build output.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "out",
    "venv",
];

/// One toolchain layer of the catalog.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Layer {
    /// The layer's id (`go`, `java`, `node`, `python`, `rust`).
    pub(crate) id: &'static str,
    /// File names that mark a repository needing it; one `*` matches any run of characters.
    markers: &'static [&'static str],
    /// Its install recipe (Dockerfile instructions); empty when the default seat base has it.
    pub(crate) recipe: &'static str,
    /// The package and download hosts its tools reach.
    pub(crate) hosts: &'static [&'static str],
}

impl Layer {
    /// Whether the layer changes the seat image (it has a recipe).
    pub(crate) fn builds(&self) -> bool {
        !self.recipe.is_empty()
    }
}

/// Debian's default PATH, the only PATH a toolchain may rely on (the seat floor's `IMAGE_PATH`).
macro_rules! default_path {
    () => {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    };
}

const GO_RECIPE: &str = concat!(
    "# go (core#1043): Debian's golang-go. GOTOOLCHAIN=auto in its go.env, so a go.mod naming a\n",
    "# newer go fetches that toolchain through the module proxy (Debian ships it set to local).\n",
    "RUN set -eu; \\\n",
    "    apt-get update; \\\n",
    "    apt-get install -y --no-install-recommends golang-go; \\\n",
    "    rm -rf /var/lib/apt/lists/*; \\\n",
    "    env=\"$(go env GOROOT)/go.env\"; \\\n",
    "    if grep -q '^GOTOOLCHAIN=' \"$env\"; then sed -i 's/^GOTOOLCHAIN=.*/GOTOOLCHAIN=auto/' \"$env\"; \\\n",
    "    else echo 'GOTOOLCHAIN=auto' >> \"$env\"; fi; \\\n",
    "    env -i PATH=",
    default_path!(),
    " HOME=/nonexistent go version\n",
);

const JAVA_RECIPE: &str = concat!(
    "# java (core#1043): Debian's OpenJDK 21 and Maven (a Gradle repository runs its own wrapper).\n",
    "# A slim base has no man dirs, which the JDK's alternatives need.\n",
    "RUN set -eu; \\\n",
    "    mkdir -p /usr/share/man/man1; \\\n",
    "    apt-get update; \\\n",
    "    apt-get install -y --no-install-recommends openjdk-21-jdk-headless maven; \\\n",
    "    rm -rf /var/lib/apt/lists/*; \\\n",
    "    env -i PATH=",
    default_path!(),
    " HOME=/nonexistent sh -c 'java -version && javac -version && mvn -v'\n",
);

const PYTHON_RECIPE: &str = concat!(
    "# python (core#1043): uv 0.13.9, its release tarball checked against the sha256 pinned per\n",
    "# architecture before it is unpacked (Debian does not package uv); uv and uvx root-owned and\n",
    "# read-only in /usr/local/bin. python3, venv and pytest come from the seat image itself.\n",
    "RUN set -eu; \\\n",
    "    case \"$(dpkg --print-architecture)\" in \\\n",
    "      amd64) t=x86_64-unknown-linux-gnu; sum=a7fbaff2b9a271d85c1bdb540941090fd1ce011797b4abeb48e4dcd39c2471d5 ;; \\\n",
    "      arm64) t=aarch64-unknown-linux-gnu; sum=0797542018e541a9288855df555c8077eed06f7a47d206362030480e8ecc9f3d ;; \\\n",
    "      *) echo \"wicked-seat: no uv is pinned for $(dpkg --print-architecture)\" >&2; exit 1 ;; \\\n",
    "    esac; \\\n",
    "    d=\"$(mktemp -d)\"; \\\n",
    "    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o \"$d/uv.tar.gz\" \\\n",
    "      \"https://github.com/astral-sh/uv/releases/download/0.13.9/uv-$t.tar.gz\"; \\\n",
    "    echo \"$sum  $d/uv.tar.gz\" | sha256sum -c -; \\\n",
    "    tar -xzf \"$d/uv.tar.gz\" -C \"$d\" --no-same-owner \"uv-$t/uv\" \"uv-$t/uvx\"; \\\n",
    "    install -o root -g root -m 0555 \"$d/uv-$t/uv\" \"$d/uv-$t/uvx\" /usr/local/bin/; \\\n",
    "    rm -rf \"$d\"; \\\n",
    "    env -i PATH=",
    default_path!(),
    " HOME=/nonexistent uv --version\n",
);

/// The GitHub hosts a release download is redirected through.
const GITHUB_DOWNLOADS: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The catalog, sorted by id (the composition and tag order).
pub(crate) static CATALOG: &[Layer] = &[
    Layer {
        id: "go",
        markers: &["go.mod", "go.work"],
        recipe: GO_RECIPE,
        // The proxy redirects toolchain zips to storage.googleapis.com.
        hosts: &[
            "proxy.golang.org",
            "sum.golang.org",
            "storage.googleapis.com",
        ],
    },
    Layer {
        id: "java",
        markers: &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        recipe: JAVA_RECIPE,
        // The Gradle wrapper's distribution redirects to GitHub; plugin jars to plugins-artifacts.
        hosts: &[
            "repo.maven.apache.org",
            "services.gradle.org",
            "downloads.gradle.org",
            "plugins.gradle.org",
            "plugins-artifacts.gradle.org",
            GITHUB_DOWNLOADS[0],
            GITHUB_DOWNLOADS[1],
            GITHUB_DOWNLOADS[2],
        ],
    },
    Layer {
        id: "node",
        markers: &["package.json"],
        recipe: "",
        hosts: &["registry.npmjs.org"],
    },
    Layer {
        id: "python",
        markers: &[
            "pyproject.toml",
            "requirements*.txt",
            "setup.py",
            "uv.lock",
            "Pipfile",
        ],
        recipe: PYTHON_RECIPE,
        // uv's managed Pythons come from GitHub releases and python.org.
        hosts: &[
            "pypi.org",
            "files.pythonhosted.org",
            "downloads.python.org",
            GITHUB_DOWNLOADS[0],
            GITHUB_DOWNLOADS[1],
            GITHUB_DOWNLOADS[2],
        ],
    },
    Layer {
        id: "rust",
        markers: &["Cargo.toml"],
        recipe: "",
        hosts: &["crates.io", "index.crates.io", "static.crates.io"],
    },
];

/// Whether `name` matches `marker` (one `*` matches any run of characters).
fn marker_matches(marker: &str, name: &str) -> bool {
    match marker.split_once('*') {
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post)
        }
        None => marker == name,
    }
}

/// The most entries [`detect`] keeps of any one directory, the first by name: a repository's root
/// and its top-level directories hold far fewer.
const MAX_ENTRIES: usize = 4096;

/// The most entries [`detect`] lists in any one directory before it sorts them, so a directory
/// a seat filled with names cannot make detection slow. Below it the kept set is the first
/// [`MAX_ENTRIES`] by name, whatever order the filesystem lists them in.
const MAX_LISTED: usize = 65536;

/// The first [`MAX_ENTRIES`] of `listed` by name, of at most [`MAX_LISTED`] read.
fn first_by_name(listed: impl Iterator<Item = (String, bool, bool)>) -> Vec<(String, bool, bool)> {
    let mut out: Vec<(String, bool, bool)> = listed.take(MAX_LISTED).collect();
    out.sort();
    out.truncate(MAX_ENTRIES);
    out
}

/// `dir`'s entries as (name, is a regular file, is a directory), never following a link
/// (`DirEntry::file_type` does not): the first [`MAX_ENTRIES`] by name ([`first_by_name`]).
fn entries(dir: &Path) -> Vec<(String, bool, bool)> {
    #[cfg(test)]
    DIR_READS.with(|n| n.set(n.get() + 1));
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    first_by_name(read.flatten().filter_map(|e| {
        let t = e.file_type().ok()?;
        Some((e.file_name().to_str()?.to_string(), t.is_file(), t.is_dir()))
    }))
}

#[cfg(test)]
thread_local! {
    /// (core#1933) The directories [`detect`] listed on this thread, for tests that count them.
    pub(crate) static DIR_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Add the ids of the layers whose markers name one of `files`.
fn mark(files: &[(String, bool, bool)], found: &mut BTreeSet<&'static str>) {
    for (name, file, _) in files {
        if !file {
            continue;
        }
        for layer in CATALOG {
            if layer.markers.iter().any(|m| marker_matches(m, name)) {
                found.insert(layer.id);
            }
        }
    }
}

/// The layers `tree` needs: marker files (regular files, never a link) at its root and in its
/// immediate subdirectories (not hidden, not dependency or build output, the first
/// [`MAX_SUBDIRS`] by name), keeping the first [`MAX_ENTRIES`] entries of each directory by name and
/// stopping once every layer is found; in catalog order. Names only: no file is opened.
pub(crate) fn detect(tree: &Path) -> Vec<&'static Layer> {
    let mut found = BTreeSet::new();
    let root = entries(tree);
    mark(&root, &mut found);
    let subdirs = root
        .iter()
        .filter(|(n, _, dir)| *dir && !n.starts_with('.') && !SKIP_DIRS.contains(&n.as_str()))
        .take(MAX_SUBDIRS);
    for (name, _, _) in subdirs {
        if found.len() == CATALOG.len() {
            break;
        }
        mark(&entries(&tree.join(name)), &mut found);
    }
    CATALOG.iter().filter(|l| found.contains(l.id)).collect()
}

/// (core#1933) How long [`detect_cached`] reuses a checkout's detection.
const DETECT_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// (core#1933) The most checkout roots [`detect_cached`] remembers.
const DETECT_CACHE_MAX: usize = 256;

/// (core#1933, core#2047) What a read a person sees derived from one checkout root, per root:
/// when it ran, and what it found. Each derivation keeps its own map and shares [`cached_at`].
pub(crate) type CheckoutCache<V> =
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, (std::time::Instant, V)>>;

/// (core#1933) Each checkout root's last detection: its layer ids.
static DETECTED: std::sync::LazyLock<CheckoutCache<Vec<&'static str>>> =
    std::sync::LazyLock::new(Default::default);

/// (core#1933) [`detect`] for the repo list a person sees, reused for [`DETECT_TTL`]: a marker
/// added or removed in the checkout shows within that time. Not for the arm, which detects on the
/// unit's own worktree every time.
pub(crate) fn detect_cached(tree: &Path) -> Vec<&'static Layer> {
    detect_cached_at(tree, std::time::Instant::now())
}

/// [`detect_cached`] at the time `now`.
fn detect_cached_at(tree: &Path, now: std::time::Instant) -> Vec<&'static Layer> {
    let ids = cached_at(&*DETECTED, tree, now, || {
        detect(tree).iter().map(|l| l.id).collect()
    });
    CATALOG.iter().filter(|l| ids.contains(&l.id)).collect()
}

/// (core#1933, core#2047) `derive` for the checkout at `tree`, reused from `cache` for
/// [`DETECT_TTL`]; at most [`DETECT_CACHE_MAX`] roots are kept (the expired go first, then the
/// oldest). The one cache behind every checkout derivation the repo list shows.
pub(crate) fn cached_at<V: Clone>(
    cache: &CheckoutCache<V>,
    tree: &Path,
    now: std::time::Instant,
    derive: impl FnOnce() -> V,
) -> V {
    let lock = || cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, found)) = lock().get(tree) {
        if now.saturating_duration_since(*at) < DETECT_TTL {
            return found.clone();
        }
    }
    let found = derive();
    let mut map = lock();
    if map.len() >= DETECT_CACHE_MAX && !map.contains_key(tree) {
        map.retain(|_, (at, _)| now.saturating_duration_since(*at) < DETECT_TTL);
        if map.len() >= DETECT_CACHE_MAX {
            let oldest = map
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                map.remove(&k);
            }
        }
    }
    map.insert(tree.to_path_buf(), (now, found.clone()));
    found
}

/// (core#2047, DES-2047b) [`cached_at`] read through `clock`. Here it reads the clock once, before
/// the derivation, so an entry is stamped with the time it was asked for (the fix stamps it when
/// it lands).
#[cfg(test)]
pub(crate) fn cached_at_clocked<V: Clone>(
    cache: &CheckoutCache<V>,
    tree: &Path,
    clock: &dyn Fn() -> std::time::Instant,
    derive: impl FnOnce() -> V,
) -> V {
    cached_at(cache, tree, clock(), derive)
}

/// (core#1933) Forget `tree`'s detection, as [`DETECT_TTL`] passing would.
#[cfg(test)]
pub(crate) fn forget_detected(tree: &Path) {
    DETECTED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(tree);
}

/// [`detect`] unless `setting` ([`LAYERS_ENV`]'s value) is `off`.
pub(crate) fn for_tree_with(tree: &Path, setting: Option<&str>) -> Vec<&'static Layer> {
    if setting.is_some_and(|v| v.trim().eq_ignore_ascii_case("off")) {
        return Vec::new();
    }
    detect(tree)
}

/// (core#1043 part 2) [`for_tree_with`] without the layers a repo turned off (its
/// `RepoEntry::seat_layers_off`, carried to the arm on the unit's governance). An id the tree
/// does not need changes nothing.
pub(crate) fn for_tree_except(
    tree: &Path,
    setting: Option<&str>,
    off: &[String],
) -> Vec<&'static Layer> {
    for_tree_with(tree, setting)
        .into_iter()
        .filter(|l| !off.iter().any(|o| o == l.id))
        .collect()
}

/// (core#1043 part 2) The repo card's rows for the checkout at `tree`: every layer it is detected
/// to need, in catalog order, and what turned one off. `off` is the repo's own switch; `setting`
/// is [`LAYERS_ENV`]'s value and `operator_base` whether the operator set a seat base image, either
/// of which turns every layer off for every repo. (part 3) `repo_base` is whether the repo has its
/// own seat base image, which turns every layer off for it and is named over the others.
pub(crate) fn rows(
    tree: &Path,
    off: &[String],
    setting: Option<&str>,
    operator_base: bool,
    repo_base: bool,
) -> Vec<crate::repo::RepoSeatLayer> {
    let daemon_off = setting.is_some_and(|v| v.trim().eq_ignore_ascii_case("off"));
    detect_cached(tree)
        .into_iter()
        .map(|l| {
            // (core#1043 part 3) The repo's own base wins over the daemon's, as at the arm.
            let off_by = if repo_base {
                Some("repo_base")
            } else if operator_base {
                Some("operator_base")
            } else if daemon_off {
                Some("daemon")
            } else if off.iter().any(|o| o == l.id) {
                Some("repo")
            } else {
                None
            };
            crate::repo::RepoSeatLayer {
                id: l.id.to_string(),
                enabled: off_by.is_none(),
                off_by: off_by.map(str::to_string),
            }
        })
        .collect()
}

/// The layers a comma-separated id list names (`seat-image --layers`), in catalog order; empty or
/// `none` names none. An id the catalog does not have is refused by name.
pub(crate) fn parse(list: &str) -> Result<Vec<&'static Layer>, String> {
    if list.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut wanted = BTreeSet::new();
    for id in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !is_layer_id(id) {
            return Err(unknown_layer(id));
        }
        wanted.insert(id);
    }
    Ok(CATALOG.iter().filter(|l| wanted.contains(l.id)).collect())
}

/// `id` is exactly a layer id in the catalog (no trimming, no list).
pub(crate) fn is_layer_id(id: &str) -> bool {
    CATALOG.iter().any(|l| l.id == id)
}

/// The refusal of `id`, which is not a layer, naming the layers there are.
pub(crate) fn unknown_layer(id: &str) -> String {
    let known: Vec<&str> = CATALOG.iter().map(|l| l.id).collect();
    format!("no seat layer `{id}` (the layers are {})", known.join(", "))
}

/// The layers of `layers` that change the image, in catalog order.
pub(crate) fn built<'a>(layers: &[&'a Layer]) -> Vec<&'a Layer> {
    let mut out: Vec<&Layer> = layers.iter().copied().filter(|l| l.builds()).collect();
    out.sort_by_key(|l| l.id);
    out.dedup_by_key(|l| l.id);
    out
}

/// What the layers add to a seat image's tag: each built layer's id and recipe. Empty when no
/// layer builds, so a seat image without layers keeps the tag it always had.
pub(crate) fn tag_parts(layers: &[&Layer]) -> Vec<String> {
    built(layers)
        .iter()
        .map(|l| format!("layer:{}\n{}", l.id, l.recipe))
        .collect()
}

/// The Dockerfile instructions `built` (layers that build, in catalog order) add, labelled
/// `wicked.seat.layers`.
pub(crate) fn layer_block(built: &[&Layer]) -> String {
    let ids: Vec<&str> = built.iter().map(|l| l.id).collect();
    let mut block = format!(
        "# wicked seat toolchain layers (core#1043): {}\n",
        ids.join(",")
    );
    for l in built {
        block.push_str(l.recipe);
    }
    block.push_str(&format!("LABEL wicked.seat.layers=\"{}\"\n", ids.join(",")));
    block
}

/// `dockerfile` (a seat recipe) with the built layers' recipes inserted just before its
/// `ARG CLI` line, labelled `wicked.seat.layers`. Unchanged when no layer builds.
pub(crate) fn compose(dockerfile: &str, layers: &[&Layer]) -> String {
    let built = built(layers);
    if built.is_empty() {
        return dockerfile.to_string();
    }
    let block = layer_block(&built);
    let mut out = String::with_capacity(dockerfile.len() + block.len());
    let mut placed = false;
    for line in dockerfile.split_inclusive('\n') {
        if !placed && line.trim_end() == "ARG CLI" {
            out.push_str(&block);
            placed = true;
        }
        out.push_str(line);
    }
    if !placed {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
    }
    out
}

/// Whether the allowlist entry `pattern` admits `host`: the same name, or a `*.` suffix wildcard
/// naming one of its parent domains.
fn covered(host: &str, pattern: &str) -> bool {
    let (host, pattern) = (host.to_ascii_lowercase(), pattern.to_ascii_lowercase());
    match pattern.strip_prefix('*') {
        Some(suffix) if suffix.starts_with('.') => host.ends_with(suffix),
        _ => host == pattern,
    }
}

/// The hosts each of `layers` needs that `allowlist` (the daemon's CONNECT allowlist, `["*"]` =
/// open) does not admit, per layer id; empty when it admits them all.
pub(crate) fn uncovered<'a>(
    layers: &[&'a Layer],
    allowlist: &[String],
) -> Vec<(&'a str, Vec<&'a str>)> {
    if allowlist.iter().any(|h| h == "*") {
        return Vec::new();
    }
    layers
        .iter()
        .filter_map(|l| {
            let missing: Vec<&str> = l
                .hosts
                .iter()
                .copied()
                .filter(|h| !allowlist.iter().any(|p| covered(h, p)))
                .collect();
            (!missing.is_empty()).then_some((l.id, missing))
        })
        .collect()
}

/// The note a unit gets when the daemon's allowlist does not admit its layers' hosts.
pub(crate) fn egress_note(layers: &[&Layer], allowlist: &[String]) -> Option<String> {
    let gaps = uncovered(layers, allowlist);
    if gaps.is_empty() {
        return None;
    }
    let each: Vec<String> = gaps
        .iter()
        .map(|(id, hosts)| format!("{id}: {}", hosts.join(", ")))
        .collect();
    Some(format!(
        "[engine — seat toolchain] this repository's toolchains reach hosts the seat egress \
         allowlist ({}) does not name, so downloads from them are refused: {}. Add them to it \
         to let the seat fetch dependencies.",
        crate::seat_gateway::EGRESS_HOSTS_ENV,
        each.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wicked-seat-layers-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ids(layers: &[&Layer]) -> Vec<&'static str> {
        layers.iter().map(|l| l.id).collect()
    }

    fn by_id(id: &str) -> &'static Layer {
        CATALOG.iter().find(|l| l.id == id).unwrap()
    }

    /// Markers at the root and one directory down are found; a hidden directory, dependency
    /// output, a deeper directory, a linked marker and a directory named like a marker are not.
    #[test]
    fn detection_reads_marker_files_at_the_root_and_one_level_down() {
        let t = tree("detect");
        assert!(detect(&t).is_empty(), "an empty tree needs no layer");
        std::fs::write(t.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(t.join("api")).unwrap();
        std::fs::write(t.join("api/go.mod"), "module x\n").unwrap();
        std::fs::create_dir_all(t.join("tools")).unwrap();
        std::fs::write(t.join("tools/requirements-dev.txt"), "").unwrap();
        // Not markers: hidden, dependency output, two levels down, a directory.
        std::fs::create_dir_all(t.join(".github")).unwrap();
        std::fs::write(t.join(".github/pom.xml"), "").unwrap();
        std::fs::create_dir_all(t.join("node_modules/x")).unwrap();
        std::fs::write(t.join("node_modules/package.json"), "{}").unwrap();
        std::fs::create_dir_all(t.join("a/b")).unwrap();
        std::fs::write(t.join("a/b/build.gradle"), "").unwrap();
        std::fs::create_dir_all(t.join("package.json")).unwrap();
        // Live links, to a real marker and to a directory holding one, are not followed.
        #[cfg(unix)]
        let elsewhere = {
            let elsewhere = tree("detect-link-target");
            std::fs::write(elsewhere.join("pom.xml"), "").unwrap();
            std::fs::write(elsewhere.join("go.work"), "").unwrap();
            std::os::unix::fs::symlink(elsewhere.join("pom.xml"), t.join("pom.xml")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, t.join("linked")).unwrap();
            assert!(t.join("pom.xml").is_file() && t.join("linked/pom.xml").is_file());
            elsewhere
        };
        assert_eq!(ids(&detect(&t)), vec!["go", "python", "rust"]);
        #[cfg(unix)]
        std::fs::remove_dir_all(&elsewhere).unwrap();
        std::fs::remove_dir_all(&t).unwrap();
    }

    /// Only the first [`MAX_SUBDIRS`] subdirectories by name are read, whatever order the file
    /// system lists them in, so one tree always detects the same layers.
    #[test]
    fn detection_reads_a_bounded_sorted_set_of_subdirectories() {
        let t = tree("bounded");
        for i in 0..MAX_SUBDIRS {
            std::fs::create_dir_all(t.join(format!("d{i:04}"))).unwrap();
        }
        std::fs::write(t.join("d0000/go.mod"), "").unwrap();
        // Past the bound by name: never read.
        std::fs::create_dir_all(t.join("zz")).unwrap();
        std::fs::write(t.join("zz/pom.xml"), "").unwrap();
        for _ in 0..3 {
            assert_eq!(ids(&detect(&t)), vec!["go"]);
        }
        std::fs::remove_dir_all(&t).unwrap();
    }

    /// (coord R2 F6 on core#1043) The cap keeps the first entries BY NAME, whatever order the
    /// filesystem lists them in.
    #[test]
    fn the_entry_cap_keeps_the_first_names_whatever_the_listing_order() {
        let mut listed: Vec<(String, bool, bool)> = (0..MAX_ENTRIES + 4)
            .map(|i| (format!("z{i:05}"), true, false))
            .collect();
        listed.push(("go.mod".to_string(), true, false));
        let kept = first_by_name(listed.into_iter());
        assert_eq!(kept.len(), MAX_ENTRIES);
        assert_eq!(kept[0].0, "go.mod");
        assert_eq!(kept[MAX_ENTRIES - 1].0, format!("z{:05}", MAX_ENTRIES - 2));
    }

    #[test]
    fn each_catalog_marker_detects_its_layer() {
        for layer in CATALOG {
            for marker in layer.markers {
                let t = tree("marker");
                std::fs::write(t.join(marker.replace('*', "")), "").unwrap();
                assert_eq!(ids(&detect(&t)), vec![layer.id], "{marker}");
                std::fs::remove_dir_all(&t).unwrap();
            }
        }
    }

    #[test]
    fn layers_off_turns_detection_off() {
        let t = tree("off");
        std::fs::write(t.join("go.mod"), "").unwrap();
        assert_eq!(ids(&for_tree_with(&t, None)), vec!["go"]);
        assert_eq!(ids(&for_tree_with(&t, Some("on"))), vec!["go"]);
        assert!(for_tree_with(&t, Some(" OFF ")).is_empty());
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn a_layer_list_parses_in_catalog_order_and_refuses_an_unknown_id() {
        assert_eq!(ids(&parse("python, go,go").unwrap()), vec!["go", "python"]);
        assert!(parse("none").unwrap().is_empty());
        assert!(parse("").unwrap().is_empty());
        let e = parse("go,cobol").unwrap_err();
        assert!(
            e.contains("`cobol`") && e.contains("go, java, node, python, rust"),
            "{e}"
        );
    }

    /// Only go, java and python build; the default base has rust and node.
    #[test]
    fn only_the_toolchains_the_base_lacks_build() {
        let all: Vec<&Layer> = CATALOG.iter().collect();
        assert_eq!(ids(&built(&all)), vec!["go", "java", "python"]);
        assert!(tag_parts(&[by_id("rust"), by_id("node")]).is_empty());
        let ab = tag_parts(&[by_id("python"), by_id("go")]);
        let ba = tag_parts(&[by_id("go"), by_id("python")]);
        assert_eq!(ab, ba, "the tag does not depend on detection order");
        assert_eq!(ab.len(), 2);
    }

    /// The layers sit before `ARG CLI` (shared by every CLI's image), in catalog order, labelled;
    /// with nothing to build the recipe is unchanged byte for byte.
    #[test]
    fn composition_inserts_the_layers_before_the_cli_args() {
        let df = "FROM x\nRUN a\nARG CLI\nRUN b\n";
        assert_eq!(compose(df, &[]), df);
        assert_eq!(compose(df, &[by_id("rust"), by_id("node")]), df);
        let out = compose(df, &[by_id("python"), by_id("go")]);
        let go = out.find("golang-go").unwrap();
        let uv = out.find("astral-sh/uv").unwrap();
        let label = out.find("LABEL wicked.seat.layers=\"go,python\"").unwrap();
        let cli = out.find("ARG CLI").unwrap();
        assert!(out.starts_with("FROM x\nRUN a\n"), "{out}");
        assert!(go < uv && uv < label && label < cli, "{out}");
        assert!(out.ends_with("ARG CLI\nRUN b\n"), "{out}");
    }

    /// Every toolchain is checked on Debian's default PATH with no image env, the only
    /// environment the seat floor hands the CLI.
    #[test]
    fn every_recipe_proves_its_tool_on_the_default_path_alone() {
        for l in CATALOG.iter().filter(|l| l.builds()) {
            assert!(
                l.recipe.contains(concat!(
                    "env -i PATH=",
                    default_path!(),
                    " HOME=/nonexistent"
                )),
                "{}",
                l.id
            );
            assert!(!l.recipe.contains("\nENV "), "{}: no image env", l.id);
            assert!(l.recipe.ends_with('\n'), "{}", l.id);
        }
        // The uv tarball is checked before it is unpacked, per architecture.
        let py = by_id("python").recipe;
        assert!(py.find("sha256sum -c -").unwrap() < py.find("tar -xzf").unwrap());
        assert_eq!(py.matches("sum=").count(), 2);
    }

    /// CI builds the layers on the seat image from this fixture and runs each toolchain offline
    /// on the default PATH (ci.yml `seat-toolchain`); it is the engine's own block, byte for byte.
    /// `WICKED_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_ci_layer_fixture_is_the_block_the_engine_composes() {
        let want = layer_block(&built(&parse("go,java,python").unwrap()));
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/seat-layers/go-java-python.Dockerfile");
        if std::env::var_os("WICKED_WRITE_FIXTURES").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
        }
        let got = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(got, want, "regenerate with WICKED_WRITE_FIXTURES=1");
        assert!(compose("ARG CLI\n", &parse("go,java,python").unwrap()).starts_with(&want));
    }

    #[test]
    fn the_egress_note_names_only_hosts_the_allowlist_lacks() {
        let go = [by_id("go")];
        let open = vec!["*".to_string()];
        assert!(egress_note(&go, &open).is_none());
        let some = vec![
            "*.golang.org".to_string(),
            "STORAGE.googleapis.com".to_string(),
        ];
        assert!(
            egress_note(&go, &some).is_none(),
            "a suffix wildcard covers"
        );
        let narrow = vec!["proxy.golang.org".to_string()];
        let note = egress_note(&go, &narrow).unwrap();
        assert!(
            note.contains("go: sum.golang.org, storage.googleapis.com"),
            "{note}"
        );
        assert!(note.contains("WICKED_SEAT_EGRESS_HOSTS"), "{note}");
        // `*.golang.org` admits a subdomain, never the apex or a look-alike.
        assert!(!covered("golang.org", "*.golang.org"));
        assert!(!covered("evilgolang.org", "*.golang.org"));
        // No egress at all: every host is missing.
        let none: Vec<String> = Vec::new();
        assert_eq!(uncovered(&[by_id("rust")], &none)[0].1.len(), 3);
    }

    /// (core#1043 part 2) A repo turns a detected layer off; the rest still compose, and the
    /// daemon's `off` still wins.
    #[test]
    fn a_repo_turns_a_detected_layer_off() {
        let t = tree("repo-off");
        std::fs::write(t.join("Cargo.toml"), "").unwrap();
        std::fs::write(t.join("go.mod"), "module x\n").unwrap();
        assert_eq!(ids(&for_tree_except(&t, None, &[])), ["go", "rust"]);
        assert_eq!(
            ids(&for_tree_except(&t, None, &["go".to_string()])),
            ["rust"]
        );
        assert_eq!(
            ids(&for_tree_except(&t, None, &["java".to_string()])),
            ["go", "rust"],
            "a layer the tree does not need changes nothing"
        );
        assert!(for_tree_except(&t, Some("off"), &[]).is_empty());
    }

    /// (core#1933, round 1 codex/pi P2) The repo card's detection is reused until the TTL passes,
    /// then runs again and shows a marker added or removed meanwhile.
    #[test]
    fn the_cards_detection_is_reused_until_the_ttl_passes() {
        let t = tree("ttl");
        std::fs::write(t.join("Cargo.toml"), "").unwrap();
        let reads = || DIR_READS.with(|n| n.get());
        let t0 = std::time::Instant::now();
        assert_eq!(ids(&detect_cached_at(&t, t0)), ["rust"]);
        std::fs::write(t.join("go.mod"), "module x\n").unwrap();
        let before = reads();
        let just_inside = t0 + DETECT_TTL - std::time::Duration::from_millis(1);
        assert_eq!(ids(&detect_cached_at(&t, just_inside)), ["rust"], "reused");
        assert_eq!(reads(), before, "a reuse lists no directory");
        let t1 = t0 + DETECT_TTL;
        assert_eq!(ids(&detect_cached_at(&t, t1)), ["go", "rust"], "added");
        assert!(reads() > before, "the TTL passed: detected again");
        std::fs::remove_file(t.join("Cargo.toml")).unwrap();
        assert_eq!(
            ids(&detect_cached_at(&t, t1 + DETECT_TTL)),
            ["go"],
            "removed"
        );
    }

    /// (core#2047) Any derivation the repo list reuses goes through the one cache: derived once
    /// within the TTL, again once it passes, and kept per checkout root.
    #[test]
    fn a_checkout_derivation_is_reused_until_the_ttl_passes_2047() {
        let cache: CheckoutCache<Option<u32>> = Default::default();
        let derived = std::cell::Cell::new(0);
        let derive = |v: Option<u32>| {
            derived.set(derived.get() + 1);
            v
        };
        let a = Path::new("/checkout/a");
        let t0 = std::time::Instant::now();
        assert_eq!(cached_at(&cache, a, t0, || derive(Some(1))), Some(1));
        let just_inside = t0 + DETECT_TTL - std::time::Duration::from_millis(1);
        assert_eq!(cached_at(&cache, a, just_inside, || derive(None)), Some(1));
        assert_eq!(derived.get(), 1, "reused within the TTL");
        assert_eq!(cached_at(&cache, a, t0 + DETECT_TTL, || derive(None)), None);
        assert_eq!(derived.get(), 2, "derived again once it passed");
        let b = Path::new("/checkout/b");
        assert_eq!(cached_at(&cache, b, t0, || derive(Some(2))), Some(2));
        assert_eq!(derived.get(), 3, "another root has its own entry");
    }

    /// core#2047 (DES-2047b): a derivation is fresh for the TTL from when it LANDS. A scan that
    /// took longer than the TTL is reused by the read right after it, never derived again at once.
    #[test]
    fn a_slow_derivation_is_fresh_for_the_ttl_after_it_lands_2047() {
        let cache: CheckoutCache<u32> = Default::default();
        let t0 = std::time::Instant::now();
        let clock = std::cell::Cell::new(t0);
        let now = || clock.get();
        let derived = std::cell::Cell::new(0);
        let a = Path::new("/checkout/slow");
        let first = cached_at_clocked(&cache, a, &now, || {
            derived.set(derived.get() + 1);
            // The scan took longer than the TTL.
            clock.set(t0 + DETECT_TTL + std::time::Duration::from_secs(1));
            1
        });
        assert_eq!(first, 1);
        let next = cached_at_clocked(&cache, a, &now, || {
            derived.set(derived.get() + 1);
            2
        });
        assert_eq!(next, 1, "the read right after it landed reuses it");
        assert_eq!(derived.get(), 1, "derived once");
    }

    /// (core#1043 part 2) The repo card's rows: every detected layer, and what turned one off.
    #[test]
    fn a_repos_layer_rows_say_what_turned_a_layer_off() {
        let t = tree("repo-rows");
        std::fs::write(t.join("Cargo.toml"), "").unwrap();
        std::fs::write(t.join("go.mod"), "module x\n").unwrap();
        let view = |off: &[&str], setting: Option<&str>, operator_base: bool| {
            let off: Vec<String> = off.iter().map(|s| s.to_string()).collect();
            rows(&t, &off, setting, operator_base, false)
                .into_iter()
                .map(|r| (r.id, r.enabled, r.off_by))
                .collect::<Vec<_>>()
        };
        let on = |id: &str| (id.to_string(), true, None);
        let off = |id: &str, by: &str| (id.to_string(), false, Some(by.to_string()));
        assert_eq!(view(&[], None, false), [on("go"), on("rust")]);
        assert_eq!(view(&["go"], None, false), [off("go", "repo"), on("rust")]);
        assert_eq!(
            view(&["go"], Some("off"), false),
            [off("go", "daemon"), off("rust", "daemon")]
        );
        assert_eq!(
            view(&[], None, true),
            [off("go", "operator_base"), off("rust", "operator_base")]
        );
        // (core#1043 part 3) The repo's own base is its toolchain, over the daemon's.
        let rows_of = |operator_base: bool| {
            rows(&t, &["go".to_string()], Some("off"), operator_base, true)
                .into_iter()
                .map(|r| (r.id, r.enabled, r.off_by))
                .collect::<Vec<_>>()
        };
        for operator_base in [false, true] {
            assert_eq!(
                rows_of(operator_base),
                [off("go", "repo_base"), off("rust", "repo_base")]
            );
        }
    }
}
