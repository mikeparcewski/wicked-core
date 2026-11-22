use super::checkout_bytes;
use std::path::Path;

#[test]
fn refuses_unsafe_batch_inputs_and_skips_empty_batches() {
    let source = Path::new("missing-checkout-bytes-test-git-dir");
    for (oid, path, error) in [
        ("abcdef", "line\nbreak", "unsafe path"),
        ("abcdef", "line\rbreak", "unsafe path"),
        ("abcdef", "", "unsafe path"),
        ("not-hex", "file", "invalid blob oid"),
    ] {
        let result = checkout_bytes(
            source,
            "HEAD",
            vec![(oid.to_string(), path.to_string())],
            true,
            |_, _| panic!("an invalid batch must not call each"),
        );
        assert!(result.unwrap_err().contains(error));
    }
    assert!(checkout_bytes(source, "HEAD", Vec::new(), true, |_, _| {
        panic!("an empty batch must not call each")
    })
    .is_ok());
}
