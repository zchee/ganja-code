//! What a language server is told a file is written in.
//!
//! Spec: upstream `packages/opencode/src/lsp/language.ts` (the table) and
//! `packages/opencode/src/lsp/client.ts:560` (the lookup).
//!
//! The table is ported entry for entry, including the two shapes that cannot
//! be reached through the lookup that reads it — see [`language_id`]. Keeping
//! them means a later port of a filename-based lookup finds the answers
//! already written down, and means this file can be diffed against upstream's
//! without explaining absences.

/// Extension (with its leading dot) to the LSP `languageId` for it.
///
/// Sorted by nothing in particular: this is upstream's order, so the two files
/// diff against each other.
const LANGUAGE_EXTENSIONS: &[(&str, &str)] = &[
    (".abap", "abap"),
    (".bat", "bat"),
    (".bib", "bibtex"),
    (".bibtex", "bibtex"),
    (".clj", "clojure"),
    (".cljs", "clojure"),
    (".cljc", "clojure"),
    (".edn", "clojure"),
    (".coffee", "coffeescript"),
    (".c", "c"),
    (".cpp", "cpp"),
    (".cxx", "cpp"),
    (".cc", "cpp"),
    (".c++", "cpp"),
    (".cs", "csharp"),
    (".csx", "csharp"),
    (".css", "css"),
    (".d", "d"),
    (".pas", "pascal"),
    (".pascal", "pascal"),
    (".diff", "diff"),
    (".patch", "diff"),
    (".dart", "dart"),
    (".dockerfile", "dockerfile"),
    (".ex", "elixir"),
    (".exs", "elixir"),
    (".erl", "erlang"),
    (".ets", "typescript"),
    (".hrl", "erlang"),
    (".fs", "fsharp"),
    (".fsi", "fsharp"),
    (".fsx", "fsharp"),
    (".fsscript", "fsharp"),
    (".gitcommit", "git-commit"),
    (".gitrebase", "git-rebase"),
    (".go", "go"),
    (".groovy", "groovy"),
    (".gleam", "gleam"),
    (".hbs", "handlebars"),
    (".handlebars", "handlebars"),
    (".hs", "haskell"),
    (".lhs", "haskell"),
    (".html", "html"),
    (".htm", "html"),
    (".ini", "ini"),
    (".java", "java"),
    (".jl", "julia"),
    (".js", "javascript"),
    (".kt", "kotlin"),
    (".kts", "kotlin"),
    (".jsx", "javascriptreact"),
    (".json", "json"),
    (".tex", "latex"),
    (".latex", "latex"),
    (".less", "less"),
    (".lua", "lua"),
    (".makefile", "makefile"),
    // Upstream's one key with no leading dot. `extname` never yields it, so
    // upstream cannot reach it either; carried so the two tables match.
    ("makefile", "makefile"),
    (".md", "markdown"),
    (".markdown", "markdown"),
    (".m", "objective-c"),
    (".mm", "objective-cpp"),
    (".pl", "perl"),
    (".pm", "perl"),
    (".pm6", "perl6"),
    (".php", "php"),
    (".ps1", "powershell"),
    (".psm1", "powershell"),
    (".pug", "jade"),
    (".jade", "jade"),
    (".py", "python"),
    (".r", "r"),
    (".cshtml", "razor"),
    (".razor", "razor"),
    (".rb", "ruby"),
    (".rake", "ruby"),
    (".gemspec", "ruby"),
    (".ru", "ruby"),
    (".erb", "erb"),
    // The three double-extension keys below share the unreachability of
    // `makefile`: a last-dot lookup on `page.html.erb` asks for `.erb`.
    (".html.erb", "erb"),
    (".js.erb", "erb"),
    (".css.erb", "erb"),
    (".json.erb", "erb"),
    (".rs", "rust"),
    (".scss", "scss"),
    (".sass", "sass"),
    (".scala", "scala"),
    (".shader", "shaderlab"),
    (".sh", "shellscript"),
    (".bash", "shellscript"),
    (".zsh", "shellscript"),
    (".ksh", "shellscript"),
    (".sql", "sql"),
    (".svelte", "svelte"),
    (".swift", "swift"),
    (".ts", "typescript"),
    (".tsx", "typescriptreact"),
    (".mts", "typescript"),
    (".cts", "typescript"),
    (".mtsx", "typescriptreact"),
    (".ctsx", "typescriptreact"),
    (".xml", "xml"),
    (".xsl", "xsl"),
    (".yaml", "yaml"),
    (".yml", "yaml"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
    (".vue", "vue"),
    (".zig", "zig"),
    (".zon", "zig"),
    (".astro", "astro"),
    (".ml", "ocaml"),
    (".mli", "ocaml"),
    (".tf", "terraform"),
    (".tfvars", "terraform-vars"),
    (".hcl", "hcl"),
    (".nix", "nix"),
    (".typ", "typst"),
    (".typc", "typst"),
];

/// What a file with no entry in the table is called.
const PLAINTEXT: &str = "plaintext";

/// The `languageId` for `path`, or `"plaintext"` when nothing claims it.
///
/// The extension is the last dot onward and is matched case-sensitively, which
/// is what upstream's `extname` + object lookup does. A file whose only dot is
/// its first character — `.bashrc` — has no extension by that rule, here as
/// there.
#[must_use]
pub fn language_id(path: &std::path::Path) -> &'static str {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return PLAINTEXT;
    };
    let dotted = format!(".{extension}");

    LANGUAGE_EXTENSIONS
        .iter()
        .find(|(candidate, _)| *candidate == dotted)
        .map_or(PLAINTEXT, |(_, id)| *id)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LANGUAGE_EXTENSIONS, language_id};

    #[test]
    fn a_known_extension_names_its_language() {
        let cases = [
            ("/tmp/main.rs", "rust"),
            ("/tmp/main.go", "go"),
            ("/tmp/app.tsx", "typescriptreact"),
            ("/tmp/build.zig", "zig"),
            ("/tmp/notes.md", "markdown"),
            ("/tmp/main.c++", "cpp"),
        ];

        for (path, expected) in cases {
            assert_eq!(language_id(Path::new(path)), expected, "for {path}");
        }
    }

    #[test]
    fn an_unclaimed_extension_is_plaintext() {
        let cases = [
            // No entry anywhere in the table.
            "/tmp/notes.wat",
            // A dotfile has no extension, so there is nothing to look up.
            "/tmp/.bashrc",
            // Neither has a file with no dot at all.
            "/tmp/Makefile",
            // The table is case-sensitive, as upstream's object lookup is.
            "/tmp/MAIN.RS",
        ];

        for path in cases {
            assert_eq!(language_id(Path::new(path)), "plaintext", "for {path}");
        }
    }

    #[test]
    fn a_double_extension_resolves_on_its_last_dot() {
        // `.html.erb` is in the table and still unreachable, because the
        // lookup asks for `.erb` — which is also in the table, and answers.
        assert_eq!(language_id(Path::new("/tmp/show.html.erb")), "erb");
    }

    #[test]
    fn the_table_holds_one_answer_per_extension() {
        let mut seen = std::collections::HashSet::new();
        for (extension, _) in LANGUAGE_EXTENSIONS {
            assert!(
                seen.insert(*extension),
                "{extension} appears twice, so which language it names depends on scan order"
            );
        }
    }
}
