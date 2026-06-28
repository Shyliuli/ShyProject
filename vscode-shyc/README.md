# ShyC VSCode Extension

This extension provides local editor support for ShyC:

- `.shyc` and `.shyh` language mode.
- `.shy` assembly language mode.
- TextMate syntax highlighting for ShyC extensions such as `impl`, `self`,
  `Type::method`, `asm!`, and `#![...]`.
- Diagnostics by running `shycc -S` in the current workspace.

## Development

```sh
cd vscode-shyc
npm install
npm run compile
```

Open this folder in VSCode and press `F5` to launch an Extension Development
Host.

When used from the ShyProject workspace, diagnostics default to:

```sh
cargo run -q -p shycc -- -S <file> -o <tmp>/check.shy
```

Set `shyc.shyccPath` if you want to use a prebuilt `shycc` binary instead.
