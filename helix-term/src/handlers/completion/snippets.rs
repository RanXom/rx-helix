//! Local snippet completions loaded from the vendored friendly-snippets set.
//!
//! These are the same snippets Neovim's Blink/LuaSnip stack uses. They are
//! expanded through Helix's existing snippet engine, so placeholders are
//! selected, repeated tabstops are mirrors, and `Tab`/`S-Tab` walk them.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use helix_core::completion::CompletionProvider;
use helix_core::movement;
use helix_core::snippets::{Snippet, SnippetRenderCtx};
use helix_core::Range as CoreRange;
use helix_event::TaskHandle;
use helix_lsp::util;
use helix_view::handlers::completion::ResponseContext;
use helix_view::{Document, Editor};
use serde::Deserialize;

use super::request::Trigger;
use super::{CompletionItem, CompletionItems, CompletionResponse};

#[derive(Clone)]
struct SnippetDef {
    prefixes: Vec<String>,
    body: String,
    description: String,
}

#[derive(Deserialize)]
struct RawSnippet {
    #[serde(default)]
    prefix: Option<RawPrefix>,
    #[serde(default)]
    body: Option<RawBody>,
    #[serde(default)]
    description: Option<RawDescription>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawDescription {
    One(String),
    Lines(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawPrefix {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawBody {
    One(String),
    Lines(Vec<String>),
}

#[derive(Deserialize)]
struct SnippetPackage {
    #[serde(default)]
    contributes: Option<Contributes>,
}

#[derive(Deserialize)]
struct Contributes {
    #[serde(default)]
    snippets: Vec<Contribution>,
}

#[derive(Deserialize)]
struct Contribution {
    language: RawPrefix,
    path: String,
}

fn vendor_root() -> Option<PathBuf> {
    let package = helix_loader::runtime_file("snippets/friendly-snippets/package.json");
    let package = if package.is_file() {
        package
    } else {
        // `cargo test` may not have initialized the runtime search path.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../runtime/snippets/friendly-snippets/package.json")
    };
    if !package.is_file() {
        return None;
    }
    package.parent().map(Path::to_path_buf)
}

fn language_paths() -> &'static HashMap<String, Vec<PathBuf>> {
    static PATHS: OnceLock<HashMap<String, Vec<PathBuf>>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let Some(root) = vendor_root() else {
            return HashMap::new();
        };
        let Ok(bytes) = fs::read(root.join("package.json")) else {
            return HashMap::new();
        };
        let Ok(package) = serde_json::from_slice::<SnippetPackage>(&bytes) else {
            log::debug!("snippets: failed to parse package.json");
            return HashMap::new();
        };
        let Some(contributes) = package.contributes else {
            return HashMap::new();
        };
        let mut paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for contribution in contributes.snippets {
            let languages = match contribution.language {
                RawPrefix::One(language) => vec![language],
                RawPrefix::Many(languages) => languages,
            };
            let relative = contribution.path.trim_start_matches("./");
            if relative.split('/').any(|part| part == "..") {
                continue;
            }
            let path = root.join(relative);
            for language in languages {
                let language = language.to_ascii_lowercase();
                let entry = paths.entry(language).or_default();
                if !entry.contains(&path) {
                    entry.push(path.clone());
                }
            }
        }
        paths
    })
}

fn language_alias(language: &str) -> Option<&'static str> {
    match language {
        "c-sharp" | "c_sharp" => Some("csharp"),
        "jsx" => Some("javascriptreact"),
        "tsx" => Some("typescriptreact"),
        "bash" => Some("shellscript"),
        "latex" => Some("tex"),
        "git-commit" => Some("gitcommit"),
        _ => None,
    }
}

fn snippets_for(language: &str) -> Arc<Vec<SnippetDef>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<SnippetDef>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let language = language.to_ascii_lowercase();
    if let Some(cached) = cache.lock().unwrap().get(&language) {
        return cached.clone();
    }

    let mut definitions = Vec::new();
    let mut seen = HashSet::new();
    if let Some(paths) = language_paths().get(&language) {
        for path in paths {
            load_file(path, &mut definitions, &mut seen);
        }
    }
    if definitions.is_empty() {
        if let Some(alias) = language_alias(&language) {
            if let Some(paths) = language_paths().get(alias) {
                for path in paths {
                    load_file(path, &mut definitions, &mut seen);
                }
            }
        }
    }
    add_builtin_extras(&language, &mut definitions, &mut seen);

    let definitions = Arc::new(definitions);
    cache.lock().unwrap().insert(language, definitions.clone());
    definitions
}

fn load_file(path: &Path, definitions: &mut Vec<SnippetDef>, seen: &mut HashSet<String>) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(raw) = serde_json::from_slice::<HashMap<String, RawSnippet>>(&bytes) else {
        log::debug!("snippets: failed to parse {}", path.display());
        return;
    };
    for (name, snippet) in raw {
        let Some(prefix) = snippet.prefix else {
            continue;
        };
        let prefixes: Vec<String> = match prefix {
            RawPrefix::One(prefix) => vec![prefix],
            RawPrefix::Many(prefixes) => prefixes,
        }
        .into_iter()
        .map(|prefix| prefix.trim().to_string())
        .filter(|prefix| !prefix.is_empty())
        .collect();
        if prefixes.is_empty() {
            continue;
        }
        let Some(body) = snippet.body else {
            continue;
        };
        let body = match body {
            RawBody::One(body) => body,
            RawBody::Lines(lines) => lines.join("\n"),
        };
        if body.is_empty() {
            continue;
        }
        // Prefixes are the completion labels. Deduplicate by the primary one
        // so two files contributing the same snippet do not double up.
        if !seen.insert(prefixes[0].to_ascii_lowercase()) {
            continue;
        }
        let description = match snippet.description {
            Some(RawDescription::One(description)) if !description.is_empty() => description,
            Some(RawDescription::Lines(lines)) => lines.join("\n"),
            _ => name,
        };
        definitions.push(SnippetDef {
            prefixes,
            body,
            description,
        });
    }
}

fn push_extra(
    definitions: &mut Vec<SnippetDef>,
    seen: &mut HashSet<String>,
    label: &str,
    body: String,
    description: &str,
) {
    if !seen.insert(label.to_ascii_lowercase()) {
        return;
    }
    definitions.push(SnippetDef {
        prefixes: vec![label.to_string()],
        body,
        description: description.to_string(),
    });
}

fn add_builtin_extras(
    language: &str,
    definitions: &mut Vec<SnippetDef>,
    seen: &mut HashSet<String>,
) {
    match language {
        "markdown" => {
            // The vendor note snippet has no explicit placeholder, so typing
            // its body would re-trigger completion. Replace it with a variant
            // that leaves the cursor in a real tabstop.
            definitions.retain(|definition| {
                !definition
                    .prefixes
                    .iter()
                    .any(|prefix| prefix.eq_ignore_ascii_case("note"))
            });
            seen.remove("note");

            // friendly-snippets stops at 5x5; extend the grid to 7x7.
            for rows in 2..=7 {
                for columns in 1..=7 {
                    push_extra(
                        definitions,
                        seen,
                        &format!("{rows}x{columns}table"),
                        table_snippet(rows, columns),
                        &format!("Insert a {rows}-row, {columns}-column table"),
                    );
                }
            }
            push_extra(
                definitions,
                seen,
                "underline",
                "<u>${1}</u> $0".into(),
                "Insert underlined text (HTML)",
            );
            push_extra(
                definitions,
                seen,
                "highlight",
                "==${1}== $0".into(),
                "Insert highlighted text",
            );
            for (label, title) in [
                ("note", "NOTE"),
                ("tip", "TIP"),
                ("warning", "WARNING"),
                ("danger", "DANGER"),
                ("info", "INFO"),
                ("todo", "TODO"),
                ("success", "SUCCESS"),
                ("question", "QUESTION"),
                ("example", "EXAMPLE"),
                ("failure", "FAILURE"),
                ("bug", "BUG"),
                ("abstract", "ABSTRACT"),
                ("summary", "SUMMARY"),
                ("important", "IMPORTANT"),
                ("caution", "CAUTION"),
            ] {
                push_extra(
                    definitions,
                    seen,
                    label,
                    format!("> [!{title}]\n> ${{1}}"),
                    &format!("Insert a {title} callout"),
                );
            }
        }
        "cpp" => {
            push_extra(
                definitions,
                seen,
                "gfor",
                "for (${1:auto} ${2:i} = ${3:0}; $2 ${4:<=} ${5:n}; $2${6:++}) {\n\t${7:// code}\n}".into(),
                "For loop with editable parts",
            );
        }
        _ => {}
    }
}

/// Build a Markdown table snippet with `rows` rows (including the header) and
/// `columns` columns. Header cells are the first tabstops, then body cells,
/// then the final `$0`.
fn table_snippet(rows: usize, columns: usize) -> String {
    let mut body = String::new();
    let mut tabstop = 1;

    body.push('|');
    for column in 1..=columns {
        body.push_str(&format!(" ${{{tabstop}:Column{column}}} |"));
        tabstop += 1;
    }
    body.push('\n');
    body.push('|');
    for _ in 0..columns {
        body.push_str(" --- |");
    }
    body.push('\n');

    for row in 1..rows {
        body.push('|');
        for column in 1..=columns {
            body.push_str(&format!(" ${{{tabstop}:Item{row}.{column}}} |"));
            tabstop += 1;
        }
        body.push('\n');
    }

    body.push_str("$0");
    body
}

fn prefix_matches(typed: &str, prefix: &str) -> bool {
    prefix
        .get(..typed.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(typed))
}

/// Returns a local completion request when the typed word matches at least
/// one snippet prefix for the current language.
pub(super) fn completion(
    editor: &Editor,
    _trigger: Trigger,
    handle: TaskHandle,
    savepoint: Arc<helix_view::document::SavePoint>,
) -> Option<impl FnOnce() -> CompletionResponse> {
    let (view, doc): (&helix_view::View, &Document) = current_ref!(editor);
    let language = doc.language_name().unwrap_or("global");

    let text = doc.text().slice(..);
    let selection = doc.selection(view.id).clone();
    let cursor = selection.primary().cursor(text);
    let word_start = movement::move_prev_word_start(text, CoreRange::point(cursor), 1);
    if word_start.head == cursor {
        return None;
    }
    let typed_word: String = text.slice(word_start.head..cursor).into();
    if typed_word.is_empty() || handle.is_canceled() {
        return None;
    }

    let definitions = snippets_for(language);
    if definitions.is_empty() {
        return None;
    }

    let rope = doc.text().clone();
    let replace_mode = editor.config().completion_replace;
    // `SnippetRenderCtx` holds a non-Send boxed resolver, so build it inside
    // the blocking task from the plain document settings.
    let tab_width = doc.tab_width();
    let indent_style = doc.indent_style;
    let line_ending = doc.line_ending.as_str();

    Some(move || {
        let mut render_ctx = SnippetRenderCtx {
            resolve_var: Box::new(|_| None),
            tab_width,
            indent_style,
            line_ending,
        };
        let mut items = Vec::new();
        let mut seen_labels = HashSet::new();

        for definition in definitions.iter() {
            let Some(label) = definition
                .prefixes
                .iter()
                .find(|prefix| prefix_matches(&typed_word, prefix))
            else {
                continue;
            };
            if !seen_labels.insert(label.to_ascii_lowercase()) {
                continue;
            }
            let Ok(snippet) = Snippet::parse(&definition.body) else {
                log::debug!(
                    "snippets: failed to parse `{}`: {:?}",
                    label,
                    definition.body
                );
                continue;
            };
            let (transaction, rendered) = util::generate_transaction_from_snippet(
                &rope,
                &selection,
                None,
                replace_mode,
                snippet,
                &mut render_ctx,
            );
            items.push(CompletionItem::Other(helix_core::CompletionItem {
                transaction,
                label: label.clone().into(),
                kind: Cow::Borrowed("snippet"),
                documentation: Some(definition.description.clone()),
                provider: CompletionProvider::Snippet,
                snippet: Some(rendered),
            }));
        }

        CompletionResponse {
            items: CompletionItems::Other(items),
            provider: CompletionProvider::Snippet,
            context: ResponseContext {
                is_incomplete: false,
                priority: -1,
                savepoint,
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::indent::IndentStyle;
    use helix_core::snippets::{ActiveSnippet, RenderedSnippet};
    use helix_core::{Rope, Selection};

    fn find_def<'a>(definitions: &'a [SnippetDef], prefix: &str) -> &'a SnippetDef {
        definitions
            .iter()
            .find(|definition| {
                definition
                    .prefixes
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(prefix))
            })
            .unwrap_or_else(|| panic!("missing snippet `{prefix}`"))
    }

    fn render_snippet(body: &str, before: &str) -> (Rope, Selection, RenderedSnippet) {
        let rope = Rope::from(format!("{before} "));
        let selection = Selection::point(before.len());
        let mut ctx = SnippetRenderCtx {
            resolve_var: Box::new(|_| None),
            tab_width: 4,
            indent_style: IndentStyle::Spaces(4),
            line_ending: "\n",
        };
        let snippet = Snippet::parse(body).unwrap();
        let (transaction, rendered) = util::generate_transaction_from_snippet(
            &rope, &selection, None, false, snippet, &mut ctx,
        );
        let mut rendered_rope = rope.clone();
        transaction.apply(&mut rendered_rope);
        let selection = transaction
            .selection()
            .expect("snippet transaction sets the first tabstop")
            .clone();
        (rendered_rope, selection, rendered)
    }

    #[test]
    fn markdown_loader_merges_vendor_and_extras() {
        let definitions = snippets_for("markdown");
        find_def(&definitions, "bold");
        find_def(&definitions, "note");
        find_def(&definitions, "7x7table");
        find_def(&definitions, "tip");
        find_def(&definitions, "underline");
        // The vendor's 2x1..5x5 tables must not be duplicated by the extras.
        let table_count = definitions
            .iter()
            .filter(|definition| definition.prefixes[0].ends_with("table"))
            .count();
        assert_eq!(table_count, 1 + 42); // `table` + 2..7 x 1..7
    }

    #[test]
    fn cpp_loader_includes_mirrored_for_and_gfor() {
        let definitions = snippets_for("cpp");
        let for_loop = find_def(&definitions, "for");
        assert!(for_loop.body.contains("$2"));
        find_def(&definitions, "gfor");
    }

    #[test]
    fn cpp_for_mirror_has_three_ranges_and_is_the_second_tabstop() {
        let definitions = snippets_for("cpp");
        let for_loop = find_def(&definitions, "for");
        let (rope, first_selection, rendered) = render_snippet(&for_loop.body, "for");
        assert_eq!(first_selection.ranges().len(), 1);
        // The first tabstop is the `size_t` placeholder.
        assert_eq!(
            first_selection.primary().to(),
            first_selection.primary().from() + "size_t".len()
        );

        let mut active = ActiveSnippet::new(rendered.clone()).expect("for loop has tabstops");
        let (mirror_selection, _) = active.next_tabstop(&first_selection);
        assert_eq!(mirror_selection.ranges().len(), 3);
        let mirror_text: String = mirror_selection
            .ranges()
            .iter()
            .map(|range| rope.slice(range.from()..range.to()).to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(mirror_text, "i|i|i");
    }

    #[test]
    fn gfor_has_operator_increment_and_body_placeholders() {
        let definitions = snippets_for("cpp");
        let gfor = find_def(&definitions, "gfor");
        assert!(gfor.body.contains("${4:<=}"));
        assert!(gfor.body.contains("${6:++}"));
        assert!(gfor.body.contains("${7:// code}"));
        assert!(Snippet::parse(&gfor.body).is_ok());
    }

    #[test]
    fn first_placeholder_is_replaced_by_typing() {
        let definitions = snippets_for("cpp");
        let gfor = find_def(&definitions, "gfor");
        let (rope, first_selection, _) = render_snippet(&gfor.body, "gfor");
        let selected: String = first_selection
            .ranges()
            .iter()
            .map(|range| rope.slice(range.from()..range.to()).to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(selected, "auto");
    }

    #[test]
    fn common_languages_load_snippets() {
        for language in [
            "rust",
            "python",
            "javascript",
            "typescript",
            "go",
            "lua",
            "java",
            "csharp",
            "c-sharp",
            "jsx",
            "tsx",
            "bash",
            "latex",
        ] {
            assert!(
                !snippets_for(language).is_empty(),
                "no snippets loaded for {language}"
            );
        }
    }

    #[test]
    fn prefix_matches_case_insensitively() {
        assert!(prefix_matches("b", "bold"));
        assert!(prefix_matches("GF", "gfor"));
        assert!(prefix_matches("2x2", "2x2table"));
        assert!(!prefix_matches("bo", "italic"));
        assert!(!prefix_matches("2x8", "2x2table"));
    }
}
