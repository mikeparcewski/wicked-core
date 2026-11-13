//! Exercise the production image-build path without a container daemon or the root unit-test
//! binary. The runtime records builds and models images surviving between CLI invocations.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// SAFETY: runs before main on one thread, before any test can emit an event.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn build(root: &Path, base: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wicked-core"));
    cmd.args(["seat-image", "build", "claude"])
        .env("WICKED_CONTAINER_RUNTIME", root.join("docker"))
        .env("SEAT_TEST_ROOT", root)
        .env_remove("WICKED_SEAT_BASE_IMAGE")
        .env_remove("WICKED_SEAT_BASE_DOCKERFILE")
        .env_remove("WICKED_SEAT_IMAGE_CLAUDE");
    if let Some(base) = base {
        cmd.env("WICKED_SEAT_BASE_IMAGE", base);
    }
    let out = cmd.output().expect("seat-image runs");
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cli_builds_the_checks_base_first_reuses_it_and_recovers_after_gc() {
    let root = std::env::temp_dir().join(format!("wicked-seat-toolchain-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let runtime = root.join("docker");
    std::fs::write(
        &runtime,
        r##"#!/bin/sh
set -eu
cd "$SEAT_TEST_ROOT"
case "$1" in
  version) echo 29.5.3 ;;
  image)
    test "$2" = inspect
    for tag do :; done
    test -f "$tag"
    printf 'sha256:%064d\n' 0
    ;;
  build)
    tag=
    while [ "$#" -gt 0 ]; do
      if [ "$1" = -t ]; then tag=$2; fi
      shift
    done
    test -n "$tag"
    cat > "$tag"
    echo "$tag" >> builds
    ;;
  *) exit 2 ;;
esac
"##,
    )
    .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();

    let first = build(&root, None);
    let builds = std::fs::read_to_string(root.join("builds")).unwrap();
    let tags: Vec<_> = builds.lines().collect();
    assert_eq!(tags.len(), 3, "launcher, checks base, seat: {builds}");
    assert!(tags[0].starts_with("wicked-floor:"), "{builds}");
    assert!(tags[1].starts_with("wicked-checks-floor:"), "{builds}");
    assert!(tags[2].starts_with("wicked-seat-claude:"), "{builds}");
    assert_eq!(
        std::fs::read_to_string(root.join(tags[1])).unwrap(),
        include_str!("../containers/checks/node22-rust.Dockerfile")
    );
    let recipe = std::fs::read_to_string(root.join(tags[2])).unwrap();
    assert!(recipe.contains("FROM ${BASE} AS wicked_toolchain\n"));
    assert!(recipe.contains("FROM wicked_toolchain\n"));
    assert_eq!(recipe.matches("https://sh.rustup.rs").count(), 1);

    // An explicit checks base must produce the same seat tag as the default base.
    assert_eq!(first, build(&root, Some(tags[1])));
    assert_eq!(first, build(&root, None));
    let reused = std::fs::read_to_string(root.join("builds")).unwrap();
    assert_eq!(reused.lines().count(), 5, "{reused}");
    assert_eq!(reused.lines().filter(|t| *t == tags[1]).count(), 1);

    // Retain the seat image, as a GC may remove only its base. The next build must repair it.
    std::fs::remove_file(root.join(tags[1])).unwrap();
    assert_eq!(first, build(&root, None));
    let repaired = std::fs::read_to_string(root.join("builds")).unwrap();
    assert_eq!(
        repaired.lines().skip(5).collect::<Vec<_>>(),
        [tags[1], tags[2]],
        "{repaired}"
    );
    std::fs::remove_dir_all(root).unwrap();
}
