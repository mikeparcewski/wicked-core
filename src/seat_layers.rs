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

/// The ids of the layers whose markers name a regular file directly in `dir`.
fn scan(dir: &Path, found: &mut BTreeSet<&'static str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow a link: a linked marker is not a marker.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        for layer in CATALOG {
            if layer.markers.iter().any(|m| marker_matches(m, name)) {
                found.insert(layer.id);
            }
        }
    }
}

/// The layers `tree` needs: marker files at its root and in its immediate subdirectories (not
/// hidden, not dependency or build output, at most [`MAX_SUBDIRS`]), in catalog order.
pub(crate) fn detect(tree: &Path) -> Vec<&'static Layer> {
    let _ = (tree, MAX_SUBDIRS, SKIP_DIRS, scan as fn(&Path, &mut BTreeSet<&'static str>));
    Vec::new()
}

/// [`detect`] unless `setting` ([`LAYERS_ENV`]'s value) is `off`.
pub(crate) fn for_tree_with(tree: &Path, setting: Option<&str>) -> Vec<&'static Layer> {
    if setting.is_some_and(|v| v.trim().eq_ignore_ascii_case("off")) {
        return Vec::new();
    }
    detect(tree)
}

/// [`for_tree_with`] this daemon's [`LAYERS_ENV`].
pub(crate) fn for_tree(tree: &Path) -> Vec<&'static Layer> {
    for_tree_with(tree, std::env::var(LAYERS_ENV).ok().as_deref())
}

/// The layers a comma-separated id list names (`seat-image --layers`), in catalog order; empty or
/// `none` names none. An id the catalog does not have is refused by name.
pub(crate) fn parse(list: &str) -> Result<Vec<&'static Layer>, String> {
    let _ = list;
    Ok(Vec::new())
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
    let _ = layers;
    Vec::new()
}

/// `dockerfile` (a seat recipe) with the built layers' recipes inserted just before its
/// `ARG CLI` line, labelled `wicked.seat.layers`. Unchanged when no layer builds.
pub(crate) fn compose(dockerfile: &str, layers: &[&Layer]) -> String {
    let _ = layers;
    dockerfile.to_string()
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
    let _ = (layers, allowlist, covered as fn(&str, &str) -> bool);
    Vec::new()
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
        #[cfg(unix)]
        {
            let elsewhere = tree("detect-link-target");
            std::fs::write(elsewhere.join("pom.xml"), "").unwrap();
            std::os::unix::fs::symlink(elsewhere.join("pom.xml"), t.join("pom.xml")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, t.join("linked")).unwrap();
            std::fs::remove_dir_all(&elsewhere).ok();
        }
        assert_eq!(ids(&detect(&t)), vec!["go", "python", "rust"]);
        std::fs::remove_dir_all(&t).unwrap();
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
}
