const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { SkillInstallError, installSkill } = require("../bin/skill-installer");

const sourceDir = path.join(__dirname, "..", "skills", "vibe-kanban-cli");

function temporarySkillsRoot(t) {
  const root = fs.mkdtempSync(
    path.join(os.tmpdir(), "vibe-kanban-skill-test-"),
  );
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return root;
}

test("installSkill installs the managed skill files", (t) => {
  const skillsRoot = temporarySkillsRoot(t);
  const result = installSkill({ sourceDir, skillsRoot, version: "1.2.3" });

  assert.equal(result.action, "installed");
  assert.equal(result.path, path.join(skillsRoot, "vibe-kanban-cli"));
  assert.equal(
    fs.readFileSync(path.join(result.path, "SKILL.md"), "utf8"),
    fs.readFileSync(path.join(sourceDir, "SKILL.md"), "utf8"),
  );
  assert.ok(
    fs.existsSync(path.join(result.path, "scripts", "vibe-kanban-cli.js")),
  );

  const manifest = JSON.parse(
    fs.readFileSync(path.join(result.path, ".vibe-kanban-skill.json"), "utf8"),
  );
  assert.equal(manifest.name, "vibe-kanban-cli");
  assert.equal(manifest.version, "1.2.3");
});

test("installSkill is idempotent and records version updates", (t) => {
  const skillsRoot = temporarySkillsRoot(t);
  installSkill({ sourceDir, skillsRoot, version: "1.2.3" });

  const unchanged = installSkill({
    sourceDir,
    skillsRoot,
    version: "1.2.3",
  });
  assert.equal(unchanged.action, "unchanged");

  const updated = installSkill({ sourceDir, skillsRoot, version: "1.2.4" });
  assert.equal(updated.action, "updated");
  const manifest = JSON.parse(
    fs.readFileSync(path.join(updated.path, ".vibe-kanban-skill.json"), "utf8"),
  );
  assert.equal(manifest.version, "1.2.4");
});

test("installSkill protects local changes unless force is explicit", (t) => {
  const skillsRoot = temporarySkillsRoot(t);
  const first = installSkill({ sourceDir, skillsRoot, version: "1.2.3" });
  const skillPath = path.join(first.path, "SKILL.md");
  fs.appendFileSync(skillPath, "\nLocal customization\n");

  assert.throws(
    () => installSkill({ sourceDir, skillsRoot, version: "1.2.4" }),
    (error) =>
      error instanceof SkillInstallError && error.code === "LOCAL_CHANGES",
  );
  assert.match(fs.readFileSync(skillPath, "utf8"), /Local customization/);

  const forced = installSkill({
    sourceDir,
    skillsRoot,
    version: "1.2.4",
    force: true,
  });
  assert.equal(forced.action, "updated");
  assert.doesNotMatch(
    fs.readFileSync(skillPath, "utf8"),
    /Local customization/,
  );
});

test("installSkill refuses to overwrite an unmanaged skill", (t) => {
  const skillsRoot = temporarySkillsRoot(t);
  const target = path.join(skillsRoot, "vibe-kanban-cli");
  fs.mkdirSync(target, { recursive: true });
  fs.writeFileSync(path.join(target, "SKILL.md"), "user-owned skill\n");

  assert.throws(
    () => installSkill({ sourceDir, skillsRoot, version: "1.2.3" }),
    (error) =>
      error instanceof SkillInstallError && error.code === "UNMANAGED_INSTALL",
  );
  assert.equal(
    fs.readFileSync(path.join(target, "SKILL.md"), "utf8"),
    "user-owned skill\n",
  );
});

test("the package CLI installs the skill in the shared agent directory", (t) => {
  const home = temporarySkillsRoot(t);
  const stubModules = temporarySkillsRoot(t);
  const admZipDir = path.join(stubModules, "adm-zip");
  fs.mkdirSync(admZipDir, { recursive: true });
  fs.writeFileSync(
    path.join(admZipDir, "index.js"),
    "module.exports = class AdmZip {};\n",
  );

  const result = spawnSync(
    process.execPath,
    [path.join(__dirname, "..", "bin", "cli.js"), "skill", "install", "--json"],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        HOME: home,
        USERPROFILE: home,
        NODE_PATH: [stubModules, process.env.NODE_PATH]
          .filter(Boolean)
          .join(path.delimiter),
      },
    },
  );

  assert.equal(result.status, 0, result.stderr);
  const output = JSON.parse(result.stdout);
  assert.equal(
    output.path,
    path.join(home, ".agents", "skills", "vibe-kanban-cli"),
  );
  assert.ok(fs.existsSync(path.join(output.path, "SKILL.md")));
});
