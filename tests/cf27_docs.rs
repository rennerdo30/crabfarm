//! CF-27: the protocol-mismatch example in the docs names the current numbers.

fn read(rel: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn the_protocol_mismatch_example_uses_the_current_version() {
    let now = crabfarm::proto::PROTO_VERSION;
    let want = format!("peer speaks protocol {}, this node speaks {now}", now - 1);
    for f in ["docs/GETTING-STARTED.md", "website/guide.html"] {
        assert!(read(f).contains(&want), "{f} should say: {want}");
    }
}
