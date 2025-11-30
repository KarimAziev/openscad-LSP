use lsp_types::{CompletionItemKind, Range, SymbolKind, Url};
use tree_sitter::Node;

use crate::utils::*;

#[derive(Clone, Debug)]
pub(crate) struct Param {
    pub name: String,
    pub default: Option<String>,
    pub range: Range,
}

impl Param {
    pub(crate) fn parse_declaration(code: &str, node: &Node) -> Vec<Self> {
        node.children(&mut node.walk())
            .filter_map(|child| {
                let kind = child.kind();
                if !kind.is_parameter() {
                    return None;
                }

                let child = child.child(0).unwrap();
                let kind = child.kind();
                match kind {
                    "identifier" => Some(Self {
                        name: node_text(code, &child).to_owned(),
                        default: None,
                        range: child.lsp_range(),
                    }),
                    "assignment" => child.child_by_field_name("name").and_then(|left| {
                        child.child_by_field_name("value").map(|right| Self {
                            name: node_text(code, &left).to_owned(),
                            default: Some(node_text(code, &right).trim().to_owned()),
                            range: left.lsp_range(),
                        })
                    }),
                    "special_variable" => None,
                    _ => None,
                }
            })
            .collect()
    }

    pub(crate) fn format_params(params: &[Self], include_defaults: bool) -> String {
        params
            .iter()
            .map(|p| {
                if include_defaults {
                    match &p.default {
                        Some(default) => format!("{}={}", p.name, default),
                        None => p.name.clone(),
                    }
                } else {
                    p.name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Default)]
pub(crate) enum ItemKind {
    #[default]
    Variable,
    Function {
        params: Vec<Param>,
    },
    Keyword,
    Module {
        params: Vec<Param>,
    },
}

impl ItemKind {
    pub(crate) const fn completion_kind(&self) -> CompletionItemKind {
        match self {
            Self::Variable => CompletionItemKind::VARIABLE,
            Self::Function { .. } => CompletionItemKind::FUNCTION,
            Self::Keyword => CompletionItemKind::KEYWORD,
            Self::Module { .. } => CompletionItemKind::MODULE,
        }
    }
}

#[derive(Default)]
pub(crate) struct Item {
    pub name: String,
    pub kind: ItemKind,
    pub range: Range,
    pub url: Option<Url>,
    pub is_builtin: bool,
    pub is_top_level: bool,

    pub(crate) doc: Option<String>,
    pub(crate) hover: Option<String>,
    pub(crate) label: Option<String>,
}

impl Item {
    pub(crate) fn get_hover(&mut self) -> String {
        if self.hover.is_none() {
            self.hover = Some(self.make_hover());
        }
        // log_to_console!("{:?}\n\n", &self.hover);
        self.hover.as_ref().unwrap().to_owned()
    }

    pub(crate) fn completion_text(&self) -> String {
        self.name.clone()
    }

    pub(crate) fn make_hover(&self) -> String {
        let mut label = match &self.label {
            Some(label) => label.to_owned(),
            None => self.make_label(),
        };
        label = match self.kind {
            ItemKind::Function { .. } => format!("```scad\nfunction {label}\n```"),
            ItemKind::Module { .. } => format!("```scad\nmodule {label}\n```"),
            _ => format!("```scad\n{label}\n```"),
        };
        if let Some(doc) = &self.doc {
            if self.is_builtin {
                label = format!("{label}\n---\n\n{doc}\n");
            } else {
                label = format!("{label}\n---\n\n<pre>\n{doc}\n</pre>\n");
            }
        }
        // print!("{}", &label);
        label
    }

    pub(crate) fn make_label(&self) -> String {
        match &self.kind {
            ItemKind::Variable => self.name.to_owned(),
            ItemKind::Function { params } => {
                format!("{}({})", self.name, Param::format_params(params, true))
            }
            ItemKind::Keyword => self.name.clone(),
            ItemKind::Module { params } => {
                format!("{}({})", self.name, Param::format_params(params, true))
            }
        }
    }

    pub(crate) fn signature(&self, include_defaults: bool) -> Option<String> {
        match &self.kind {
            ItemKind::Function { params } | ItemKind::Module { params } => {
                if include_defaults {
                    Some(format!(
                        "{}({})",
                        self.name,
                        Param::format_params(params, include_defaults)
                    ))
                } else {
                    Some(self.name.clone())
                }
            }
            _ => None,
        }
    }

    pub(crate) fn parse(code: &str, node: &Node) -> Option<Self> {
        let extract_name = |node: &Node, name| {
            // log_to_console!("{} {:?}", name, res);
            node.child_by_field_name(name)
                .map(|child| node_text(code, &child).to_owned())
        };

        let kind = node.kind();
        // log_to_console!("{}", kind);
        match kind {
            "module_item" => Some(Self {
                name: extract_name(node, "name")?,
                kind: ItemKind::Module {
                    params: node
                        .child_by_field_name("parameters")
                        .map_or(vec![], |params| Param::parse_declaration(code, &params)),
                },
                range: node.lsp_range(),
                ..Default::default()
            }),
            "function_item" => Some(Self {
                name: extract_name(node, "name")?,
                kind: ItemKind::Function {
                    params: node
                        .child_by_field_name("parameters")
                        .map_or(vec![], |params| Param::parse_declaration(code, &params)),
                },
                range: node.lsp_range(),
                ..Default::default()
            }),
            "var_declaration" => {
                let node = node.named_child(0)?;
                Some(Self {
                    name: extract_name(&node, "name")?,
                    kind: ItemKind::Variable,
                    range: node.lsp_range(),
                    ..Default::default()
                })
            }
            _ => None,
        }
    }

    pub(crate) const fn get_symbol_kind(&self) -> SymbolKind {
        match self.kind {
            ItemKind::Function { .. } => SymbolKind::FUNCTION,
            ItemKind::Module { .. } => SymbolKind::MODULE,
            ItemKind::Variable => SymbolKind::VARIABLE,
            ItemKind::Keyword => SymbolKind::KEY,
        }
    }
}
