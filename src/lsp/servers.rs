//! The language servers reviewr runs: one table row each, picked by file extension.

use std::path::Path;
use std::process::Command;

use crate::config::PluginConfig;

/// One language server: how to find it, run it, and which files it answers for.
#[derive(Debug)]
pub struct Server {
    /// The program's default name, said in status lines.
    pub name: &'static str,
    extensions: &'static [&'static str],
    /// The LSP `languageId` of a file by its extension.
    language_id: fn(&str) -> &'static str,
    /// The config key's program, when set.
    program: fn(&PluginConfig) -> Option<&str>,
    args: fn(&PluginConfig) -> Vec<String>,
    /// The environment the server runs in, adjusted from reviewr's own.
    env: fn(&mut Command),
    pub install_hint: &'static str,
    /// By `languageId`, the words no definition stands behind, never offered on a line.
    keywords: fn(&str) -> &'static str,
    /// What the project lacks for good answers, judged from the worktree's root.
    setup: fn(&Path) -> Option<&'static str>,
}

/// C's keywords, C23 included.
const C_KEYWORDS: &str = "alignas alignof auto bool break case char const constexpr continue \
    default do double else enum extern false float for goto if inline int long nullptr register \
    restrict return short signed sizeof static static_assert struct switch thread_local true \
    typedef typeof typeof_unqual union unsigned void volatile while _Alignas _Alignof _Atomic \
    _BitInt _Bool _Complex _Generic _Imaginary _Noreturn _Static_assert _Thread_local";

/// C++'s keywords but `auto`, `decltype` and `this`, which clangd answers with the type behind.
const CPP_KEYWORDS: &str = "alignas alignof and and_eq asm bitand bitor bool break case catch \
    char char8_t char16_t char32_t class co_await co_return co_yield compl concept const consteval \
    constexpr constinit const_cast continue default delete do double dynamic_cast else enum \
    explicit export extern false float for friend goto if inline int long mutable namespace new \
    noexcept not not_eq nullptr operator or or_eq private protected public register \
    reinterpret_cast requires return short signed sizeof static static_assert static_cast struct \
    switch template thread_local throw true try typedef typeid typename union unsigned using \
    virtual void volatile wchar_t while xor xor_eq";

pub const SERVERS: &[Server] = &[
    Server {
        name: "clangd",
        extensions: &[
            "c", "h", "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "inl", "ipp", "tpp",
        ],
        language_id: |ext| if ext == "c" { "c" } else { "cpp" },
        program: PluginConfig::clangd,
        // No compile-commands dir: clangd finds the project's own, `.clangd` included.
        args: |config| vec![format!("--background-index={}", config.clangd_index())],
        env: |_| {},
        install_hint: "install clangd from your LLVM or system packages",
        keywords: |language| if language == "c" { C_KEYWORDS } else { CPP_KEYWORDS },
        // Where clangd looks first; anything deeper, a project names in its `.clangd`.
        setup: |root| {
            let markers = [
                "compile_commands.json",
                "build/compile_commands.json",
                "compile_flags.txt",
                ".clangd",
            ];
            (!markers.iter().any(|m| root.join(m).exists()))
                .then_some("no compile_commands.json: build once, or name it in .clangd")
        },
    },
    Server {
        name: "gopls",
        extensions: &["go"],
        language_id: |_| "go",
        program: PluginConfig::gopls,
        args: |_| Vec::new(),
        env: |cmd| {
            if let Some(flags) = std::env::var_os("GOFLAGS") {
                cmd.env("GOFLAGS", without_mod_flag(&flags.to_string_lossy()));
            }
        },
        install_hint: "run `go install golang.org/x/tools/gopls@latest`",
        // Predeclared names (`int`, `len`, `nil`) stay: gopls answers them from `builtin`.
        keywords: |_| {
            "break case chan const continue default defer else fallthrough for func go goto if \
             import interface map package range return select struct switch type var"
        },
        setup: |_| None,
    },
];

/// The server for a repo path, by its extension.
pub fn for_path(path: &str) -> Option<(usize, &'static Server)> {
    let ext = path.rsplit_once('.').map(|(_, ext)| ext)?;
    SERVERS
        .iter()
        .enumerate()
        .find(|(_, s)| s.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

impl Server {
    pub fn language_id(&self, path: &str) -> &'static str {
        let ext = path.rsplit_once('.').map_or("", |(_, ext)| ext);
        (self.language_id)(&ext.to_ascii_lowercase())
    }

    /// Whether `name` is a keyword of `path`'s language.
    pub fn is_keyword(&self, path: &str, name: &str) -> bool {
        (self.keywords)(self.language_id(path)).split_whitespace().any(|k| k == name)
    }

    /// The program and arguments this config runs, which a reconfigure compares.
    pub fn argv(&self, config: &PluginConfig) -> Vec<String> {
        let program = (self.program)(config).unwrap_or(self.name).to_string();
        std::iter::once(program).chain((self.args)(config)).collect()
    }

    /// What the project at `root` lacks for this server to answer well.
    pub fn missing_setup(&self, root: &Path) -> Option<&'static str> {
        (self.setup)(root)
    }

    /// The server's command, or `None` when its program is nowhere to be found.
    pub fn command(&self, config: &PluginConfig) -> Option<Command> {
        let argv = self.argv(config);
        let mut cmd = crate::proc::user_command(&argv[0])?;
        cmd.args(&argv[1..]);
        (self.env)(&mut cmd);
        Some(cmd)
    }
}

/// `GOFLAGS` without any `-mod=`, so gopls's `go` runs never rewrite `go.mod` or `go.sum`.
fn without_mod_flag(flags: &str) -> String {
    let kept = flags.split_whitespace().filter(|f| !f.trim_start_matches('-').starts_with("mod="));
    kept.collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{SERVERS, for_path, without_mod_flag};
    use crate::config::PluginConfig;

    fn config(text: &str) -> PluginConfig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), text).unwrap();
        crate::config::plugin_config_in(dir.path()).unwrap()
    }

    #[test]
    fn a_file_finds_its_server_by_extension() {
        assert_eq!(for_path("src/a.cpp").map(|(_, s)| s.name), Some("clangd"));
        assert_eq!(for_path("include/A.HPP").map(|(_, s)| s.name), Some("clangd"));
        assert_eq!(for_path("cmd/main.go").map(|(_, s)| s.name), Some("gopls"));
        assert!(for_path("README.md").is_none());
        assert!(for_path("Makefile").is_none());
        let (_, gopls) = for_path("main.go").unwrap();
        assert!(gopls.is_keyword("main.go", "return") && !gopls.is_keyword("main.go", "len"));
        let (_, clangd) = for_path("a.c").unwrap();
        assert!(clangd.is_keyword("a.c", "auto") && clangd.is_keyword("a.c", "_Generic"));
        assert!(clangd.is_keyword("a.cpp", "return") && !clangd.is_keyword("a.cpp", "auto"));
        assert_eq!((clangd.language_id("a.c"), clangd.language_id("a.h")), ("c", "cpp"));
    }

    #[test]
    fn clangd_keeps_its_index_only_when_asked_and_never_gets_a_compile_commands_dir() {
        let clangd = &SERVERS[0];
        assert_eq!(clangd.argv(&PluginConfig::default()), ["clangd", "--background-index=false"]);
        let indexed = config("clangd = \"/opt/llvm/bin/clangd\"\nclangd_index = true\n");
        assert_eq!(clangd.argv(&indexed), ["/opt/llvm/bin/clangd", "--background-index=true"]);
    }

    #[test]
    fn gopls_runs_without_a_mod_flag_and_keeps_the_rest() {
        assert_eq!(without_mod_flag("-mod=mod -tags=integration"), "-tags=integration");
        assert_eq!(without_mod_flag("--mod=vendor -race"), "-race");
        assert_eq!(without_mod_flag("-modcacherw"), "-modcacherw");
        assert_eq!(without_mod_flag("-mod=readonly"), "");
    }

    #[test]
    fn clangd_says_when_the_project_names_no_compile_database() {
        let root = tempfile::tempdir().unwrap();
        assert!(SERVERS[0].missing_setup(root.path()).is_some());
        for marker in
            ["compile_commands.json", "build/compile_commands.json", "compile_flags.txt", ".clangd"]
        {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("build")).unwrap();
            std::fs::write(root.path().join(marker), "").unwrap();
            assert_eq!(SERVERS[0].missing_setup(root.path()), None, "{marker}");
        }
        assert_eq!(SERVERS[1].missing_setup(root.path()), None, "gopls needs nothing named");
    }

    #[test]
    fn a_missing_program_finds_no_command() {
        let missing = config("gopls = \"/nonexistent/gopls\"\n");
        assert!(SERVERS[1].command(&missing).is_none());
    }
}
