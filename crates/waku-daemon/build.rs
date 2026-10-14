use std::path::Path;

fn visit(path: &Path, hash: &mut u64) {
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        let mut paths: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        for path in paths {
            visit(&path, hash);
        }
    } else {
        for byte in path
            .to_string_lossy()
            .bytes()
            .chain(std::fs::read(path).unwrap())
        {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
}

fn main() {
    let mut hash = 0xcbf29ce484222325;
    for source in [
        "src",
        "build.rs",
        "Cargo.toml",
        "../waku-core/src",
        "../waku-protocol/src",
        "../waku-client/src",
        "../../Cargo.lock",
    ] {
        visit(Path::new(source), &mut hash);
    }
    println!("cargo:rustc-env=WAKU_DAEMON_BUILD_ID=daemon-{hash:016x}");
}
