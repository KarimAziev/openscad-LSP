use std::collections::HashSet;

use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Url};
use tree_sitter::Node;
use tree_sitter_traversal2::{Order, traverse};

use crate::{parse_code::ParsedCode, server::Server, utils::*};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum CallableKind {
    Function,
    Module,
}

type Names = HashSet<(String, CallableKind)>;

#[derive(Eq, Hash, PartialEq)]
struct CallableDeclaration {
    name: (String, CallableKind),
    url: Url,
    start_byte: usize,
}

#[derive(Default)]
struct ImportNames {
    references: Names,
    declarations: HashSet<CallableDeclaration>,
}

impl Server {
    pub(crate) fn unused_use_diagnostics(&mut self, code: &ParsedCode) -> Vec<Diagnostic> {
        let root = code.tree.root_node();
        let imports: Vec<_> = root
            .named_children(&mut root.walk())
            .filter(|node| node.kind().is_use_statement())
            .collect();
        if imports.is_empty() || root.has_error() {
            return vec![];
        }

        let document = if self.args.unused_use_includes {
            let Some(document) = self.import_names(&code.url, false, 0) else {
                return vec![];
            };
            document
        } else {
            ImportNames {
                references: traverse(code.tree.walk(), Order::Pre)
                    .filter(|node| node.kind() == "identifier")
                    .filter_map(|node| {
                        reference_kind(node)
                            .map(|kind| (node_text(&code.code, &node).to_owned(), kind))
                    })
                    .collect(),
                ..Default::default()
            }
        };

        imports
            .into_iter()
            .filter_map(|node| {
                let url = code.get_include_url(&node)?;
                let exports = if self.args.unused_use_includes {
                    self.import_names(&url, true, 1)?
                } else {
                    let library = self.get_code(&url)?;
                    let library = library.borrow();
                    let root = library.tree.root_node();
                    if root.has_error() {
                        return None;
                    }
                    ImportNames {
                        declarations: root
                            .named_children(&mut root.walk())
                            .filter_map(|node| callable_declaration(&library, node))
                            .collect(),
                        ..Default::default()
                    }
                };
                if exports.declarations.iter().any(|declaration| {
                    document.references.contains(&declaration.name)
                        && !document.declarations.contains(declaration)
                }) {
                    return None;
                }
                let path = node.child(1)?;
                Some(Diagnostic {
                    range: node.lsp_range(),
                    severity: Some(DiagnosticSeverity::WARNING),
                    code: Some(NumberOrString::String("unused-use".to_owned())),
                    source: Some("openscad-lsp".to_owned()),
                    message: format!("unused use directive `{}`", node_text(&code.code, &path)),
                    tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                    ..Default::default()
                })
            })
            .collect()
    }

    /// Collect declarations and possible references through textual includes.
    /// Declaration locations identify callables already available without `use`.
    /// Other name collisions count as possible usage regardless of precedence.
    fn import_names(&mut self, url: &Url, exports_only: bool, depth: usize) -> Option<ImportNames> {
        let mut names = ImportNames::default();
        let mut visited = HashSet::new();
        let mut pending = vec![(url.clone(), depth, true)];

        while let Some((url, depth, file_scope)) = pending.pop() {
            if !visited.insert((url.clone(), file_scope)) {
                continue;
            }
            if self.args.depth != 0 && depth > self.args.depth {
                return None;
            }
            let code = self.get_code(&url)?;
            let code = code.borrow();
            let root = code.tree.root_node();
            if root.has_error() {
                return None;
            }

            for node in traverse(code.tree.walk(), Order::Pre) {
                if node.kind().is_include_statement() {
                    // Only file-scope declarations are exported by `use`.
                    let included_at_file_scope = file_scope && node.parent() == Some(root);
                    if !exports_only || included_at_file_scope {
                        pending.push((
                            code.get_include_url(&node)?,
                            depth + 1,
                            included_at_file_scope,
                        ));
                    }
                } else if file_scope && node.parent() == Some(root) {
                    if let Some(declaration) = callable_declaration(&code, node) {
                        names.declarations.insert(declaration);
                    }
                } else if !exports_only && node.kind() == "identifier" {
                    if let Some(kind) = reference_kind(node) {
                        names
                            .references
                            .insert((node_text(&code.code, &node).to_owned(), kind));
                    }
                }
            }
        }
        Some(names)
    }
}

fn callable_declaration(code: &ParsedCode, node: Node) -> Option<CallableDeclaration> {
    let kind = match node.kind() {
        "function_item" => CallableKind::Function,
        "module_item" => CallableKind::Module,
        _ => return None,
    };
    let name = node.child_by_field_name("name")?;
    Some(CallableDeclaration {
        name: (node_text(&code.code, &name).to_owned(), kind),
        url: code.url.clone(),
        start_byte: name.start_byte(),
    })
}

fn reference_kind(node: Node) -> Option<CallableKind> {
    let parent = node.parent()?;
    if parent.child_by_field_name("name") == Some(node) {
        match parent.kind() {
            "module_call" => return Some(CallableKind::Module),
            "module_item" | "function_item" | "assignment" => return None,
            _ => {}
        }
    }
    if parent.kind() == "parameter"
        || (parent.kind() == "dot_index_expression"
            && parent.child_by_field_name("index") == Some(node))
    {
        return None;
    }
    // Bare identifiers can pass function values to another function or module.
    Some(CallableKind::Function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use lsp_server::Connection;
    use std::fs;
    use tempfile::tempdir;

    fn diagnostics(source: &str, files: &[(&str, &str)], depth: usize) -> Vec<Diagnostic> {
        diagnostics_with_mode(source, files, depth, false)
    }

    fn diagnostics_with_includes(
        source: &str,
        files: &[(&str, &str)],
        depth: usize,
    ) -> Vec<Diagnostic> {
        diagnostics_with_mode(source, files, depth, true)
    }

    fn diagnostics_with_mode(
        source: &str,
        files: &[(&str, &str)],
        depth: usize,
        includes: bool,
    ) -> Vec<Diagnostic> {
        let directory = tempdir().unwrap();
        for (name, text) in files {
            fs::write(directory.path().join(name), text).unwrap();
        }
        let (connection, _client) = Connection::memory();
        let mut args = Cli::parse_from(["openscad-lsp"]);
        args.depth = depth;
        args.unused_use_includes = includes;
        let mut server = Server::new(connection, args);
        let url = Url::from_file_path(directory.path().join("main.scad")).unwrap();
        let code = server.insert_code(url, source.to_owned());
        server.unused_use_diagnostics(&code.borrow())
    }

    const LIBRARY: &[(&str, &str)] = &[(
        "lib.scad",
        "module shape() {} function size() = 1; variable = 1;",
    )];

    #[test]
    fn include_analysis_requires_cli_flag() {
        assert!(!Cli::parse_from(["openscad-lsp"]).unused_use_includes);
        assert!(Cli::parse_from(["openscad-lsp", "--unused-use-includes"]).unused_use_includes);
    }

    #[test]
    fn default_analysis_loads_only_direct_use_files() {
        let directory = tempdir().unwrap();
        for (name, text) in [
            ("params.scad", "module broken( {"),
            ("nested.scad", "module indirect() {}"),
            ("body.scad", "shape();"),
            (
                "lib.scad",
                "include <params.scad>\nuse <nested.scad>\nmodule shape() {}",
            ),
        ] {
            fs::write(directory.path().join(name), text).unwrap();
        }
        let (connection, _client) = Connection::memory();
        let mut server = Server::new(connection, Cli::parse_from(["openscad-lsp"]));
        let url = Url::from_file_path(directory.path().join("main.scad")).unwrap();
        let code = server.insert_code(
            url,
            "include <body.scad>\nuse <lib.scad>\ncube(1);".to_owned(),
        );
        assert_eq!(server.unused_use_diagnostics(&code.borrow()).len(), 1);
        let lib_url = Url::from_file_path(directory.path().join("lib.scad")).unwrap();
        assert!(server.codes.contains_key(&lib_url));
        for name in ["params.scad", "nested.scad", "body.scad"] {
            let url = Url::from_file_path(directory.path().join(name)).unwrap();
            assert!(
                !server.codes.contains_key(&url),
                "unexpectedly loaded {name}"
            );
        }
    }

    #[test]
    fn default_analysis_checks_only_direct_declarations_and_document_references() {
        let files = [
            ("params.scad", "function shared() = 1;"),
            ("body.scad", "shape();"),
            ("lib.scad", "include <params.scad>\nmodule shape() {}"),
        ];
        let source = "include <params.scad>\nuse <lib.scad>\necho(shared());";
        assert_eq!(diagnostics(source, &files, 0).len(), 1);
        assert!(diagnostics("use <lib.scad>\nshape();", &files, 0).is_empty());

        let source = "use <lib.scad>\ninclude <body.scad>";
        assert_eq!(diagnostics(source, &files, 0).len(), 1);
        assert!(diagnostics_with_includes(source, &files, 0).is_empty());

        let source = "use <lib.scad>\necho(shared());";
        assert_eq!(diagnostics(source, &files, 0).len(), 1);
        assert!(diagnostics_with_includes(source, &files, 0).is_empty());
    }

    #[test]
    fn default_analysis_skips_missing_or_invalid_direct_use_files() {
        for files in [vec![], vec![("lib.scad", "module shape( {")]] {
            assert!(diagnostics("use <lib.scad>\ncube(1);", &files, 0).is_empty());
        }
        assert!(diagnostics("use <lib.scad>\nshape(", LIBRARY, 0).is_empty());
        assert_eq!(
            diagnostics("use <lib.scad>\ninclude <missing.scad>", LIBRARY, 0).len(),
            1
        );
    }

    #[test]
    fn reports_whole_directive_with_unnecessary_tag() {
        let result = diagnostics("use <lib.scad>\ncube(1);", LIBRARY, 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "unused use directive `<lib.scad>`");
        assert_eq!(result[0].severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(result[0].tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        assert_eq!(result[0].range.start, lsp_types::Position::new(0, 0));
        assert_eq!(result[0].range.end, lsp_types::Position::new(0, 14));
        assert_eq!(
            result[0].code,
            Some(NumberOrString::String("unused-use".to_owned()))
        );
    }

    #[test]
    fn counts_calls_defaults_children_and_function_values() {
        for source in [
            "shape();",
            "echo(size());",
            "module wrapper() { shape(); }",
            "function wrapper() = size();",
            "module wrapper(x = size()) {}",
            "translate([0,0,0]) shape();",
            "callback = size; echo(callback());",
            "echo([for (x = [1]) size()]);",
        ] {
            assert!(
                diagnostics(&format!("use <lib.scad>\n{source}"), LIBRARY, 0).is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn ignores_comments_strings_declarations_labels_and_variables() {
        for source in [
            "// shape(); size();\ncube(1);",
            "echo(\"shape size\");",
            "module shape() {} function size() = 2;",
            "module wrapper(size) {}",
            "echo(size = 1);",
            "size = 2;",
            "echo(object.size);",
            "echo(variable);",
        ] {
            assert_eq!(
                diagnostics(&format!("use <lib.scad>\n{source}"), LIBRARY, 0).len(),
                1,
                "{source}"
            );
        }
    }

    #[test]
    fn distinguishes_function_and_module_namespaces() {
        assert_eq!(
            diagnostics("use <lib.scad>\nsize(); echo(shape());", LIBRARY, 0).len(),
            1
        );
    }

    #[test]
    fn preserves_ambiguous_imports() {
        let files = [
            ("a.scad", "module shape() {}"),
            ("b.scad", "module shape() {}"),
        ];
        assert!(diagnostics("use <a.scad>\nuse <b.scad>\nshape();", &files, 0).is_empty());
        assert!(diagnostics("use <a.scad>\nmodule shape() {} shape();", &files, 0).is_empty());
    }

    #[test]
    fn reports_only_unused_imports() {
        let files = [
            ("a.scad", "module first() {}"),
            ("b.scad", "module second() {}"),
        ];
        let result = diagnostics("use <a.scad>\nuse <b.scad>\nfirst();", &files, 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "unused use directive `<b.scad>`");
    }

    #[test]
    fn shared_include_callable_does_not_keep_unused_use_alive() {
        let files = [
            (
                "params.scad",
                "function merge_spec(value) = value; spec = merge_spec(1);",
            ),
            ("arm.scad", "include <params.scad>\nmodule arm() {}"),
        ];
        let result = diagnostics_with_includes(
            "include <params.scad>\nuse <arm.scad>\ncube(spec);",
            &files,
            0,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "unused use directive `<arm.scad>`");
        assert!(
            diagnostics_with_includes("include <params.scad>\nuse <arm.scad>\narm();", &files, 0)
                .is_empty()
        );
        assert!(
            diagnostics_with_includes("use <arm.scad>\necho(merge_spec(1));", &files, 0).is_empty()
        );
    }

    #[test]
    fn shared_declarations_are_matched_by_location_and_file_scope() {
        let files = [
            ("shared.scad", "module shape() {}"),
            ("lib.scad", "include <shared.scad>"),
            ("other.scad", "module shape() {}"),
            ("params.scad", "include <shared.scad>"),
        ];
        assert_eq!(
            diagnostics_with_includes("include <params.scad>\nuse <lib.scad>\nshape();", &files, 0)
                .len(),
            1
        );
        assert!(
            diagnostics_with_includes("include <other.scad>\nuse <lib.scad>\nshape();", &files, 0)
                .is_empty()
        );
        assert!(
            diagnostics_with_includes(
                "use <lib.scad>\nmodule local() { include <shared.scad> } shape();",
                &files,
                0,
            )
            .is_empty()
        );
        // The same file can be included in both local and file scope.
        assert_eq!(diagnostics_with_includes(
            "include <shared.scad>\nuse <lib.scad>\nmodule local() { include <shared.scad> } shape();",
            &files,
            0,
        ).len(), 1);
    }

    #[test]
    fn follows_includes_for_exports_and_references_with_cycles() {
        let files = [
            ("lib.scad", "include <exports.scad>"),
            ("exports.scad", "include <lib.scad>\nmodule shape() {}"),
            ("body.scad", "include <nested.scad>"),
            ("nested.scad", "include <body.scad>\nshape();"),
        ];
        assert!(
            diagnostics_with_includes("use <lib.scad>\ninclude <body.scad>", &files, 0).is_empty()
        );
        assert_eq!(
            diagnostics_with_includes("use <lib.scad>\ncube(1);", &files, 0).len(),
            1
        );
    }

    #[test]
    fn nested_use_does_not_reexport_callables() {
        let files = [
            (
                "lib.scad",
                "use <private.scad>\nmodule wrapper() { shape(); }",
            ),
            ("private.scad", "module shape() {}"),
        ];
        assert_eq!(diagnostics("use <lib.scad>\nshape();", &files, 0).len(), 1);
        assert!(diagnostics("use <lib.scad>\nwrapper();", &files, 0).is_empty());
    }

    #[test]
    fn skips_incomplete_analysis() {
        for files in [
            vec![],
            vec![("lib.scad", "module shape( {")],
            vec![("lib.scad", "include <missing.scad>")],
        ] {
            assert!(diagnostics_with_includes("use <lib.scad>\ncube(1);", &files, 0).is_empty());
        }
        assert!(
            diagnostics_with_includes("use <lib.scad>\ninclude <missing.scad>", LIBRARY, 0)
                .is_empty()
        );
        assert!(diagnostics_with_includes("use <lib.scad>\nshape(", LIBRARY, 0).is_empty());
        let files = [
            ("lib.scad", "include <nested.scad>"),
            ("nested.scad", "module shape() {}"),
        ];
        assert!(diagnostics_with_includes("use <lib.scad>\ncube(1);", &files, 1).is_empty());
        assert_eq!(
            diagnostics_with_includes("use <lib.scad>\ncube(1);", &files, 2).len(),
            1
        );
    }

    #[test]
    fn does_not_report_include_directives() {
        assert!(diagnostics("include <lib.scad>\ncube(1);", LIBRARY, 0).is_empty());
    }
}
