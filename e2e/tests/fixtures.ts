import { test as base, expect, type Page } from "@playwright/test";
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

export { expect };

const FURMI_BIN =
  process.env.FURMI_BIN ?? path.resolve(__dirname, "../../target/debug/furmi");

/** A running dashboard plus the throwaway state behind it. */
export interface Forge {
  url: string;
  home: string;
  /** The git repo tasks run in. */
  repo: string;
  /** Runs `furmi <args>` against this home and returns its stdout. */
  furmi(...args: string[]): string;
  /** Queues a task through the CLI, the way a user would from a terminal. */
  add(id: string, prompt: string, ...extra: string[]): void;
  /** Path of a task file, wherever it currently is (tasks/ or archive/). */
  taskFile(id: string): string | undefined;
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address() as net.AddressInfo;
      srv.close(() => resolve(port));
    });
  });
}

async function waitForServer(url: string, child: ChildProcess) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`furmi exited early (${child.exitCode})`);
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`dashboard never came up at ${url}`);
}

function git(cwd: string, ...args: string[]) {
  execFileSync("git", args, { cwd, stdio: "pipe" });
}

export const test = base.extend<{ forge: Forge; runner: boolean; config: string }>({
  /** Run `furmi start` (runner + dashboard) instead of just `furmi web`. */
  runner: [false, { option: true }],
  /** Extra config.toml text appended after the stub agent. */
  config: ["", { option: true }],

  forge: async ({ runner, config }, use) => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "furmi-e2e-"));
    const home = path.join(root, "home");
    const repo = path.join(root, "repo");
    const tmux = path.join(root, "tmux");
    fs.mkdirSync(repo);
    fs.mkdirSync(tmux);

    // Nothing here may touch the real user's files or tmux server.
    const env = {
      ...process.env,
      HOME: root,
      TMUX_TMPDIR: tmux,
      FURMI_NO_UPDATE_CHECK: "1",
    };
    delete env.TMUX;

    const furmi = (...args: string[]) =>
      execFileSync(FURMI_BIN, ["--home", home, ...args], { env, cwd: repo, stdio: "pipe" }).toString();

    git(repo, "init", "-q", "-b", "main");
    git(repo, "config", "user.email", "e2e@example.com");
    git(repo, "config", "user.name", "E2E");
    git(repo, "commit", "-q", "--allow-empty", "-m", "init");

    furmi("init", "--agent", "claude", "--no-global-skill");
    // A stub "agent" that runs the task's prompt as a shell command, plus a
    // second one so the agent picker has a real choice.
    fs.writeFileSync(
      path.join(home, "config.toml"),
      `[runner]
tmux_session = "furmie2e"
poll_interval = "1s"
max_parallel_per_repo = 3 # tests run several tasks in the one test repo

[defaults]
agent = "stub"
retry_delay = "1s"
retries = 0

[agents.stub]
headless = ["sh", "-c", "{prompt}"]
interactive = ["sh", "-c", "{prompt}"]

[agents.stub2]
headless = ["sh", "-c", "{prompt}"]
interactive = ["sh", "-c", "{prompt}"]
${config}
`,
    );

    const port = await freePort();
    const url = `http://127.0.0.1:${port}`;
    const server = spawn(
      FURMI_BIN,
      ["--home", home, runner ? "start" : "web", "--no-open", "--port", String(port)],
      { env, cwd: repo, stdio: "ignore" },
    );

    try {
      await waitForServer(url, server);
      await use({
        url,
        home,
        repo,
        furmi,
        add: (id, prompt, ...extra) =>
          void furmi("add", "--repo", repo, "--id", id, "--prompt", prompt, ...extra),
        taskFile: (id) =>
          ["tasks", "archive"]
            .map((d) => path.join(home, d, `${id}.md`))
            .find((p) => fs.existsSync(p)),
      });
    } finally {
      server.kill();
      // `furmi start` leaves its runner behind on purpose; stop it and the
      // tmux server so nothing outlives the test.
      try {
        furmi("stop");
      } catch {
        /* none running */
      }
      try {
        execFileSync("tmux", ["kill-server"], { env, stdio: "ignore" });
      } catch {
        /* no server */
      }
      fs.rmSync(root, { recursive: true, force: true });
    }
  },
});

/** The dashboard page with its first state load done. */
export async function openDashboard(page: Page, forge: Forge) {
  await page.goto(forge.url);
  // The home is shown with `~` for $HOME, which the fixture points at the
  // temp root; this also proves the first /api/state load has rendered.
  await expect(page.locator("#home")).toHaveText("~/home");
}

/** The table row for a task or schedule id in the active panel. */
export function row(page: Page, id: string) {
  return page.locator("tbody tr", { hasText: id });
}

export function tab(page: Page, name: "queue" | "running" | "archive" | "schedules") {
  return page.locator(`#tabs button[data-tab="${name}"]`);
}

/**
 * The id from a "Queued <id>" / "Scheduled <id>" toast. Waits for the toast
 * first: reading it straight after clicking submit races the server's reply.
 */
export async function createdId(page: Page, verb: "Queued" | "Scheduled"): Promise<string> {
  const toast = page.locator("#toast");
  await expect(toast).toContainText(`${verb} `);
  const id = (await toast.textContent())!.replace(`${verb} `, "").trim();
  expect(id).not.toBe("");
  return id;
}
