//! Fixtures the crate's unit tests share.

/// A fresh repository on `main` with an identity, and a runner for git in it.
pub(crate) fn test_repo() -> (tempfile::TempDir, impl Fn(&[&str]) -> String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().to_path_buf();
    let git = move |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(&repo).args(args).output();
        let out = out.unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    (dir, git)
}

/// The fake language server (`examples/fake_lsp.rs`), built once per test process.
pub(crate) fn fake_lsp() -> &'static std::path::Path {
    static BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let output = std::process::Command::new(env!("CARGO"))
            .args(["build", "--example", "fake_lsp", "--message-format=json"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["target"]["name"] == "fake_lsp")
            .find_map(|message| message["executable"].as_str().map(std::path::PathBuf::from))
            .expect("cargo reports the fake server's executable")
    })
}
