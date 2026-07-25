const vscode = require("vscode");
const fs = require("fs");
const path = require("path");

function resolveExecutable(workspaceFolder) {
    const configured = vscode.workspace
        .getConfiguration("ion-win", workspaceFolder?.uri)
        .get("executablePath", "")
        .trim();

    if (configured) {
        return configured.replace(
            /\$\{workspaceFolder\}/g,
            workspaceFolder ? workspaceFolder.uri.fsPath : ""
        );
    }

    if (workspaceFolder) {
        const local = path.join(
            workspaceFolder.uri.fsPath,
            "target",
            "release",
            "ion-win.exe"
        );
        if (fs.existsSync(local)) {
            return local;
        }
    }

    return "ion-win.exe";
}

async function runCurrentFile() {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document.languageId !== "ion-win") {
        vscode.window.showErrorMessage("Open an .ion file before running an Ion script.");
        return;
    }

    if (editor.document.isUntitled) {
        vscode.window.showErrorMessage("Save the Ion script before running it.");
        return;
    }

    if (editor.document.isDirty && !(await editor.document.save())) {
        return;
    }

    const document = editor.document;
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const executable = resolveExecutable(workspaceFolder);

    if (path.isAbsolute(executable) && !fs.existsSync(executable)) {
        const build = await vscode.window.showErrorMessage(
            `ion-win executable not found at ${executable}`,
            "Build Release"
        );
        if (build === "Build Release") {
            await vscode.commands.executeCommand(
                "workbench.action.tasks.runTask",
                "Build ion-win (release)"
            );
        }
        return;
    }

    const terminal = vscode.window.createTerminal({
        name: `ion-win: ${path.basename(document.uri.fsPath)}`,
        shellPath: executable,
        shellArgs: [document.uri.fsPath],
        cwd: workspaceFolder
            ? workspaceFolder.uri.fsPath
            : path.dirname(document.uri.fsPath)
    });
    terminal.show();
}

function activate(context) {
    context.subscriptions.push(
        vscode.commands.registerCommand("ion-win.runFile", runCurrentFile)
    );
}

function deactivate() {}

module.exports = { activate, deactivate };
