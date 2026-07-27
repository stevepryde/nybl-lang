import { resolve } from "node:path";

const workspaceRoot = resolve(import.meta.dir, "..");
const licenseFiles = ["LICENSE-MIT", "LICENSE-APACHE"] as const;
const packages = [
  { name: "nybl-lang", directory: "nybl" },
  { name: "nybl-sys", directory: "nybl-sys" },
  { name: "nybl-vm", directory: "nybl-vm" },
  { name: "nybl-compile", directory: "nybl-compile" },
  { name: "nybl-cli", directory: "nybl-cli" },
] as const;

const canonicalLicenses = new Map(
  await Promise.all(
    licenseFiles.map(async (file) => [
      file,
      await Bun.file(resolve(workspaceRoot, file)).text(),
    ]),
  ),
);

for (const pkg of packages) {
  for (const file of licenseFiles) {
    const packagedLicense = Bun.file(resolve(workspaceRoot, pkg.directory, file));
    if (!(await packagedLicense.exists())) {
      throw new Error(`${pkg.name}: missing package-local ${file}`);
    }

    const expected = canonicalLicenses.get(file);
    const actual = await packagedLicense.text();
    if (actual !== expected) {
      throw new Error(
        `${pkg.name}: ${file} differs from the workspace-root license text`,
      );
    }
  }

  const result = Bun.spawnSync(
    [
      "cargo",
      "package",
      "-p",
      pkg.name,
      "--allow-dirty",
      "--locked",
      "--list",
    ],
    {
      cwd: workspaceRoot,
      stdout: "pipe",
      stderr: "pipe",
    },
  );

  if (!result.success) {
    throw new Error(
      `${pkg.name}: cargo package --list failed\n${result.stderr.toString()}`,
    );
  }

  const packagedFiles = new Set(
    result.stdout
      .toString()
      .split(/\r?\n/)
      .filter((line) => line.length > 0),
  );
  const missing = licenseFiles.filter((file) => !packagedFiles.has(file));
  if (missing.length > 0) {
    throw new Error(
      `${pkg.name}: package archive would omit ${missing.join(", ")}`,
    );
  }

  console.log(`${pkg.name}: includes both canonical license texts`);
}
