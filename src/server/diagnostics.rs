use lsp_types::{Diagnostic, DiagnosticSeverity};
use tree_sitter::Node;

use crate::{parse_code::ParsedCode, utils::*};

#[derive(Debug)]
struct LocalDeclaration {
    name: String,
    range: lsp_types::Range,
    used: bool,
    report_unused: bool,
}

#[derive(Default, Debug)]
struct Scope {
    declarations: Vec<LocalDeclaration>,
}

struct UnusedLocalAnalyzer<'a> {
    code: &'a str,
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<Scope>,
    callable_depth: usize,
}

impl<'a> UnusedLocalAnalyzer<'a> {
    fn new(code: &'a str) -> Self {
        Self {
            code,
            diagnostics: vec![],
            scopes: vec![],
            callable_depth: 0,
        }
    }

    fn finish(mut self, root: Node) -> Vec<Diagnostic> {
        self.visit_children(root);
        self.diagnostics
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn pop_scope(&mut self) {
        let Some(scope) = self.scopes.pop() else {
            return;
        };

        self.diagnostics.extend(
            scope
                .declarations
                .into_iter()
                .filter(|declaration| declaration.report_unused && !declaration.used)
                .map(|declaration| Diagnostic {
                    range: declaration.range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("openscad-lsp".to_owned()),
                    message: format!("unused local variable `{}`", declaration.name),
                    ..Default::default()
                }),
        );
    }

    fn declare(&mut self, name_node: Node, report_unused: bool) {
        if self.scopes.is_empty() || name_node.kind() != "identifier" {
            return;
        }

        let name = node_text(self.code, &name_node).to_owned();
        let report_unused = report_unused && self.callable_depth > 0 && !name.starts_with('_');
        self.scopes
            .last_mut()
            .expect("scope exists")
            .declarations
            .push(LocalDeclaration {
                name,
                range: name_node.lsp_range(),
                used: false,
                report_unused,
            });
    }

    fn mark_used(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(declaration) = scope
                .declarations
                .iter_mut()
                .rev()
                .find(|declaration| declaration.name == name)
            {
                declaration.used = true;
                return;
            }
        }
    }

    fn visit_children(&mut self, node: Node) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit_node(child);
        }
    }

    fn visit_node(&mut self, node: Node) {
        match node.kind() {
            "module_item" | "function_item" | "function_lit" => self.visit_callable(node),
            "var_declaration" => self.visit_var_declaration(node),
            "let_expression"
            | "let_block"
            | "assign_block"
            | "for_block"
            | "intersection_for_block"
            | "for_clause"
            | "if_clause"
            | "each" => {
                self.visit_assignment_scope(node);
            }
            "let_prefix" => self.visit_let_prefix(node),
            "arguments" => self.visit_arguments(node),
            "module_call" => self.visit_module_call(node),
            "function_call" => self.visit_function_call(node),
            "dot_index_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit_node(value);
                }
            }
            "assignment" => self.visit_update_assignment(node),
            "identifier" => {
                if self.is_variable_reference(node) {
                    let name = node_text(self.code, &node).to_owned();
                    self.mark_used(&name);
                }
            }
            _ => self.visit_children(node),
        }
    }

    fn visit_callable(&mut self, node: Node) {
        self.callable_depth += 1;
        self.push_scope();

        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.visit_parameters(parameters);
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if Some(child) == node.child_by_field_name("name")
                || Some(child) == node.child_by_field_name("parameters")
            {
                continue;
            }

            self.visit_node(child);
        }

        self.pop_scope();
        self.callable_depth -= 1;
    }

    fn visit_parameters(&mut self, parameters: Node) {
        let mut cursor = parameters.walk();
        for parameter in parameters
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "parameter")
        {
            let Some(child) = parameter.named_child(0) else {
                continue;
            };

            match child.kind() {
                "identifier" => self.declare(child, false),
                "assignment" => {
                    if let Some(value) = child.child_by_field_name("value") {
                        self.visit_node(value);
                    }
                    if let Some(name) = child.child_by_field_name("name") {
                        self.declare(name, false);
                    }
                }
                _ => {}
            }
        }
    }

    fn visit_var_declaration(&mut self, node: Node) {
        if let Some(assignment) = node.named_child(0) {
            self.visit_declaration_assignment(assignment, true);
        }
    }

    fn visit_assignment_scope(&mut self, node: Node) {
        self.push_scope();

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "assignments" => self.visit_assignment_list(child, true),
                "let_prefix" => self.visit_let_prefix(child),
                "condition_update_clause" => self.visit_condition_update_clause(child),
                _ => self.visit_node(child),
            }
        }

        self.pop_scope();
    }

    fn visit_let_prefix(&mut self, node: Node) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "assignments" {
                self.visit_assignment_list(child, true);
            } else {
                self.visit_node(child);
            }
        }
    }

    fn visit_condition_update_clause(&mut self, node: Node) {
        if let Some(initializer) = node.child_by_field_name("initializer") {
            self.visit_condition_assignment_sequence(initializer, true);
        }

        if let Some(condition) = node.child_by_field_name("condition") {
            self.visit_node(condition);
        }

        if let Some(update) = node.child_by_field_name("update") {
            self.visit_condition_assignment_sequence(update, false);
        }
    }

    fn visit_condition_assignment_sequence(&mut self, node: Node, declares: bool) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() != "assignment" {
                self.visit_node(child);
                continue;
            }

            if declares {
                self.visit_declaration_assignment(child, true);
            } else {
                self.visit_update_assignment(child);
            }
        }
    }

    fn visit_assignment_list(&mut self, assignments: Node, report_unused: bool) {
        let mut cursor = assignments.walk();
        for assignment in assignments
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "assignment")
        {
            self.visit_declaration_assignment(assignment, report_unused);
        }
    }

    fn visit_declaration_assignment(&mut self, assignment: Node, report_unused: bool) {
        if let Some(value) = assignment.child_by_field_name("value") {
            self.visit_node(value);
        }

        if let Some(name) = assignment.child_by_field_name("name") {
            self.declare(name, report_unused);
        }
    }

    fn visit_arguments(&mut self, arguments: Node) {
        let mut cursor = arguments.walk();
        for child in arguments.named_children(&mut cursor) {
            if child.kind() == "assignment" {
                if let Some(value) = child.child_by_field_name("value") {
                    self.visit_node(value);
                }
            } else {
                self.visit_node(child);
            }
        }
    }

    fn visit_module_call(&mut self, node: Node) {
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit_node(arguments);
        }
    }

    fn visit_function_call(&mut self, node: Node) {
        if let Some(name) = node.child_by_field_name("name") {
            self.visit_node(name);
        }
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit_node(arguments);
        }
    }

    fn visit_update_assignment(&mut self, assignment: Node) {
        if let Some(name) = assignment.child_by_field_name("name") {
            self.visit_node(name);
        }
        if let Some(value) = assignment.child_by_field_name("value") {
            self.visit_node(value);
        }
    }

    fn is_variable_reference(&self, node: Node) -> bool {
        let Some(parent) = node.parent() else {
            return true;
        };

        if parent
            .child_by_field_name("name")
            .is_some_and(|name| name == node)
            && matches!(
                parent.kind(),
                "assignment" | "module_item" | "function_item" | "module_call"
            )
        {
            return false;
        }

        if parent
            .child_by_field_name("index")
            .is_some_and(|index| index == node)
            && parent.kind() == "dot_index_expression"
        {
            return false;
        }

        parent.kind() != "parameter"
    }
}

pub(crate) fn document_diagnostics(code: &ParsedCode) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<_> = error_nodes(code.tree.walk())
        .into_iter()
        .map(|node| Diagnostic {
            range: node.lsp_range(),
            severity: Some(DiagnosticSeverity::ERROR),
            message: if node.is_missing() {
                format!("missing {}", node.kind())
            } else {
                "syntax error".to_owned()
            },
            ..Default::default()
        })
        .collect();

    collect_missing_dependency_diagnostics(code.tree.root_node(), code, &mut diagnostics);

    diagnostics.extend(UnusedLocalAnalyzer::new(&code.code).finish(code.tree.root_node()));
    diagnostics
}

fn collect_missing_dependency_diagnostics(
    node: Node,
    code: &ParsedCode,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node.kind().is_dependency_statement() && code.get_include_url(&node).is_none() {
        if let Some(path) = node.child(1) {
            let mut range = path.lsp_range();
            range.start.character += 1;
            range.end.character = range.end.character.saturating_sub(1);
            diagnostics.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                message: "file not found!".to_owned(),
                ..Default::default()
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_missing_dependency_diagnostics(child, code, diagnostics);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Url;
    use std::{cell::RefCell, rc::Rc};

    fn diagnostics_for(source: &str) -> Vec<Diagnostic> {
        let url = Url::parse("file:///test.scad").unwrap();
        let code = ParsedCode::new(source.to_owned(), url, Rc::new(RefCell::new(vec![])));
        document_diagnostics(&code)
    }

    fn unused_messages(source: &str) -> Vec<String> {
        diagnostics_for(source)
            .into_iter()
            .filter(|diagnostic| diagnostic.message.starts_with("unused local variable"))
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[test]
    fn reports_unused_module_variable() {
        let messages = unused_messages(
            r#"
module m() {
  used = 1;
  unused = 2;
  echo(used);
}
"#,
        );

        assert_eq!(messages, vec!["unused local variable `unused`"]);
    }

    #[test]
    fn reports_unused_function_let_variable() {
        let messages = unused_messages("function f() = let(used = 1, unused = 2) used;\n");

        assert_eq!(messages, vec!["unused local variable `unused`"]);
    }

    #[test]
    fn ignores_top_level_variables_and_parameters() {
        let messages = unused_messages(
            r#"
top_unused = 1;
let(top_let_unused = 1) cube(1);
module m(unused_param) {
  used = 1;
  echo(used);
}
"#,
        );

        assert!(messages.is_empty());
    }

    #[test]
    fn respects_shadowing_in_nested_assignment_scopes() {
        let messages = unused_messages(
            r#"
module m() {
  x = 1;
  let(x = 2) echo(x);
}
"#,
        );

        assert_eq!(messages, vec!["unused local variable `x`"]);
    }
}
