# About

**Table of Contents**

> - [About](#about)
>   - [Features](#features)
>   - [IDE plugins](#ide-plugins)
>   - [Install](#install)
>   - [Build](#build)
>   - [Usage](#usage)

Fork of [dzhu/openscad-language-server](https://github.com/dzhu/openscad-language-server), focused on improving OpenSCAD editing for larger projects and day-to-day IDE workflows.

Main differences in this fork:

- workspace-scale symbol/rename support with identifier indexing and caching
- configurable include traversal depth for symbol resolution (`--depth`, where `0` means unlimited)
- improved completion behavior, including better deduplication and callable parameter suggestions
- corrected `Go to Definition` behavior for parameter defaults like `p = p` (RHS resolves to outer scope)
- refreshed builtin documentation and cleaner parser-based extraction of comment docs

Compatibility notes:

- Tested with VSCode on Mac and Windows.
- [VSCode extension](https://github.com/Leathong/openscad-support-vscode)
- Tested with lsp-mode on Emacs on Linux by [@Lenbok](https://github.com/Lenbok).

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
- hover and suggestion documentation from comments before function/module declarations

## IDE plugins

| IDE     | Plugin                                                                           | Note                           |
| ------- | -------------------------------------------------------------------------------- | ------------------------------ |
| Neovim  | [mason.nvim](https://github.com/williamboman/mason.nvim)                         | Only tested on Mac and Linux   |
| Neovim  | [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig)                       | Only tested on Mac and Linux   |
| VS Code | [openscad-language-support](https://github.com/Leathong/openscad-support-vscode) | Only tested on Mac and Windows |
| Emacs   | [lsp-bridge](https://github.com/manateelazycat/lsp-bridge)                       | Only tested on Mac and Linux   |

## Install

openscad-LSP is written in [Rust](https://rust-lang.org), in order to use it, you need to
install [Rust toolchain](https://www.rust-lang.org/learn/get-started).

```sh
make install-local
```

Equivalent direct command:

```sh
cargo install --path . --locked --force
```

## Build

```sh
cd openscad-LSP
cargo build --release
```

Cross-platform release archives via Makefile:

```sh
make build
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
