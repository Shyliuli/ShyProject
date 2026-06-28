import * as cp from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

const SHYC_LANG = "shyc";
const SHYASM_LANG = "shyasm";
const builtinTypePattern =
  "\\b(void|_Bool|char|short|int|long|float|double|signed|unsigned|struct|union|enum|typedef|const|volatile|static|extern|inline|i8|i16|i32|i64|u8|u16|u32|u64|isize|usize|f32|f64|int8_t|int16_t|int32_t|int64_t|uint8_t|uint16_t|uint32_t|uint64_t|intptr_t|uintptr_t|size_t|ptrdiff_t)\\b";

const tokenTypes = [
  "keyword",
  "type",
  "function",
  "method",
  "variable",
  "macro",
  "comment",
  "string",
  "number",
  "operator",
  "property",
  "namespace",
];
const semanticLegend = new vscode.SemanticTokensLegend(tokenTypes, []);

let diagnostics: vscode.DiagnosticCollection;
const timers = new Map<string, NodeJS.Timeout>();

export function activate(context: vscode.ExtensionContext) {
  diagnostics = vscode.languages.createDiagnosticCollection("shyc");
  context.subscriptions.push(diagnostics);

  context.subscriptions.push(
    vscode.commands.registerCommand("shyc.checkFile", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.languageId !== SHYC_LANG) {
        vscode.window.showInformationMessage("Open a ShyC file first.");
        return;
      }
      await checkDocument(editor.document);
    }),
  );

  context.subscriptions.push(
    vscode.languages.registerDocumentSemanticTokensProvider(
      { language: SHYC_LANG },
      new ShySemanticTokensProvider(),
      semanticLegend,
    ),
    vscode.languages.registerDocumentSemanticTokensProvider(
      { language: SHYASM_LANG },
      new ShyAsmSemanticTokensProvider(),
      semanticLegend,
    ),
    vscode.languages.registerDefinitionProvider({ language: SHYC_LANG }, new ShyDefinitionProvider()),
    vscode.languages.registerCompletionItemProvider(
      { language: SHYC_LANG },
      new ShyCompletionProvider(),
      ".",
      ":",
    ),
  );

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.languageId === SHYC_LANG) {
        scheduleCheck(doc);
      }
    }),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (doc.languageId === SHYC_LANG) {
        void checkDocument(doc);
      }
    }),
    vscode.workspace.onDidChangeTextDocument((event) => {
      if (event.document.languageId === SHYC_LANG) {
        const cfg = vscode.workspace.getConfiguration("shyc", event.document.uri);
        if (!cfg.get<boolean>("diagnostics.onSaveOnly", false)) {
          scheduleCheck(event.document);
        }
      }
    }),
    vscode.workspace.onDidCloseTextDocument((doc) => diagnostics.delete(doc.uri)),
  );

  for (const doc of vscode.workspace.textDocuments) {
    if (doc.languageId === SHYC_LANG) {
      scheduleCheck(doc);
    }
  }
}

export function deactivate() {
  for (const timer of timers.values()) {
    clearTimeout(timer);
  }
  timers.clear();
}

function scheduleCheck(document: vscode.TextDocument) {
  const cfg = vscode.workspace.getConfiguration("shyc", document.uri);
  if (!cfg.get<boolean>("diagnostics.enabled", true)) {
    diagnostics.delete(document.uri);
    return;
  }

  const key = document.uri.toString();
  const old = timers.get(key);
  if (old) {
    clearTimeout(old);
  }

  timers.set(
    key,
    setTimeout(() => {
      timers.delete(key);
      void checkDocument(document);
    }, 350),
  );
}

async function checkDocument(document: vscode.TextDocument) {
  const cfg = vscode.workspace.getConfiguration("shyc", document.uri);
  if (!cfg.get<boolean>("diagnostics.enabled", true)) {
    diagnostics.delete(document.uri);
    return;
  }

  if (document.uri.scheme !== "file") {
    return;
  }

  const workspace = workspaceRoot(document.uri);
  if (!workspace) {
    diagnostics.set(document.uri, [
      diagnosticAtStart("ShyC diagnostics require an open workspace folder."),
    ]);
    return;
  }

  const tmpDir = await fs.promises.mkdtemp(path.join(os.tmpdir(), "shyc-vscode-"));
  const out = path.join(tmpDir, "check.shy");

  try {
    const input = await diagnosticInput(document, tmpDir);
    const result = await runShycc(input.fileName, out, workspace, cfg);
    diagnostics.set(document.uri, parseDiagnostics(document, result.stderr + result.stdout, input.aliases));
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    diagnostics.set(document.uri, [diagnosticAtStart(message)]);
  } finally {
    await fs.promises.rm(tmpDir, { force: true, recursive: true });
  }
}

async function diagnosticInput(
  document: vscode.TextDocument,
  tmpDir: string,
): Promise<{ fileName: string; aliases: string[] }> {
  const ext = path.extname(document.fileName);
  if (ext !== ".shyh" && ext !== ".h") {
    return { fileName: document.fileName, aliases: [document.fileName] };
  }

  const headerName = path.basename(document.fileName);
  const headerPath = path.join(tmpDir, headerName);
  const wrapperPath = path.join(tmpDir, "check.shyc");
  await fs.promises.writeFile(headerPath, document.getText());
  await fs.promises.writeFile(wrapperPath, `#include "${headerName}"\n`);
  return { fileName: wrapperPath, aliases: [document.fileName, headerPath] };
}

function workspaceRoot(uri: vscode.Uri): string | undefined {
  const folder = vscode.workspace.getWorkspaceFolder(uri);
  return folder?.uri.fsPath;
}

function runShycc(
  input: string,
  output: string,
  cwd: string,
  cfg: vscode.WorkspaceConfiguration,
): Promise<{ stdout: string; stderr: string; code: number | null }> {
  const configured = cfg.get<string>("shycPath", "").trim();
  const command = configured || "cargo";
  const args = configured
    ? ["-S", input, "-o", output]
    : ["run", "-q", "-p", "shycc", "--", "-S", input, "-o", output];

  return new Promise((resolve, reject) => {
    const child = cp.spawn(command, args, { cwd });
    let stdout = "";
    let stderr = "";

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", (code) => {
      resolve({ stdout, stderr, code });
    });
  });
}

function parseDiagnostics(document: vscode.TextDocument, output: string, aliases: string[]): vscode.Diagnostic[] {
  if (!output.trim()) {
    return [];
  }

  const lines = output.split(/\r?\n/);
  const out: vscode.Diagnostic[] = [];

  for (let i = 0; i < lines.length; i++) {
    const match = /^(.*):(\d+):\s*(.*)$/.exec(lines[i]);
    if (!match) {
      continue;
    }

    const file = path.resolve(match[1]);
    if (!aliases.map((p) => path.resolve(p)).includes(file)) {
      continue;
    }

    const line = Math.max(0, Number(match[2]) - 1);
    let message = match[3].trim();
    let column = 0;

    if (i + 1 < lines.length && /^\s*\^/.test(lines[i + 1])) {
      const caret = lines[i + 1];
      column = Math.max(0, caret.indexOf("^"));
      const caretMessage = caret.slice(column + 1).trim();
      if (caretMessage) {
        message = caretMessage;
      }
    }

    const textLine = document.lineAt(Math.min(line, document.lineCount - 1));
    const start = Math.min(column, textLine.text.length);
    const end = Math.min(Math.max(start + 1, firstTokenEnd(textLine.text, start)), textLine.text.length);
    const range = new vscode.Range(line, start, line, end);
    out.push(new vscode.Diagnostic(range, message || "ShyC compile error", vscode.DiagnosticSeverity.Error));
  }

  if (out.length === 0 && output.trim()) {
    out.push(diagnosticAtStart(output.trim()));
  }

  return out;
}

function firstTokenEnd(line: string, start: number): number {
  let i = start;
  while (i < line.length && /\s/.test(line[i])) {
    i++;
  }
  while (i < line.length && /[A-Za-z0-9_]/.test(line[i])) {
    i++;
  }
  return i > start ? i : start + 1;
}

function diagnosticAtStart(message: string): vscode.Diagnostic {
  return new vscode.Diagnostic(new vscode.Range(0, 0, 0, 1), message, vscode.DiagnosticSeverity.Error);
}

class ShySemanticTokensProvider implements vscode.DocumentSemanticTokensProvider {
  provideDocumentSemanticTokens(document: vscode.TextDocument): vscode.ProviderResult<vscode.SemanticTokens> {
    const builder = new vscode.SemanticTokensBuilder(semanticLegend);
    const index: ShyIndex = {
      types: new Set(),
      structs: new Map(),
      methods: new Map(),
      functions: [],
      variables: [],
    };
    scanTextIntoIndex(document.getText(), index, true);

    for (let lineNo = 0; lineNo < document.lineCount; lineNo++) {
      const line = document.lineAt(lineNo).text;
      const spans: TokenSpan[] = [];
      const codeLimit = addLineComment(line, spans);
      const isPreprocessor = /^\s*#/.test(line);
      addRegexTokens(line, spans, /"([^"\\]|\\.)*"/g, "string", codeLimit);
      addRegexTokens(line, spans, /'([^'\\]|\\.)*'/g, "string", codeLimit);
      addRegexTokens(line, spans, /\b(0[xX][0-9a-fA-F]+|[0-9]+(\.[0-9]+)?([eEpP][+-]?[0-9]+)?[uUlLfF]*)\b/g, "number", codeLimit);
      addRegexTokens(line, spans, /^\s*#\s*(include|define|undef|if|ifdef|ifndef|elif|else|endif|pragma|error|warning)\b/g, "macro", codeLimit);
      if (!isPreprocessor) {
        addRegexTokens(line, spans, /\b(return|if|else|for|while|do|switch|case|default|break|continue|goto|sizeof|typeof|_Alignof|_Alignas)\b/g, "keyword", codeLimit);
        addRegexTokens(line, spans, /\b(impl|self)\b/g, "keyword", codeLimit);
        addRegexTokens(line, spans, new RegExp(builtinTypePattern, "g"), "type", codeLimit);
        addKnownTypeTokens(line, spans, index.types, codeLimit);
        addRegexTokens(line, spans, /\b([A-Za-z_][A-Za-z0-9_]*)\b(?=\s*::)/g, "type", codeLimit);
        addRegexTokens(line, spans, /(?<=::\s*)\b[A-Za-z_][A-Za-z0-9_]*\b/g, "method", codeLimit);
        addRegexTokens(line, spans, /(?<=\.\s*)\b[A-Za-z_][A-Za-z0-9_]*\b(?=\s*\()/g, "method", codeLimit);
        addRegexTokens(line, spans, /(?<=\.\s*)\b[A-Za-z_][A-Za-z0-9_]*\b(?!\s*\()/g, "property", codeLimit);
        addDeclarationVariableToken(line, spans, lineNo, codeLimit);
        addKnownVariableTokens(line, spans, index.variables, codeLimit);
        addRegexTokens(line, spans, /\basm!/g, "macro", codeLimit);
        addRegexTokens(line, spans, /#!\[(no_main|mem\([^\]]*\)|stack\([^\]]*\))\]/g, "macro", codeLimit);
        addRegexTokens(line, spans, /\b[A-Za-z_][A-Za-z0-9_]*\b(?=\s*\()/g, "function", codeLimit);
        addRegexTokens(line, spans, /(::|->|==|!=|<=|>=|\+=|-=|\*=|\/=|%=|&&|\|\||[+\-*\/%=!<>.&|^~])/g, "operator", codeLimit);
      }

      emitLineTokens(builder, lineNo, spans);
    }

    return builder.build();
  }
}

class ShyAsmSemanticTokensProvider implements vscode.DocumentSemanticTokensProvider {
  provideDocumentSemanticTokens(document: vscode.TextDocument): vscode.ProviderResult<vscode.SemanticTokens> {
    const builder = new vscode.SemanticTokensBuilder(semanticLegend);

    for (let lineNo = 0; lineNo < document.lineCount; lineNo++) {
      const line = document.lineAt(lineNo).text;
      const spans: TokenSpan[] = [];
      const codeLimit = addLineComment(line, spans);
      addRegexTokens(line, spans, /"([^"\\]|\\.)*"/g, "string", codeLimit);
      addRegexTokens(line, spans, /\b(___DEFINE___|___DATA___|___CODE___)\b/g, "keyword", codeLimit);
      addRegexTokens(line, spans, /^\s*\.(section|symbol)\b/g, "macro", codeLimit);
      addRegexTokens(line, spans, /#!\[(mem|stack)\([^\]]*\)\]/g, "macro", codeLimit);
      addRegexTokens(line, spans, /^\s*[A-Za-z_.$][A-Za-z0-9_.$]*(?=:)/g, "property", codeLimit);
      addRegexTokens(line, spans, /\b([1-9]x|ax|bx|cx|dx|sp|bp)\b/g, "variable", codeLimit);
      addRegexTokens(line, spans, /\b(0[xX][0-9a-fA-F]+|[01]+b|[0-9]+)\b/g, "number", codeLimit);
      addRegexTokens(line, spans, /^\s*[a-z][a-z0-9]*/g, "function", codeLimit);
      emitLineTokens(builder, lineNo, spans);
    }

    return builder.build();
  }
}

class ShyDefinitionProvider implements vscode.DefinitionProvider {
  async provideDefinition(
    document: vscode.TextDocument,
    position: vscode.Position,
  ): Promise<vscode.Definition | undefined> {
    const range = document.getWordRangeAtPosition(position, /[A-Za-z_][A-Za-z0-9_]*/);
    if (!range) {
      return undefined;
    }

    const word = document.getText(range);
    const line = document.lineAt(position.line).text;
    const before = line.slice(0, range.start.character);
    const after = line.slice(range.end.character);

    let query: DefinitionQuery;
    const scopeMatch = /([A-Za-z_][A-Za-z0-9_]*)\s*::\s*$/.exec(before);
    if (scopeMatch) {
      query = { kind: "method", name: word, typeName: scopeMatch[1] };
    } else if (/::\s*$/.test(after)) {
      query = { kind: "type", name: word };
    } else {
      query = { kind: "any", name: word };
    }

    const locations = await findDefinitions(document.uri, query);
    return locations.length ? locations : undefined;
  }
}

class ShyCompletionProvider implements vscode.CompletionItemProvider {
  async provideCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position,
  ): Promise<vscode.CompletionItem[] | undefined> {
    const prefix = document.lineAt(position.line).text.slice(0, position.character);
    const index = await buildShyIndex(document);
    const items: vscode.CompletionItem[] = [];

    const staticMatch = /([A-Za-z_][A-Za-z0-9_]*)\s*::\s*(?:[A-Za-z_][A-Za-z0-9_]*)?$/.exec(prefix);
    if (staticMatch) {
      for (const method of index.methods.get(staticMatch[1]) ?? []) {
        items.push(completionForMethod(method, true));
      }
      return dedupeCompletions(items);
    }

    const memberMatch = /([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*(?:[A-Za-z_][A-Za-z0-9_]*)?$/.exec(prefix);
    if (memberMatch) {
      const receiver = memberMatch[1];
      const typeName = inferVariableType(document, position, receiver, index);
      if (!typeName) {
        return undefined;
      }

      for (const field of index.structs.get(typeName)?.fields ?? []) {
        items.push(completionForField(field));
      }
      for (const method of index.methods.get(typeName) ?? []) {
        if (method.params.length > 0 && /\bself\b/.test(method.params[0])) {
          items.push(completionForMethod(method, false));
        }
      }
      return dedupeCompletions(items);
    }

    for (const typeName of index.types) {
      const item = new vscode.CompletionItem(typeName, vscode.CompletionItemKind.Class);
      item.detail = "type";
      items.push(item);
    }
    for (const variable of visibleVariables(document, position, index)) {
      const item = new vscode.CompletionItem(variable.name, vscode.CompletionItemKind.Variable);
      item.detail = variable.type;
      items.push(item);
    }
    for (const fn of index.functions) {
      items.push(completionForFunction(fn));
    }

    return dedupeCompletions(items);
  }
}

type TokenSpan = {
  start: number;
  length: number;
  type: string;
};

type FieldInfo = {
  name: string;
  type: string;
};

type FunctionInfo = {
  name: string;
  returnType: string;
  params: string[];
};

type MethodInfo = FunctionInfo & {
  typeName: string;
};

type VariableInfo = {
  name: string;
  type: string;
  line: number;
};

type StructInfo = {
  name: string;
  fields: FieldInfo[];
};

type ShyIndex = {
  types: Set<string>;
  structs: Map<string, StructInfo>;
  methods: Map<string, MethodInfo[]>;
  functions: FunctionInfo[];
  variables: VariableInfo[];
};

type DefinitionQuery =
  | { kind: "any"; name: string }
  | { kind: "type"; name: string }
  | { kind: "method"; name: string; typeName?: string };

function addLineComment(line: string, spans: TokenSpan[]): number {
  const idx = line.indexOf("//");
  if (idx >= 0) {
    spans.push({ start: idx, length: line.length - idx, type: "comment" });
    return idx;
  }
  return line.length;
}

function addRegexTokens(line: string, spans: TokenSpan[], regex: RegExp, type: string, limit: number) {
  regex.lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = regex.exec(line)) !== null) {
    const start = match.index;
    const text = match[0];
    if (start >= limit || text.length === 0) {
      continue;
    }
    spans.push({ start, length: Math.min(text.length, limit - start), type });
  }
}

function addKnownTypeTokens(line: string, spans: TokenSpan[], types: Set<string>, limit: number) {
  for (const name of types) {
    if (isBuiltinKeyword(name)) {
      continue;
    }
    const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    addRegexTokens(line, spans, new RegExp(`\\b${escaped}\\b`, "g"), "type", limit);
  }
}

function addKnownVariableTokens(line: string, spans: TokenSpan[], variables: VariableInfo[], limit: number) {
  const names = new Set(variables.map((variable) => variable.name));
  for (const name of names) {
    if (isBuiltinKeyword(name)) {
      continue;
    }
    const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    addRegexTokens(line, spans, new RegExp(`\\b${escaped}\\b`, "g"), "variable", limit);
  }
}

function addDeclarationVariableToken(line: string, spans: TokenSpan[], lineNo: number, limit: number) {
  const variable = parseVariableDecl(stripLineComment(line).trim(), lineNo);
  if (!variable) {
    return;
  }
  const start = line.lastIndexOf(variable.name);
  if (start >= 0 && start < limit) {
    spans.push({ start, length: Math.min(variable.name.length, limit - start), type: "variable" });
  }
}

function emitLineTokens(builder: vscode.SemanticTokensBuilder, lineNo: number, spans: TokenSpan[]) {
  spans.sort((a, b) => a.start - b.start || b.length - a.length);

  let end = 0;
  for (const span of spans) {
    if (span.start < end || span.length <= 0) {
      continue;
    }
    const idx = tokenTypes.indexOf(span.type);
    if (idx < 0) {
      continue;
    }
    builder.push(lineNo, span.start, span.length, idx, 0);
    end = span.start + span.length;
  }
}

async function buildShyIndex(document: vscode.TextDocument): Promise<ShyIndex> {
  const index: ShyIndex = {
    types: new Set(),
    structs: new Map(),
    methods: new Map(),
    functions: [],
    variables: [],
  };

  scanTextIntoIndex(document.getText(), index, true);

  const files = await vscode.workspace.findFiles(
    "**/*.{shyc,shyh,c,h}",
    "**/{target,node_modules,third_party/chibicc/test,vscode-shyc/node_modules}/**",
    300,
  );

  for (const uri of files) {
    if (uri.toString() === document.uri.toString()) {
      continue;
    }
    const text = await readWorkspaceText(uri);
    if (text !== undefined) {
      scanTextIntoIndex(text, index, false);
    }
  }

  return index;
}

function scanTextIntoIndex(text: string, index: ShyIndex, collectVariables: boolean) {
  const lines = text.split(/\r?\n/);
  let currentStruct: StructInfo | undefined;
  let currentImpl: string | undefined;
  let braceDepth = 0;

  for (let i = 0; i < lines.length; i++) {
    const line = stripLineComment(lines[i]);
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }

    if (currentStruct) {
      braceDepth += countChar(line, "{") - countChar(line, "}");
      const field = parseVariableDecl(trimmed, i);
      if (field) {
        currentStruct.fields.push({ name: field.name, type: field.type });
      }
      if (braceDepth <= 0) {
        currentStruct = undefined;
      }
      continue;
    }

    if (currentImpl) {
      braceDepth += countChar(line, "{") - countChar(line, "}");
      const fn = parseFunctionSignature(trimmed);
      if (fn) {
        addMethod(index, currentImpl, fn);
        if (collectVariables) {
          addParamVariables(index, fn, i);
        }
      } else if (collectVariables) {
        const variable = parseVariableDecl(trimmed, i);
        if (variable) {
          index.variables.push(variable);
        }
      }
      if (braceDepth <= 0) {
        currentImpl = undefined;
      }
      continue;
    }

    const typedefStruct = /^\s*typedef\s+struct\s+([A-Za-z_][A-Za-z0-9_]*)\s+([A-Za-z_][A-Za-z0-9_]*)\s*;/.exec(line);
    if (typedefStruct) {
      index.types.add(typedefStruct[1]);
      index.types.add(typedefStruct[2]);
      continue;
    }

    const structStart = /^\s*(?:typedef\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)?\s*\{/.exec(line);
    if (structStart) {
      const name = structStart[1];
      if (name) {
        index.types.add(name);
        currentStruct = index.structs.get(name) ?? { name, fields: [] };
        index.structs.set(name, currentStruct);
      }
      braceDepth = countChar(line, "{") - countChar(line, "}");
      if (braceDepth <= 0) {
        currentStruct = undefined;
      }
      continue;
    }

    const taggedStruct = /^\s*struct\s+([A-Za-z_][A-Za-z0-9_]*)\s*;/.exec(line);
    if (taggedStruct) {
      index.types.add(taggedStruct[1]);
      continue;
    }

    const typedef = /^\s*typedef\b.*\b([A-Za-z_][A-Za-z0-9_]*)\s*;/.exec(line);
    if (typedef) {
      index.types.add(typedef[1]);
      continue;
    }

    const implStart = /^\s*impl\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{/.exec(line);
    if (implStart) {
      currentImpl = implStart[1];
      index.types.add(currentImpl);
      braceDepth = countChar(line, "{") - countChar(line, "}");
      if (braceDepth <= 0) {
        currentImpl = undefined;
      }
      continue;
    }

    const fn = parseFunctionSignature(trimmed);
    if (fn) {
      index.functions.push(fn);
      if (collectVariables) {
        addParamVariables(index, fn, i);
      }
      continue;
    }

    if (collectVariables) {
      const variable = parseVariableDecl(trimmed, i);
      if (variable) {
        index.variables.push(variable);
      }
    }
  }
}

function parseFunctionSignature(line: string): FunctionInfo | undefined {
  if (!/[);{]\s*$/.test(line) || !line.includes("(")) {
    return undefined;
  }
  if (/^\s*(if|for|while|switch|return|sizeof)\b/.test(line)) {
    return undefined;
  }

  const header = line.replace(/\{.*$/, "").replace(/;.*$/, "").trim();
  const match = /^(.*?)\s*\(([^()]*)\)$/.exec(header);
  if (!match) {
    return undefined;
  }
  const named = parseNamedDecl(match[1]);
  if (!named) {
    return undefined;
  }

  return {
    returnType: named.type,
    name: named.name,
    params: splitParams(match[2]),
  };
}

function parseVariableDecl(line: string, lineNo: number): VariableInfo | undefined {
  if (!line.endsWith(";") || /^\s*(return|break|continue|goto|case|default)\b/.test(line)) {
    return undefined;
  }

  const firstDecl = line.replace(/;.*/, "").split(",")[0].replace(/=.*/, "").replace(/\[[^\]]*\]/g, "").trim();
  if (firstDecl.includes("(") || firstDecl.includes(".") || firstDecl.includes("->")) {
    return undefined;
  }

  const named = parseNamedDecl(firstDecl);
  if (!named || isBuiltinKeyword(named.name)) {
    return undefined;
  }
  return { ...named, line: lineNo };
}

function parseNamedDecl(text: string): { name: string; type: string } | undefined {
  const match = /([A-Za-z_][A-Za-z0-9_]*)\s*$/.exec(text.trim());
  if (!match) {
    return undefined;
  }

  const name = match[1];
  const type = normalizeWhitespace(text.slice(0, match.index).trim());
  if (!type || isBuiltinKeyword(name)) {
    return undefined;
  }
  return { name, type };
}

function splitParams(text: string): string[] {
  const trimmed = text.trim();
  if (!trimmed || trimmed === "void") {
    return [];
  }
  return trimmed.split(",").map((param) => normalizeWhitespace(param.trim())).filter(Boolean);
}

function addMethod(index: ShyIndex, typeName: string, fn: FunctionInfo) {
  const method: MethodInfo = { ...fn, typeName };
  const methods = index.methods.get(typeName) ?? [];
  if (!methods.some((old) => old.name === method.name && old.params.join(",") === method.params.join(","))) {
    methods.push(method);
  }
  index.methods.set(typeName, methods);
}

function addParamVariables(index: ShyIndex, fn: FunctionInfo, lineNo: number) {
  for (const param of fn.params) {
    const named = parseNamedDecl(param);
    if (named && !isBuiltinKeyword(named.name)) {
      index.variables.push({ ...named, line: lineNo });
    }
  }
}

function inferVariableType(
  document: vscode.TextDocument,
  position: vscode.Position,
  name: string,
  index: ShyIndex,
): string | undefined {
  for (const variable of visibleVariables(document, position, index).reverse()) {
    if (variable.name === name) {
      return baseTypeName(variable.type);
    }
  }
  return undefined;
}

function visibleVariables(document: vscode.TextDocument, position: vscode.Position, index: ShyIndex): VariableInfo[] {
  const out = index.variables.filter((variable) => variable.line <= position.line);
  const params = currentFunctionParams(document, position);
  out.push(...params);
  return out;
}

function currentFunctionParams(document: vscode.TextDocument, position: vscode.Position): VariableInfo[] {
  for (let lineNo = position.line; lineNo >= 0; lineNo--) {
    const line = stripLineComment(document.lineAt(lineNo).text).trim();
    const fn = parseFunctionSignature(line);
    if (!fn || !line.includes("{")) {
      continue;
    }

    return fn.params
      .map((param) => parseNamedDecl(param))
      .filter((param): param is { name: string; type: string } => param !== undefined)
      .map((param) => ({ ...param, line: lineNo }));
  }
  return [];
}

function completionForField(field: FieldInfo): vscode.CompletionItem {
  const item = new vscode.CompletionItem(field.name, vscode.CompletionItemKind.Field);
  item.detail = field.type;
  return item;
}

function completionForFunction(fn: FunctionInfo): vscode.CompletionItem {
  const item = new vscode.CompletionItem(fn.name, vscode.CompletionItemKind.Function);
  item.detail = signatureOf(fn);
  item.insertText = snippetCall(fn.name, fn.params);
  return item;
}

function completionForMethod(method: MethodInfo, staticCall: boolean): vscode.CompletionItem {
  const item = new vscode.CompletionItem(method.name, vscode.CompletionItemKind.Method);
  item.detail = signatureOf(method);
  item.documentation = new vscode.MarkdownString(`\`\`\`c\n${signatureOf(method)}\n\`\`\``);
  item.insertText = snippetCall(method.name, staticCall ? method.params : method.params.slice(1));
  return item;
}

function snippetCall(name: string, params: string[]): vscode.SnippetString {
  const args = params
    .map((param, idx) => `\${${idx + 1}:${paramName(param) ?? `arg${idx + 1}`}}`)
    .join(", ");
  return new vscode.SnippetString(`${name}(${args})`);
}

function paramName(param: string): string | undefined {
  return parseNamedDecl(param)?.name;
}

function signatureOf(fn: FunctionInfo): string {
  return `${fn.returnType} ${fn.name}(${fn.params.join(", ")})`;
}

function dedupeCompletions(items: vscode.CompletionItem[]): vscode.CompletionItem[] {
  const seen = new Set<string>();
  const out: vscode.CompletionItem[] = [];
  for (const item of items) {
    const key = `${item.kind}:${item.label}`;
    if (seen.has(key)) {
      continue;
    }
    seen.add(key);
    out.push(item);
  }
  return out;
}

function baseTypeName(type: string): string {
  return normalizeWhitespace(type)
    .replace(/\*/g, " ")
    .replace(/\b(const|volatile|static|extern|inline|signed|unsigned|long|short)\b/g, " ")
    .replace(/\b(struct|union|enum)\b/g, " ")
    .trim()
    .split(/\s+/)
    .pop() ?? "";
}

function stripLineComment(line: string): string {
  return line.replace(/\/\/.*$/, "");
}

function normalizeWhitespace(text: string): string {
  return text.replace(/\s+/g, " ").replace(/\s+\*/g, " *").replace(/\*\s+/g, "* ").trim();
}

function isBuiltinKeyword(name: string): boolean {
  return /^(void|char|short|int|long|float|double|signed|unsigned|const|volatile|static|extern|inline|struct|union|enum|return|if|else|for|while|do|switch|case|default|i8|i16|i32|i64|u8|u16|u32|u64|isize|usize|f32|f64|int8_t|int16_t|int32_t|int64_t|uint8_t|uint16_t|uint32_t|uint64_t|intptr_t|uintptr_t|size_t|ptrdiff_t)$/.test(name);
}

async function findDefinitions(origin: vscode.Uri, query: DefinitionQuery): Promise<vscode.Location[]> {
  const files = await vscode.workspace.findFiles(
    "**/*.{shyc,shyh,c,h}",
    "**/{target,node_modules,third_party/chibicc/test}/**",
    300,
  );
  const locations: vscode.Location[] = [];

  for (const uri of files) {
    const text = await readWorkspaceText(uri);
    if (text === undefined) {
      continue;
    }
    locations.push(...findDefinitionsInText(uri, text, query));
  }

  return locations.filter((loc) => loc.uri.toString() !== origin.toString() || loc.range.start.line >= 0);
}

async function readWorkspaceText(uri: vscode.Uri): Promise<string | undefined> {
  const open = vscode.workspace.textDocuments.find((doc) => doc.uri.toString() === uri.toString());
  if (open) {
    return open.getText();
  }

  try {
    return Buffer.from(await vscode.workspace.fs.readFile(uri)).toString("utf8");
  } catch {
    return undefined;
  }
}

function findDefinitionsInText(uri: vscode.Uri, text: string, query: DefinitionQuery): vscode.Location[] {
  const locations: vscode.Location[] = [];
  const lines = text.split(/\r?\n/);
  let currentImpl: string | undefined;
  let braceDepth = 0;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const impl = /^\s*impl\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{/.exec(line);
    if (impl) {
      currentImpl = impl[1];
      braceDepth = 1;
      addDefinitionIfMatch(locations, uri, i, impl.index + line.indexOf(impl[1]), impl[1], { kind: "type", name: impl[1] }, query);
      continue;
    }

    if (currentImpl) {
      braceDepth += countChar(line, "{") - countChar(line, "}");
      const method = /^\s*(?:[A-Za-z_][A-Za-z0-9_]*|\w+\s*\*|\w+\s+)+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*\)\s*\{?/.exec(line);
      if (method && (query.kind === "any" || query.kind === "method") &&
          method[1] === query.name &&
          (query.kind !== "method" || !query.typeName || query.typeName === currentImpl)) {
        locations.push(locationFor(uri, i, line.indexOf(method[1]), method[1].length));
      }
      if (braceDepth <= 0) {
        currentImpl = undefined;
      }
      continue;
    }

    const typePatterns = [
      /\btypedef\s+struct\s+([A-Za-z_][A-Za-z0-9_]*)\b/,
      /\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)\b/,
      /\btypedef\b.*\b([A-Za-z_][A-Za-z0-9_]*)\s*;/,
    ];
    if (query.kind === "any" || query.kind === "type") {
      for (const pattern of typePatterns) {
        const match = pattern.exec(line);
        if (match && match[1] === query.name) {
          locations.push(locationFor(uri, i, line.indexOf(match[1]), match[1].length));
          break;
        }
      }
    }

    if (query.kind === "any") {
      const fn = /^\s*(?:[A-Za-z_][A-Za-z0-9_]*|\w+\s*\*|\w+\s+)+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*\)\s*\{/.exec(line);
      if (fn && fn[1] === query.name) {
        locations.push(locationFor(uri, i, line.indexOf(fn[1]), fn[1].length));
      }
    }
  }

  return locations;
}

function addDefinitionIfMatch(
  locations: vscode.Location[],
  uri: vscode.Uri,
  line: number,
  col: number,
  text: string,
  candidate: DefinitionQuery,
  query: DefinitionQuery,
) {
  if ((query.kind === "any" || query.kind === candidate.kind) && query.name === candidate.name) {
    locations.push(locationFor(uri, line, col, text.length));
  }
}

function locationFor(uri: vscode.Uri, line: number, col: number, len: number): vscode.Location {
  return new vscode.Location(uri, new vscode.Range(line, Math.max(0, col), line, Math.max(0, col) + len));
}

function countChar(s: string, ch: string): number {
  let n = 0;
  for (const c of s) {
    if (c === ch) {
      n++;
    }
  }
  return n;
}
