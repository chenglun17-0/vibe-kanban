const assert = require("node:assert/strict");
const http = require("node:http");
const { Readable } = require("node:stream");
const test = require("node:test");

const { run } = require("../skills/vibe-kanban-cli/scripts/vibe-kanban-cli");

function captureStream() {
  let content = "";
  return {
    stream: {
      write(chunk) {
        content += chunk.toString();
      },
    },
    json() {
      return JSON.parse(content);
    },
    text() {
      return content;
    },
  };
}

async function withServer(handler, callback) {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();

  try {
    await callback(`http://127.0.0.1:${address.port}`);
  } finally {
    await new Promise((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }
}

function respond(response, data) {
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify({ success: true, data }));
}

function runtime(baseUrl, input = "") {
  const stdout = captureStream();
  const stderr = captureStream();
  return {
    overrides: {
      stdin: Readable.from([input]),
      stdout: stdout.stream,
      stderr: stderr.stream,
      cwd: process.cwd(),
      env: { VIBE_BACKEND_URL: baseUrl },
    },
    stdout,
    stderr,
  };
}

test("context resolves the current directory through the local API", async () => {
  await withServer(
    (request, response) => {
      const url = new URL(request.url, "http://localhost");
      assert.equal(url.pathname, "/api/containers/attempt-context");
      assert.equal(url.searchParams.get("ref"), process.cwd());
      respond(response, {
        project: { id: "project-1", name: "Example" },
        task: { id: "task-1" },
        workspace: { id: "workspace-1" },
        workspace_repos: [],
      });
    },
    async (baseUrl) => {
      const state = runtime(baseUrl);
      const exitCode = await run(["context", "--json"], state.overrides);

      assert.equal(exitCode, 0);
      assert.equal(state.stderr.text(), "");
      assert.equal(state.stdout.json().context.project.id, "project-1");
    },
  );
});

test("project list and task get return structured JSON", async () => {
  await withServer(
    (request, response) => {
      if (request.url === "/api/projects") {
        respond(response, [{ id: "project-1", name: "Example" }]);
        return;
      }
      if (request.url === "/api/tasks/task-1") {
        respond(response, { id: "task-1", title: "Existing task" });
        return;
      }
      response.writeHead(404).end();
    },
    async (baseUrl) => {
      const projectsState = runtime(baseUrl);
      assert.equal(
        await run(["project", "list", "--json"], projectsState.overrides),
        0,
      );
      assert.deepEqual(projectsState.stdout.json().projects, [
        { id: "project-1", name: "Example" },
      ]);

      const taskState = runtime(baseUrl);
      assert.equal(
        await run(
          ["task", "get", "--task-id", "task-1", "--json"],
          taskState.overrides,
        ),
        0,
      );
      assert.equal(taskState.stdout.json().task.title, "Existing task");
    },
  );
});

test("task list applies status and limit filters", async () => {
  await withServer(
    (request, response) => {
      const url = new URL(request.url, "http://localhost");
      assert.equal(url.pathname, "/api/tasks");
      assert.equal(url.searchParams.get("project_id"), "project-1");
      respond(response, [
        { id: "task-1", status: "todo" },
        { id: "task-2", status: "done" },
        { id: "task-3", status: "todo" },
      ]);
    },
    async (baseUrl) => {
      const state = runtime(baseUrl);
      const exitCode = await run(
        [
          "task",
          "list",
          "--project-id",
          "project-1",
          "--status",
          "todo",
          "--limit",
          "1",
          "--json",
        ],
        state.overrides,
      );

      assert.equal(exitCode, 0);
      assert.deepEqual(state.stdout.json().tasks, [
        { id: "task-1", status: "todo" },
      ]);
    },
  );
});

test("task create validates and forwards JSON input", async () => {
  let receivedBody;
  await withServer(
    (request, response) => {
      assert.equal(request.method, "POST");
      assert.equal(request.url, "/api/tasks");
      const chunks = [];
      request.on("data", (chunk) => chunks.push(chunk));
      request.on("end", () => {
        receivedBody = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        respond(response, { id: "task-1", ...receivedBody, status: "todo" });
      });
    },
    async (baseUrl) => {
      const input = JSON.stringify({
        project_id: "project-1",
        title: "  Create CLI skill  ",
        description: "Acceptance criteria",
      });
      const state = runtime(baseUrl, input);
      const exitCode = await run(
        ["task", "create", "--from-json", "-", "--json"],
        state.overrides,
      );

      assert.equal(exitCode, 0);
      assert.deepEqual(receivedBody, {
        project_id: "project-1",
        title: "Create CLI skill",
        description: "Acceptance criteria",
      });
      assert.equal(state.stdout.json().task.id, "task-1");
    },
  );
});

test("task start resolves the default profile and repository branches", async () => {
  let receivedBody;
  await withServer(
    (request, response) => {
      if (request.url === "/api/tasks/task-1") {
        respond(response, { id: "task-1", project_id: "project-1" });
        return;
      }
      if (request.url === "/api/info") {
        respond(response, {
          config: {
            executor_profile: { executor: "CODEX", variant: "DEFAULT" },
          },
        });
        return;
      }
      if (request.url === "/api/projects/project-1/repositories") {
        respond(response, [
          {
            id: "repo-1",
            display_name: "Primary",
            default_target_branch: "develop",
          },
          { id: "repo-2", display_name: "Secondary" },
        ]);
        return;
      }
      if (request.url === "/api/repos/repo-1/branches") {
        respond(response, [
          { name: "main", is_current: true },
          { name: "develop", is_current: false },
        ]);
        return;
      }
      if (request.url === "/api/repos/repo-2/branches") {
        respond(response, [
          { name: "stable", is_current: false },
          { name: "feature", is_current: true },
        ]);
        return;
      }
      if (request.url === "/api/task-attempts") {
        assert.equal(request.method, "POST");
        const chunks = [];
        request.on("data", (chunk) => chunks.push(chunk));
        request.on("end", () => {
          receivedBody = JSON.parse(Buffer.concat(chunks).toString("utf8"));
          respond(response, { id: "attempt-1", task_id: "task-1" });
        });
        return;
      }
      response.writeHead(404).end();
    },
    async (baseUrl) => {
      const state = runtime(baseUrl);
      const exitCode = await run(
        ["task", "start", "--task-id", "task-1", "--json"],
        state.overrides,
      );

      assert.equal(exitCode, 0);
      assert.deepEqual(receivedBody, {
        task_id: "task-1",
        executor_profile_id: { executor: "CODEX", variant: "DEFAULT" },
        repos: [
          { repo_id: "repo-1", target_branch: "develop" },
          { repo_id: "repo-2", target_branch: "feature" },
        ],
      });
      assert.equal(state.stdout.json().attempt.id, "attempt-1");
      assert.equal(state.stderr.text(), "");
    },
  );
});

test("task create-and-start forwards task input with automatic defaults", async () => {
  let receivedBody;
  await withServer(
    (request, response) => {
      if (request.url === "/api/info") {
        respond(response, {
          config: { executor_profile: { executor: "CLAUDE_CODE" } },
        });
        return;
      }
      if (request.url === "/api/projects/project-1/repositories") {
        respond(response, [
          {
            id: "repo-1",
            display_name: "Primary",
            default_target_branch: "missing",
          },
        ]);
        return;
      }
      if (request.url === "/api/repos/repo-1/branches") {
        respond(response, [{ name: "main", is_current: false }]);
        return;
      }
      if (request.url === "/api/tasks/create-and-start") {
        assert.equal(request.method, "POST");
        const chunks = [];
        request.on("data", (chunk) => chunks.push(chunk));
        request.on("end", () => {
          receivedBody = JSON.parse(Buffer.concat(chunks).toString("utf8"));
          respond(response, {
            id: "task-1",
            has_in_progress_attempt: true,
          });
        });
        return;
      }
      response.writeHead(404).end();
    },
    async (baseUrl) => {
      const state = runtime(
        baseUrl,
        JSON.stringify({
          project_id: "project-1",
          title: "  Create and start  ",
          description: "Acceptance criteria",
        }),
      );
      const exitCode = await run(
        ["task", "create-and-start", "--from-json", "-", "--json"],
        state.overrides,
      );

      assert.equal(exitCode, 0);
      assert.deepEqual(receivedBody, {
        task: {
          project_id: "project-1",
          title: "Create and start",
          description: "Acceptance criteria",
        },
        executor_profile_id: { executor: "CLAUDE_CODE" },
        repos: [{ repo_id: "repo-1", target_branch: "main" }],
      });
      assert.equal(state.stdout.json().task.has_in_progress_attempt, true);
      assert.equal(state.stderr.text(), "");
    },
  );
});

test("task start fails before creating an attempt when the project has no repositories", async () => {
  let attemptRequests = 0;
  await withServer(
    (request, response) => {
      if (request.url === "/api/tasks/task-1") {
        respond(response, { id: "task-1", project_id: "project-1" });
        return;
      }
      if (request.url === "/api/info") {
        respond(response, {
          config: { executor_profile: { executor: "CODEX" } },
        });
        return;
      }
      if (request.url === "/api/projects/project-1/repositories") {
        respond(response, []);
        return;
      }
      if (request.url === "/api/task-attempts") attemptRequests += 1;
      response.writeHead(404).end();
    },
    async (baseUrl) => {
      const state = runtime(baseUrl);
      const exitCode = await run(
        ["task", "start", "--task-id", "task-1", "--json"],
        state.overrides,
      );

      assert.equal(exitCode, 1);
      assert.equal(attemptRequests, 0);
      assert.equal(state.stderr.json().error.code, "START_CONFIGURATION_ERROR");
    },
  );
});

test("task create rejects empty titles before sending a request", async () => {
  let requestCount = 0;
  await withServer(
    (_request, response) => {
      requestCount += 1;
      respond(response, {});
    },
    async (baseUrl) => {
      const state = runtime(
        baseUrl,
        JSON.stringify({ project_id: "project-1", title: "   " }),
      );
      const exitCode = await run(
        ["task", "create", "--from-json", "-", "--json"],
        state.overrides,
      );

      assert.equal(exitCode, 1);
      assert.equal(requestCount, 0);
      assert.equal(state.stderr.json().error.code, "INVALID_INPUT");
    },
  );
});
