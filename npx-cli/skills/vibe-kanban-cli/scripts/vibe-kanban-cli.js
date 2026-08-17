#!/usr/bin/env node

const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");

const VALID_TASK_STATUSES = new Set([
  "todo",
  "inprogress",
  "inreview",
  "done",
  "cancelled",
]);
const DEFAULT_LIMIT = 50;
const MAX_LIMIT = 1000;
const MAX_RESPONSE_BYTES = 5 * 1024 * 1024;
const MAX_INPUT_BYTES = 1024 * 1024;

class CliError extends Error {
  constructor(message, code = "CLI_ERROR", details) {
    super(message);
    this.name = "CliError";
    this.code = code;
    this.details = details;
  }
}

function writeJson(stream, value) {
  stream.write(`${JSON.stringify(value)}\n`);
}

function parseOptions(args, schema) {
  const options = {};

  for (let index = 0; index < args.length; index += 1) {
    const name = args[index];
    if (!name.startsWith("--") || !(name in schema)) {
      throw new CliError(`Unknown option: ${name}`, "INVALID_ARGUMENT");
    }

    if (schema[name] === "boolean") {
      options[name] = true;
      continue;
    }

    const value = args[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new CliError(`Missing value for ${name}`, "INVALID_ARGUMENT");
    }
    options[name] = value;
    index += 1;
  }

  return options;
}

function requireOption(options, name) {
  const value = options[name];
  if (typeof value !== "string" || value.trim() === "") {
    throw new CliError(`${name} is required`, "INVALID_ARGUMENT");
  }
  return value;
}

function resolveBackendUrl(env) {
  if (env.VIBE_BACKEND_URL) {
    return validateBackendUrl(env.VIBE_BACKEND_URL);
  }

  const port = env.BACKEND_PORT;
  if (port) {
    if (!/^\d+$/.test(port)) {
      throw new CliError(
        `Invalid backend port: ${port}`,
        "INVALID_CONFIGURATION",
      );
    }
    const configuredHost = env.HOST || "127.0.0.1";
    const host = ["0.0.0.0", "::", "[::]"].includes(configuredHost)
      ? "127.0.0.1"
      : configuredHost;
    return validateBackendUrl(`http://${host}:${port}`);
  }

  const portFile = path.join(os.tmpdir(), "vibe-kanban", "vibe-kanban.port");
  let storedPort;
  try {
    storedPort = fs.readFileSync(portFile, "utf8").trim();
  } catch (error) {
    throw new CliError(
      "Vibe Kanban is not running or its backend could not be discovered",
      "BACKEND_NOT_FOUND",
      error.message,
    );
  }

  if (!/^\d+$/.test(storedPort)) {
    throw new CliError(`Invalid port in ${portFile}`, "INVALID_CONFIGURATION");
  }
  return `http://127.0.0.1:${storedPort}`;
}

function validateBackendUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new CliError(
      `Invalid VIBE_BACKEND_URL: ${value}`,
      "INVALID_CONFIGURATION",
    );
  }

  if (!["http:", "https:"].includes(url.protocol)) {
    throw new CliError(
      "VIBE_BACKEND_URL must use http or https",
      "INVALID_CONFIGURATION",
    );
  }
  return url.toString().replace(/\/$/, "");
}

function requestJson(baseUrl, pathname, options = {}) {
  const url = new URL(pathname, `${baseUrl}/`);
  const transport = url.protocol === "https:" ? https : http;
  const timeoutMs = options.timeoutMs || 5000;
  const body =
    options.body === undefined ? undefined : JSON.stringify(options.body);

  return new Promise((resolve, reject) => {
    const request = transport.request(
      url,
      {
        method: options.method || "GET",
        headers: body
          ? {
              "content-type": "application/json",
              "content-length": Buffer.byteLength(body),
            }
          : undefined,
      },
      (response) => {
        const chunks = [];
        let totalBytes = 0;

        response.on("data", (chunk) => {
          totalBytes += chunk.length;
          if (totalBytes > MAX_RESPONSE_BYTES) {
            request.destroy(
              new CliError("Backend response is too large", "INVALID_RESPONSE"),
            );
            return;
          }
          chunks.push(chunk);
        });

        response.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8");
          let envelope;
          try {
            envelope = JSON.parse(text);
          } catch {
            reject(
              new CliError(
                `Backend returned HTTP ${response.statusCode} with invalid JSON`,
                "INVALID_RESPONSE",
              ),
            );
            return;
          }

          if (
            response.statusCode < 200 ||
            response.statusCode >= 300 ||
            envelope.success !== true
          ) {
            reject(
              new CliError(
                envelope.message ||
                  `Backend returned HTTP ${response.statusCode}`,
                "BACKEND_ERROR",
                envelope.error_data,
              ),
            );
            return;
          }

          resolve(envelope.data);
        });
      },
    );

    request.setTimeout(timeoutMs, () => {
      request.destroy(
        new CliError(
          `Backend request timed out after ${timeoutMs}ms`,
          "TIMEOUT",
        ),
      );
    });
    request.on("error", (error) => {
      reject(
        error instanceof CliError
          ? error
          : new CliError(
              `Failed to connect to Vibe Kanban: ${error.message}`,
              "CONNECTION_ERROR",
            ),
      );
    });

    if (body) request.write(body);
    request.end();
  });
}

function normalizeContainerRef(cwd) {
  let resolved = cwd;
  try {
    resolved = fs.realpathSync(cwd);
  } catch {
    resolved = path.resolve(cwd);
  }

  if (process.platform === "darwin") {
    if (resolved === "/private/var") return "/var";
    if (resolved.startsWith("/private/var/")) return resolved.slice(8);
    if (resolved === "/private/tmp") return "/tmp";
    if (resolved.startsWith("/private/tmp/")) return resolved.slice(8);
  }
  return resolved;
}

function getTimeoutMs(env) {
  const raw = env.VIBE_KANBAN_CLI_TIMEOUT_MS;
  if (!raw) return 5000;
  const timeout = Number(raw);
  if (!Number.isInteger(timeout) || timeout <= 0 || timeout > 120000) {
    throw new CliError(
      `Invalid VIBE_KANBAN_CLI_TIMEOUT_MS: ${raw}`,
      "INVALID_CONFIGURATION",
    );
  }
  return timeout;
}

function readStream(stream) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let totalBytes = 0;

    stream.on("data", (chunk) => {
      const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      totalBytes += buffer.length;
      if (totalBytes > MAX_INPUT_BYTES) {
        reject(new CliError("JSON input is too large", "INVALID_INPUT"));
        return;
      }
      chunks.push(buffer);
    });
    stream.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    stream.on("error", reject);
  });
}

async function readCreateInput(source, stdin, cwd) {
  let text;
  if (source === "-") {
    text = await readStream(stdin);
  } else {
    const inputPath = path.resolve(cwd, source);
    try {
      const stats = fs.statSync(inputPath);
      if (stats.size > MAX_INPUT_BYTES) {
        throw new CliError("JSON input is too large", "INVALID_INPUT");
      }
      text = fs.readFileSync(inputPath, "utf8");
    } catch (error) {
      if (error instanceof CliError) throw error;
      throw new CliError(
        `Cannot read task input file: ${error.message}`,
        "INVALID_INPUT",
      );
    }
  }

  let input;
  try {
    input = JSON.parse(text);
  } catch {
    throw new CliError("Task input must be valid JSON", "INVALID_INPUT");
  }

  if (!input || typeof input !== "object" || Array.isArray(input)) {
    throw new CliError("Task input must be a JSON object", "INVALID_INPUT");
  }

  const allowedKeys = new Set(["project_id", "title", "description"]);
  const unknownKeys = Object.keys(input).filter((key) => !allowedKeys.has(key));
  if (unknownKeys.length > 0) {
    throw new CliError(
      `Unknown task fields: ${unknownKeys.join(", ")}`,
      "INVALID_INPUT",
    );
  }
  if (typeof input.project_id !== "string" || input.project_id.trim() === "") {
    throw new CliError("project_id is required", "INVALID_INPUT");
  }
  if (typeof input.title !== "string" || input.title.trim() === "") {
    throw new CliError("title must not be empty", "INVALID_INPUT");
  }
  if (
    input.description !== undefined &&
    input.description !== null &&
    typeof input.description !== "string"
  ) {
    throw new CliError("description must be a string or null", "INVALID_INPUT");
  }

  return {
    project_id: input.project_id.trim(),
    title: input.title.trim(),
    description: input.description ?? null,
  };
}

function selectTargetBranch(repo, branches) {
  if (!Array.isArray(branches)) {
    throw new CliError(
      `Invalid branch response for repository ${repo.id}`,
      "INVALID_RESPONSE",
    );
  }

  const validBranches = branches.filter(
    (branch) => branch && typeof branch.name === "string" && branch.name,
  );
  const branchNames = new Set(validBranches.map((branch) => branch.name));
  if (
    typeof repo.default_target_branch === "string" &&
    branchNames.has(repo.default_target_branch)
  ) {
    return repo.default_target_branch;
  }

  const currentBranch = validBranches.find((branch) => branch.is_current);
  const targetBranch = currentBranch?.name || validBranches[0]?.name;
  if (!targetBranch) {
    throw new CliError(
      `Repository ${repo.display_name || repo.id} has no branches`,
      "START_CONFIGURATION_ERROR",
    );
  }
  return targetBranch;
}

async function resolveStartConfiguration(baseUrl, projectId, requestOptions) {
  const [info, projectRepos] = await Promise.all([
    requestJson(baseUrl, "/api/info", requestOptions),
    requestJson(
      baseUrl,
      `/api/projects/${encodeURIComponent(projectId)}/repositories`,
      requestOptions,
    ),
  ]);

  const configuredProfile = info?.config?.executor_profile;
  if (
    !configuredProfile ||
    typeof configuredProfile !== "object" ||
    typeof configuredProfile.executor !== "string" ||
    configuredProfile.executor.trim() === "" ||
    (configuredProfile.variant !== undefined &&
      configuredProfile.variant !== null &&
      typeof configuredProfile.variant !== "string")
  ) {
    throw new CliError(
      "No valid default executor profile is configured",
      "START_CONFIGURATION_ERROR",
    );
  }
  if (!Array.isArray(projectRepos)) {
    throw new CliError(
      "Backend returned an invalid project repository list",
      "INVALID_RESPONSE",
    );
  }
  if (projectRepos.length === 0) {
    throw new CliError(
      "The task project has no repositories",
      "START_CONFIGURATION_ERROR",
    );
  }

  const repos = await Promise.all(
    projectRepos.map(async (repo) => {
      if (!repo || typeof repo.id !== "string" || repo.id.trim() === "") {
        throw new CliError(
          "Backend returned a project repository without an ID",
          "INVALID_RESPONSE",
        );
      }
      const branches = await requestJson(
        baseUrl,
        `/api/repos/${encodeURIComponent(repo.id)}/branches`,
        requestOptions,
      );
      return {
        repo_id: repo.id,
        target_branch: selectTargetBranch(repo, branches),
      };
    }),
  );

  const executorProfileId = { executor: configuredProfile.executor };
  if (configuredProfile.variant !== undefined) {
    executorProfileId.variant = configuredProfile.variant;
  }

  return { executor_profile_id: executorProfileId, repos };
}

function helpText() {
  return `Vibe Kanban task CLI

Commands:
  context --json
  project list --json
  task list --project-id <uuid> [--status <status>] [--limit <count>] --json
  task get --task-id <uuid> --json
  task create --from-json <path|-> --json
  task start --task-id <uuid> --json
  task create-and-start --from-json <path|-> --json
`;
}

async function execute(args, runtime) {
  const [group, action, ...rest] = args;
  if (!group || group === "help" || group === "--help") {
    runtime.stdout.write(helpText());
    return 0;
  }

  const baseUrl = resolveBackendUrl(runtime.env);
  const timeoutMs = getTimeoutMs(runtime.env);
  const requestOptions = { timeoutMs };

  if (group === "context") {
    parseOptions([action, ...rest].filter(Boolean), { "--json": "boolean" });
    const containerRef = normalizeContainerRef(runtime.cwd);
    const query = new URLSearchParams({ ref: containerRef });
    const context = await requestJson(
      baseUrl,
      `/api/containers/attempt-context?${query}`,
      requestOptions,
    );
    writeJson(runtime.stdout, { ok: true, context });
    return 0;
  }

  if (group === "project" && action === "list") {
    parseOptions(rest, { "--json": "boolean" });
    const projects = await requestJson(
      baseUrl,
      "/api/projects",
      requestOptions,
    );
    writeJson(runtime.stdout, { ok: true, projects });
    return 0;
  }

  if (group === "task" && action === "list") {
    const options = parseOptions(rest, {
      "--project-id": "string",
      "--status": "string",
      "--limit": "string",
      "--json": "boolean",
    });
    const projectId = requireOption(options, "--project-id");
    const status = options["--status"];
    if (status && !VALID_TASK_STATUSES.has(status)) {
      throw new CliError(`Invalid task status: ${status}`, "INVALID_ARGUMENT");
    }
    const limit = options["--limit"]
      ? Number(options["--limit"])
      : DEFAULT_LIMIT;
    if (!Number.isInteger(limit) || limit <= 0 || limit > MAX_LIMIT) {
      throw new CliError(
        `--limit must be an integer between 1 and ${MAX_LIMIT}`,
        "INVALID_ARGUMENT",
      );
    }

    const query = new URLSearchParams({ project_id: projectId });
    let tasks = await requestJson(
      baseUrl,
      `/api/tasks?${query}`,
      requestOptions,
    );
    if (status) tasks = tasks.filter((task) => task.status === status);
    tasks = tasks.slice(0, limit);
    writeJson(runtime.stdout, { ok: true, tasks });
    return 0;
  }

  if (group === "task" && action === "get") {
    const options = parseOptions(rest, {
      "--task-id": "string",
      "--json": "boolean",
    });
    const taskId = requireOption(options, "--task-id");
    const task = await requestJson(
      baseUrl,
      `/api/tasks/${encodeURIComponent(taskId)}`,
      requestOptions,
    );
    writeJson(runtime.stdout, { ok: true, task });
    return 0;
  }

  if (group === "task" && action === "create") {
    const options = parseOptions(rest, {
      "--from-json": "string",
      "--json": "boolean",
    });
    const source = requireOption(options, "--from-json");
    const payload = await readCreateInput(source, runtime.stdin, runtime.cwd);
    const task = await requestJson(baseUrl, "/api/tasks", {
      ...requestOptions,
      method: "POST",
      body: payload,
    });
    writeJson(runtime.stdout, { ok: true, task });
    return 0;
  }

  if (group === "task" && action === "start") {
    const options = parseOptions(rest, {
      "--task-id": "string",
      "--json": "boolean",
    });
    const taskId = requireOption(options, "--task-id");
    const task = await requestJson(
      baseUrl,
      `/api/tasks/${encodeURIComponent(taskId)}`,
      requestOptions,
    );
    if (!task || typeof task.project_id !== "string") {
      throw new CliError(
        "Backend returned a task without a project ID",
        "INVALID_RESPONSE",
      );
    }
    const startConfiguration = await resolveStartConfiguration(
      baseUrl,
      task.project_id,
      requestOptions,
    );
    const attempt = await requestJson(baseUrl, "/api/task-attempts", {
      ...requestOptions,
      method: "POST",
      body: { task_id: taskId, ...startConfiguration },
    });
    writeJson(runtime.stdout, { ok: true, attempt });
    return 0;
  }

  if (group === "task" && action === "create-and-start") {
    const options = parseOptions(rest, {
      "--from-json": "string",
      "--json": "boolean",
    });
    const source = requireOption(options, "--from-json");
    const taskPayload = await readCreateInput(
      source,
      runtime.stdin,
      runtime.cwd,
    );
    const startConfiguration = await resolveStartConfiguration(
      baseUrl,
      taskPayload.project_id,
      requestOptions,
    );
    const task = await requestJson(baseUrl, "/api/tasks/create-and-start", {
      ...requestOptions,
      method: "POST",
      body: { task: taskPayload, ...startConfiguration },
    });
    writeJson(runtime.stdout, { ok: true, task });
    return 0;
  }

  throw new CliError(`Unknown command: ${args.join(" ")}`, "INVALID_ARGUMENT");
}

async function run(args, overrides = {}) {
  const runtime = {
    stdin: overrides.stdin || process.stdin,
    stdout: overrides.stdout || process.stdout,
    stderr: overrides.stderr || process.stderr,
    cwd: overrides.cwd || process.cwd(),
    env: overrides.env || process.env,
  };

  try {
    return await execute(args, runtime);
  } catch (error) {
    const cliError =
      error instanceof CliError
        ? error
        : new CliError(error.message || String(error), "UNEXPECTED_ERROR");
    const payload = {
      ok: false,
      error: {
        code: cliError.code,
        message: cliError.message,
      },
    };
    if (cliError.details !== undefined)
      payload.error.details = cliError.details;
    writeJson(runtime.stderr, payload);
    return 1;
  }
}

if (require.main === module) {
  run(process.argv.slice(2)).then((exitCode) => {
    process.exitCode = exitCode;
  });
}

module.exports = { run };
