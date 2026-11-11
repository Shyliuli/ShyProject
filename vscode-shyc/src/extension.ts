import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let output: vscode.OutputChannel;

export async function activate(context: vscode.ExtensionContext) {
  output = vscode.window.createOutputChannel("ShyC Language Server");
  context.subscriptions.push(
    output,
    vscode.commands.registerCommand("shyc.showOutput", () => output.show()),
    vscode.commands.registerCommand("shyc.restartServer", async () => {
      await stopClient();
      await startClient(context);
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (!event.affectsConfiguration("shyc")) {
        return;
      }
      if (event.affectsConfiguration("shyc.lsp.path")) {
        await stopClient();
        await startClient(context);
      } else {
        await sendConfiguration();
      }
    }),
  );

  await startClient(context);
}

export async function deactivate(): Promise<void> {
  await stopClient();
}

async function startClient(context: vscode.ExtensionContext) {
  const server = serverCommand(context);
  if (!server) {
    const message =
      'ShyC language server was not found. Set "shyc.lsp.path" in VS Code settings.json, or install a package that includes server/bin/shyc-lsp.';
    output.appendLine(`failed to start: ${message}`);
    output.show(true);
    const openSettings = "Open Settings";
    void vscode.window.showErrorMessage(message, openSettings).then((choice) => {
      if (choice === openSettings) {
        void vscode.commands.executeCommand(
          "workbench.action.openSettings",
          "shyc.lsp.path",
        );
      }
    });
    return;
  }
  output.appendLine(`starting: ${server.command} ${server.args.join(" ")}`);

  const serverOptions: ServerOptions = {
    command: server.command,
    args: server.args,
    options: { cwd: server.cwd },
    transport: TransportKind.stdio,
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "shyc" }],
    initializationOptions: configuration(),
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.{shyc,shyh,h}"),
    },
    outputChannel: output,
  };

  client = new LanguageClient(
    "shyc",
    "ShyC Language Server",
    serverOptions,
    clientOptions,
  );
  try {
    await client.start();
  } catch (error) {
    client = undefined;
    const message = error instanceof Error ? error.message : String(error);
    output.appendLine(`failed to start: ${message}`);
    output.show(true);
    void vscode.window.showErrorMessage(`ShyC language server failed to start: ${message}`);
  }
}

async function stopClient() {
  if (!client) {
    return;
  }
  const running = client;
  client = undefined;
  try {
    await running.stop();
  } catch (error) {
    output.appendLine(`failed to stop: ${error instanceof Error ? error.message : String(error)}`);
  }
}

async function sendConfiguration() {
  if (!client) {
    return;
  }
  await client.sendNotification("workspace/didChangeConfiguration", {
    settings: { shyc: configuration() },
  });
}

function configuration() {
  const config = vscode.workspace.getConfiguration("shyc");
  return {
    libshyPath: config.get<string>("libshyPath", ""),
    shyccPath: config.get<string>("shyccPath", ""),
    includeDirs: config.get<string[]>("includeDirs", []),
    defines: config.get<string[]>("defines", []),
    diagnosticsEnabled: config.get<boolean>("diagnostics.enabled", true),
  };
}

function serverCommand(context: vscode.ExtensionContext): {
  command: string;
  args: string[];
  cwd: string | undefined;
} | undefined {
  const cwd = workspaceRoot();
  const configured = vscode.workspace
    .getConfiguration("shyc")
    .get<string>("lsp.path", "")
    .trim();
  if (configured) {
    return { command: configured, args: [], cwd };
  }

  const suffix = process.platform === "win32" ? ".exe" : "";
  const candidates: string[] = [];
  if (cwd) {
    candidates.push(
      path.join(cwd, "target", "bin", `shyc-lsp${suffix}`),
      path.join(cwd, "target", "debug", `shyc-lsp${suffix}`),
      path.join(cwd, "target", "release", `shyc-lsp${suffix}`),
    );
  }
  candidates.push(
    path.join(context.extensionPath, "server", "target", "release", `shyc-lsp${suffix}`),
    path.join(context.extensionPath, "server", "target", "debug", `shyc-lsp${suffix}`),
    path.join(context.extensionPath, "server", "bin", `shyc-lsp${suffix}`),
  );
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return { command: candidate, args: [], cwd };
    }
  }

  return undefined;
}

function workspaceRoot(): string | undefined {
  const editor = vscode.window.activeTextEditor;
  const folder = editor ? vscode.workspace.getWorkspaceFolder(editor.document.uri) : undefined;
  return folder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}
