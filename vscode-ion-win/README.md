# ion-win Language Support

VS Code support for `.ion` scripts:

- syntax highlighting for Ion keywords, builtins, variables, strings, types, and operators
- bracket, quote, comment, and indentation behavior
- snippets for variables, control flow, functions, tables, and date formatting
- **Ion: Run Current Ion Script** in the Command Palette, editor menu, and play button

The runner saves the current file, then launches it with `ion-win.exe`. Set
`ion-win.executablePath` if the executable is not at
`target/release/ion-win.exe` in the current workspace.
