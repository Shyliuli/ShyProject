# ShyC VSCode Extension

Editor support for ShyC and ShyISA:

- `.shyc` and `.shyh` language mode, `.shy` assembly language mode.
- TextMate highlighting for the current ShyC surface (`fn`, `impl`, `trait`,
  `let`, generics, payload enums, `use`, `asm!`, attributes).
- A Rust language server that runs the real `shyc` frontend. Diagnostics,
  completion, go-to-definition, hover, document symbols, and semantic tokens
  therefore always agree with the compiler.

The server reuses `../shyc` as a library; it does not ship a second parser or a
second list of language rules.

## Layout

```text
vscode-shyc/
  server/       shyc-lsp, a Rust LSP server (depends on ../../shyc)
  src/          thin VS Code LanguageClient
  syntaxes/     TextMate grammars
```

## Build

```sh
cd vscode-shyc
npm install
npm run build        # cargo build server + tsc
```

Open this folder in VS Code and press `F5` to launch an Extension Development
Host.

When the extension starts it uses, in order:

1. `shyc.lsp.path`, if set;
2. `target/bin/shyc-lsp`, `target/debug/shyc-lsp`, or `target/release/shyc-lsp`
   in the workspace;
3. `server/target/{debug,release}/shyc-lsp` inside this extension;
4. `cargo run --quiet --manifest-path server/Cargo.toml`.

The first launch through `cargo run` compiles the server and the compiler, so
it can take a while. `npm run build:server` avoids that.

## Include directories

The server searches the file's ancestors for the usual ShyProject layouts:
`libshy/c`, `libshy`, `libshy/include`, `include`, and `user`. Add project
specific paths with `shyc.includeDirs`:

```json
{
  "shyc.includeDirs": ["projects/os/user", "projects/os/kernel"]
}
```

Add build defines the same way when a project uses them:

```json
{
  "shyc.defines": ["SHYOS_RISCV", "DEBUG=1"]
}
```

`shyc.diagnostics.enabled` turns compiler diagnostics off without disabling
the other editor features.
