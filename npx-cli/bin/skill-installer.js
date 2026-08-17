const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");

const SKILL_NAME = "vibe-kanban-cli";
const MANIFEST_FILE = ".vibe-kanban-skill.json";
const MANAGED_FILES = ["SKILL.md", "scripts/vibe-kanban-cli.js"];

class SkillInstallError extends Error {
  constructor(message, code) {
    super(message);
    this.name = "SkillInstallError";
    this.code = code;
  }
}

function sha256File(filePath) {
  return crypto
    .createHash("sha256")
    .update(fs.readFileSync(filePath))
    .digest("hex");
}

function readManifest(targetDir) {
  const manifestPath = path.join(targetDir, MANIFEST_FILE);
  if (!fs.existsSync(manifestPath)) return null;

  try {
    const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    if (
      manifest.name !== SKILL_NAME ||
      !manifest.files ||
      typeof manifest.files !== "object" ||
      Array.isArray(manifest.files)
    ) {
      throw new Error("invalid manifest fields");
    }
    return manifest;
  } catch (error) {
    throw new SkillInstallError(
      `Cannot read the existing ${SKILL_NAME} install manifest: ${error.message}`,
      "INVALID_MANIFEST",
    );
  }
}

function currentFilesMatch(targetDir, expectedHashes) {
  return MANAGED_FILES.every((relativePath) => {
    const targetPath = path.join(targetDir, relativePath);
    return (
      fs.existsSync(targetPath) &&
      expectedHashes[relativePath] === sha256File(targetPath)
    );
  });
}

function sourceHashes(sourceDir) {
  return Object.fromEntries(
    MANAGED_FILES.map((relativePath) => {
      const sourcePath = path.join(sourceDir, relativePath);
      if (!fs.existsSync(sourcePath)) {
        throw new SkillInstallError(
          `Bundled skill file is missing: ${relativePath}`,
          "INVALID_PACKAGE",
        );
      }
      return [relativePath, sha256File(sourcePath)];
    }),
  );
}

function copyManagedFiles(sourceDir, targetDir) {
  for (const relativePath of MANAGED_FILES) {
    const sourcePath = path.join(sourceDir, relativePath);
    const targetPath = path.join(targetDir, relativePath);
    fs.mkdirSync(path.dirname(targetPath), { recursive: true });
    fs.copyFileSync(sourcePath, targetPath);
  }

  if (process.platform !== "win32") {
    fs.chmodSync(path.join(targetDir, "scripts/vibe-kanban-cli.js"), 0o755);
  }
}

function installSkill(options = {}) {
  const sourceDir =
    options.sourceDir || path.join(__dirname, "..", "skills", SKILL_NAME);
  const skillsRoot =
    options.skillsRoot || path.join(os.homedir(), ".agents", "skills");
  const targetDir = path.join(skillsRoot, SKILL_NAME);
  const version = options.version || "unknown";
  const force = options.force === true;
  const hashes = sourceHashes(sourceDir);
  const targetExists = fs.existsSync(targetDir);
  let action = targetExists ? "updated" : "installed";

  if (targetExists) {
    let manifest = null;
    try {
      manifest = readManifest(targetDir);
    } catch (error) {
      if (!force) throw error;
    }

    if (manifest) {
      const hasUserChanges = !currentFilesMatch(targetDir, manifest.files);
      if (hasUserChanges && !force) {
        throw new SkillInstallError(
          `The installed ${SKILL_NAME} skill has local changes; rerun with --force to overwrite managed files`,
          "LOCAL_CHANGES",
        );
      }
      if (
        !hasUserChanges &&
        manifest.version === version &&
        currentFilesMatch(targetDir, hashes)
      ) {
        return {
          ok: true,
          skill: SKILL_NAME,
          path: targetDir,
          version,
          action: "unchanged",
        };
      }
    } else if (currentFilesMatch(targetDir, hashes)) {
      action = "adopted";
    } else {
      const hasManagedFiles = MANAGED_FILES.some((relativePath) =>
        fs.existsSync(path.join(targetDir, relativePath)),
      );
      if (hasManagedFiles && !force) {
        throw new SkillInstallError(
          `The target directory already contains an unmanaged ${SKILL_NAME} skill; rerun with --force to overwrite managed files`,
          "UNMANAGED_INSTALL",
        );
      }
    }
  }

  fs.mkdirSync(targetDir, { recursive: true });
  copyManagedFiles(sourceDir, targetDir);
  const manifest = {
    name: SKILL_NAME,
    version,
    files: hashes,
  };
  fs.writeFileSync(
    path.join(targetDir, MANIFEST_FILE),
    `${JSON.stringify(manifest, null, 2)}\n`,
    "utf8",
  );

  return { ok: true, skill: SKILL_NAME, path: targetDir, version, action };
}

module.exports = { SkillInstallError, installSkill };
