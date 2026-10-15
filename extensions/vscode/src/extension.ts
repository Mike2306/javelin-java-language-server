// VS Code integration.
//
// This file starts `javelin serve` and forwards the user's settings. That is the
// whole extension. Anything that looks like Java knowledge belongs in the
// server, so VS Code and Zed cannot drift apart.
//
// Status: skeleton. Not packaged, not published, not built in CI.

import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const settings = vscode.workspace.getConfiguration("javelin");
  const command = settings.get<string>("serverPath", "javelin");

  const serverOptions: ServerOptions = {
    command,
    args: ["serve"],
    transport: TransportKind.stdio,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "java" }],
    // The server reads its configuration once, at initialize, and treats an
    // unknown or malformed key as "use the default" rather than failing.
    initializationOptions: {
      logging: { level: settings.get<string>("logging.level", "info") },
      index: {
        backgroundIndexing: settings.get<boolean>("index.backgroundIndexing", true),
      },
      format: {
        command: settings.get<string[]>("format.command", []),
        timeoutMs: settings.get<number>("format.timeoutMs", 10000),
      },
    },
  };

  client = new LanguageClient("javelin", "Javelin", serverOptions, clientOptions);
  await client.start();
  context.subscriptions.push({ dispose: () => void client?.stop() });
}

export async function deactivate(): Promise<void> {
  await client?.stop();
  client = undefined;
}
