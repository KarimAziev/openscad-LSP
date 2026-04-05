# About

**Table of Contents**

> - [About](#about)
>   - [Features](#features)
>   - [Install](#install)
>   - [IDE plugins](#ide-plugins)
>     - [Emacs](#emacs)
>   - [Usage](#usage)

This a fork of [openscad-language-server](https://github.com/dzhu/openscad-language-server), focused on improving OpenSCAD editing for larger projects and day-to-day IDE workflows.

Main differences in this fork:

- workspace-scale symbol/rename support with identifier indexing and caching
- watched workspace `.scad` files keep the symbol index fresh even when documents are closed
- configurable include traversal depth for symbol resolution (`--depth`, where `0` means unlimited)
- improved completion behavior, including better deduplication and callable parameter suggestions
- corrected `Go to Definition` behavior for parameter defaults like `p = p` (RHS resolves to outer scope)
- refreshed builtin documentation and cleaner parser-based extraction of comment docs

## Features

- builtin function/module documentation (bundled docs, including `roof()`)
- code and include-path completion
- completion docs/signatures and callable parameter-name suggestions
- jump to definition
- correct definition lookup for parameter defaults like `p = p` (RHS resolves to outer scope)
- code snippets
- function/module signatures on hover
- document symbols
- formatter using Topiary
- variable/module renaming (local scope and workspace/include-aware for global symbols)
- indexed/cached identifier lookup for workspace-scale rename/reference resolution
- watched workspace `.scad` file changes refresh the rename/reference index for closed files
- hover and suggestion documentation from comments before function/module declarations

## Install

openscad-LSP is written in [Rust](https://rust-lang.org), in order to use it, you need to
install [Rust toolchain](https://www.rust-lang.org/learn/get-started).

Since this fork is unpublished, install it locally from this repository and ensure the installed
binary is available on your `PATH` so your editor can start `openscad-lsp`.

```sh
make install-local
```

Equivalent direct command:

```sh
cargo install --path . --locked --force
```

## IDE plugins

| IDE     | Plugin                                                                                                               | Note                           |
| ------- | -------------------------------------------------------------------------------------------------------------------- | ------------------------------ |
| Neovim  | [mason.nvim](https://github.com/williamboman/mason.nvim)                                                             | Only tested on Mac and Linux   |
| Neovim  | [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig)                                                           | Only tested on Mac and Linux   |
| VS Code | [openscad-language-support](https://github.com/Leathong/openscad-support-vscode)                                     | Only tested on Mac and Windows |
| Emacs   | [lsp-mode](https://github.com/emacs-lsp/lsp-mode), eglot, [lsp-bridge](https://github.com/manateelazycat/lsp-bridge) | Only tested on Mac and Linux   |

### Emacs

For `scad-mode` with `lsp-mode`:

```elisp
;; https://github.com/emacs-lsp/lsp-mode
(use-package lsp-mode
  :hook ((scad-mode . lsp)))
```

For `scad-ts-mode` and `scad-mode` with `lsp-mode`:

```elisp
;; https://github.com/emacs-lsp/lsp-mode
(use-package lsp-mode
  :hook ((scad-mode . lsp)
         (scad-ts-mode . lsp))
  :defines (make-lsp-client)
  :config
  (require 'lsp)
  (require 'lsp-openscad)
  (add-to-list 'lsp-language-id-configuration
               '(scad-ts-mode . "openscad"))
  (declare-function lsp-register-client "lsp-mode")
  (declare-function make-lsp-client "lsp-mode")
  (declare-function lsp-openscad-server-connection "lsp-openscad")
  (declare-function lsp-get "lsp-protocol")
  (declare-function lsp:set-server-capabilities-completion-provider?
                    "lsp-protocol")
  (declare-function lsp--set-configuration "lsp-protocol")
  (declare-function lsp-configuration-section "lsp-protocol")
  (lsp-register-client
   (make-lsp-client
    :new-connection (lsp-openscad-server-connection)
    :major-modes '(scad-ts-mode scad-mode)
    :priority -1
    :initialized-fn (lambda (workspace)
                      (let ((caps (lsp--workspace-server-capabilities
                                   workspace)))
                        (unless (lsp-get caps :completionProvider)
                          (lsp:set-server-capabilities-completion-provider?
                           caps t)))
                      (with-lsp-workspace workspace
                        (lsp--set-configuration
                         (lsp-configuration-section
                          "openscad"))))
    :server-id 'openscad)))
```

For `eglot`:

```elisp
(use-package eglot
  :hook (((scad-mode
           scad-ts-mode)
          .
          eglot-ensure))
  :config
  (add-to-list 'eglot-server-programs
               '(scad-ts-mode . ("openscad-lsp" "--stdio")))
  (add-to-list 'eglot-server-programs
               '(scad-mode . ("openscad-lsp" "--stdio"))))
```

## Usage

The server communicates over TCP socket (127.0.0.1:3245).

```
A language(LSP) server for OpenSCAD

Usage: openscad-lsp [OPTIONS]

Options:
  -p, --port <PORT>              [default: 3245]
      --ip <IP>                  [default: 127.0.0.1]
      --builtin <BUILTIN>        external builtin functions file path, if not set, the built-in file will be used [default: ]
      --stdio                    use stdio instead of tcp
      --include-default-params   include default params in auto-completion
      --depth <DEPTH>            maximum include depth to traverse (0 = unlimited) [default: 0]
      --indent <INDENT>          The indentation string used for that particular language. Defaults to "  " if not provided. Any string can be provided, but in most instances will be some whitespace: "  ", "    ", or "\t". [default: "  "]
      --query-file <QUERY_FILE>  The query file used for topiary formatting
  -h, --help                     Print help
  -V, --version                  Print version
```

To change the config at runtime, you can send notification `workspace/didChangeConfiguration`
(`search_paths` should use your platform path separator, e.g. `:` on Unix/macOS, `;` on Windows):

```jsonc
{
  "settings": {
    "openscad": {
      "search_paths": "/libs",
      "indent": "    ",
      "query_file": "path/to/my/openscad.scm",
      "default_param": true,
    },
  },
}
```
