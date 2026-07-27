import {
  access,
  copyFile,
  mkdir,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import {
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
} from "node:path";

type NavigationItem = {
  title: string;
  source: string;
  path: string;
};

type NavigationSection = {
  title: string;
  items: NavigationItem[];
};

type RenderedPage = NavigationItem & {
  cleanMarkdown: string;
  outputPath: string;
};

const repositoryRoot = join(import.meta.dir, "..");
const contentRoot = join(repositoryRoot, "docs", "content", "docs");
const staticRoot = join(repositoryRoot, "docs", "static");
const generatedDocsRoot = join(staticRoot, "docs");
const llmsSource = join(repositoryRoot, "llms.txt");
const llmsOutput = join(staticRoot, "llms.txt");
const llmsFullOutput = join(staticRoot, "llms-full.txt");
const navigationPath = join(repositoryRoot, "docs", "data", "navigation.json");
const workspaceManifestPath = join(repositoryRoot, "Cargo.toml");
const exampleGuidePath = join(
  repositoryRoot,
  "examples",
  "rust-embedding",
  "README.md",
);
const baseUrl = "https://nybl-lang.com";
const rawRepositoryUrl =
  "https://raw.githubusercontent.com/stevepryde/nybl-lang/main/";

function assertWithin(root: string, candidate: string, label: string): void {
  const relativePath = relative(root, candidate);
  if (
    relativePath === "" ||
    relativePath === ".." ||
    relativePath.startsWith("../") ||
    relativePath.startsWith("..\\") ||
    isAbsolute(relativePath)
  ) {
    throw new Error(`${label} escapes its owned root: ${candidate}`);
  }
}

function validateNavigationSource(source: string): string {
  if (
    source.startsWith("/") ||
    source.includes("\\") ||
    !source.endsWith(".md") ||
    source.split("/").some((segment) => segment === "." || segment === "..")
  ) {
    throw new Error(`Invalid documentation source path: ${source}`);
  }

  const sourcePath = resolve(contentRoot, source);
  assertWithin(contentRoot, sourcePath, "Documentation source");
  return sourcePath;
}

function validateNavigationPath(path: string): string {
  if (
    !path.startsWith("/docs/") ||
    !path.endsWith("/") ||
    path.includes("\\") ||
    path.split("/").some((segment) => segment === "." || segment === "..")
  ) {
    throw new Error(`Invalid documentation route: ${path}`);
  }

  const outputPath = resolve(staticRoot, `.${path}`, "index.html.md");
  assertWithin(generatedDocsRoot, outputPath, "Generated documentation route");
  return outputPath;
}

function stripFrontMatter(markdown: string, source: string): string {
  if (!markdown.startsWith("+++\n")) {
    throw new Error(`${source} does not start with Zola TOML front matter`);
  }

  const closingDelimiter = markdown.indexOf("\n+++\n", 4);
  if (closingDelimiter === -1) {
    throw new Error(`${source} has unterminated Zola TOML front matter`);
  }

  return markdown.slice(closingDelimiter + 5).trim();
}

function markdownUrl(path: string): string {
  if (!path.startsWith("/") || !path.endsWith("/")) {
    throw new Error(`Documentation path must start and end with "/": ${path}`);
  }
  return `${baseUrl}${path}index.html.md`;
}

function rewriteDocumentationLinks(markdown: string): string {
  return markdown.replace(
    /\]\((\/docs\/[^)\s]*?\/)(#[^)\s]+)?\)/g,
    (_match, path: string, fragment: string | undefined) =>
      `](${markdownUrl(path)}${fragment ?? ""})`,
  );
}

function markdownLinks(markdown: string): string[] {
  return [...markdown.matchAll(/\[[^\]]+\]\((https?:\/\/[^)]+)\)/g)]
    .map((match) => match[1])
    .filter((url): url is string => url !== undefined);
}

function withoutFragment(url: string): string {
  const parsed = new URL(url);
  parsed.hash = "";
  return parsed.toString();
}

function validateGeneratedLinks(
  markdown: string,
  generatedUrls: Set<string>,
  source: string,
): void {
  for (const url of markdownLinks(markdown)) {
    const target = withoutFragment(url);
    if (
      target.startsWith(`${baseUrl}/docs/`) &&
      target.endsWith(".md") &&
      !generatedUrls.has(target)
    ) {
      throw new Error(`${source} links to a Markdown page that was not generated: ${url}`);
    }
  }
}

async function validateRepositoryLinks(markdown: string): Promise<void> {
  for (const url of markdownLinks(markdown)) {
    if (!url.startsWith(rawRepositoryUrl)) {
      continue;
    }

    const relativePath = decodeURIComponent(
      withoutFragment(url).slice(rawRepositoryUrl.length),
    );
    const candidate = resolve(repositoryRoot, relativePath);
    assertWithin(repositoryRoot, candidate, "Raw repository link");
    await access(candidate).catch(() => {
      throw new Error(`llms.txt links to a missing repository file: ${url}`);
    });
  }
}

function validateLlmsIndex(
  llmsIndex: string,
  generatedUrls: Set<string>,
  releaseLine: string,
  rustVersion: string,
): void {
  const lines = llmsIndex.split("\n");
  if (!lines[0]?.startsWith("# ")) {
    throw new Error("llms.txt must begin with a single H1 project name");
  }

  const h1Count = lines.filter((line) => line.startsWith("# ")).length;
  if (h1Count !== 1) {
    throw new Error(`llms.txt must contain exactly one H1; found ${h1Count}`);
  }

  const firstContentLine = lines.findIndex((line, index) => index > 0 && line !== "");
  if (firstContentLine === -1 || !lines[firstContentLine]?.startsWith("> ")) {
    throw new Error("llms.txt must place its blockquote summary immediately after the H1");
  }

  const sectionIndexes = lines
    .map((line, index) => (line.startsWith("## ") ? index : -1))
    .filter((index) => index >= 0);
  if (sectionIndexes.length === 0) {
    throw new Error("llms.txt must include at least one H2 file-list section");
  }

  const firstSection = sectionIndexes[0]!;
  if (lines.slice(1, firstSection).some((line) => line.startsWith("#"))) {
    throw new Error("llms.txt preamble may not contain headings");
  }

  const sectionNames = new Set<string>();
  for (const [position, start] of sectionIndexes.entries()) {
    const name = lines[start]!.slice(3);
    if (sectionNames.has(name)) {
      throw new Error(`llms.txt contains a duplicate section: ${name}`);
    }
    if (name === "Optional" && position !== sectionIndexes.length - 1) {
      throw new Error("The llms.txt Optional section must be last");
    }
    sectionNames.add(name);

    const end = sectionIndexes[position + 1] ?? lines.length;
    const entries = lines.slice(start + 1, end).filter((line) => line !== "");
    if (entries.length === 0) {
      throw new Error(`llms.txt section has no file links: ${name}`);
    }
    for (const entry of entries) {
      if (!/^- \[[^\]]+\]\(https?:\/\/[^)]+\)(?:: .+)?$/.test(entry)) {
        throw new Error(`Invalid llms.txt file-list entry in "${name}": ${entry}`);
      }
    }
  }

  const metadata = `describes Nybl ${releaseLine} and requires Rust ${rustVersion} or newer`;
  if (!llmsIndex.includes(metadata)) {
    throw new Error(`llms.txt release metadata must include: ${metadata}`);
  }

  validateGeneratedLinks(llmsIndex, generatedUrls, "llms.txt");
}

function workspaceMetadata(manifest: string): {
  releaseLine: string;
  rustVersion: string;
} {
  const section = manifest.match(
    /\[workspace\.package\]\s*\n([\s\S]*?)(?=\n\[|$)/,
  )?.[1];
  const version = section?.match(/^version = "([^"]+)"$/m)?.[1];
  const rustVersion = section?.match(/^rust-version = "([^"]+)"$/m)?.[1];
  if (!version || !rustVersion) {
    throw new Error("Cargo.toml must declare workspace version and rust-version");
  }
  return {
    releaseLine: version.split(".").slice(0, 2).join("."),
    rustVersion,
  };
}

function validateExampleGuide(guide: string, releaseLine: string): void {
  const requiredSnippets = [
    `Nybl ${releaseLine} crates`,
    `nybl = { package = "nybl-lang", version = "${releaseLine}" }`,
    `nybl-vm = "${releaseLine}"`,
    `nybl-compile = "${releaseLine}"`,
  ];
  for (const snippet of requiredSnippets) {
    if (!guide.includes(snippet)) {
      throw new Error(`Rust embedding example guide must include: ${snippet}`);
    }
  }
}

const [navigationJson, workspaceManifest, llmsIndex, exampleGuide] =
  await Promise.all([
    readFile(navigationPath, "utf8"),
    readFile(workspaceManifestPath, "utf8"),
    readFile(llmsSource, "utf8"),
    readFile(exampleGuidePath, "utf8"),
  ]);
const navigation = JSON.parse(navigationJson) as NavigationSection[];
const { releaseLine, rustVersion } = workspaceMetadata(workspaceManifest);
validateExampleGuide(exampleGuide, releaseLine);
const pages = navigation.flatMap((section) =>
  section.items.map((item) => ({ ...item, section: section.title })),
);

const sources = new Set<string>();
const paths = new Set<string>();
for (const page of pages) {
  validateNavigationSource(page.source);
  validateNavigationPath(page.path);
  if (sources.has(page.source)) {
    throw new Error(`Duplicate documentation source in navigation: ${page.source}`);
  }
  if (paths.has(page.path)) {
    throw new Error(`Duplicate documentation path in navigation: ${page.path}`);
  }
  sources.add(page.source);
  paths.add(page.path);
}

const generatedUrls = new Set<string>();
for (const page of pages) {
  generatedUrls.add(markdownUrl(page.path));
}

const renderedPages: RenderedPage[] = [];
for (const page of pages) {
  const sourcePath = validateNavigationSource(page.source);
  const rawMarkdown = await readFile(sourcePath, "utf8");
  const cleanMarkdown = rewriteDocumentationLinks(
    stripFrontMatter(rawMarkdown, sourcePath),
  );
  const outputPath = validateNavigationPath(page.path);

  validateGeneratedLinks(cleanMarkdown, generatedUrls, page.source);
  renderedPages.push({ ...page, cleanMarkdown, outputPath });
}

validateLlmsIndex(llmsIndex, generatedUrls, releaseLine, rustVersion);
await validateRepositoryLinks(llmsIndex);

const llmsFull = [
  "# Nybl complete documentation",
  "",
  `> LLM-oriented Markdown export of the Nybl ${releaseLine} language, standard library, tools, and Rust embedding documentation.`,
  "",
  "This file is generated from the documentation sources selected by `docs/data/navigation.json`. For the concise index and integration rules, see https://nybl-lang.com/llms.txt.",
  "",
  ...renderedPages.flatMap((page) => [
    "---",
    "",
    `Source: ${baseUrl}${page.path}\n\n${page.cleanMarkdown}`,
    "",
  ]),
].join("\n");
validateGeneratedLinks(llmsFull, generatedUrls, "llms-full.txt");

await rm(generatedDocsRoot, { recursive: true, force: true });
for (const page of renderedPages) {
  await mkdir(dirname(page.outputPath), { recursive: true });
  await writeFile(
    page.outputPath,
    `<!-- Generated from docs/content/docs/${page.source}; edit the source file instead. -->\n\n${page.cleanMarkdown}\n`,
  );
}

await copyFile(llmsSource, llmsOutput);
await writeFile(llmsFullOutput, `${llmsFull.trim()}\n`);

console.log(
  `Generated ${pages.length} Markdown documentation pages, llms.txt, and llms-full.txt`,
);
