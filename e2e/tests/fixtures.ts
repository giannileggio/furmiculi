import { test as base, expect, type Page, type TestInfo } from "@playwright/test";
import { execFileSync, spawn } from "node:child_process";
import * as fs from "node:fs";
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

/** Most of the dashboard's own output kept for a failure report. */
const OUTPUT_KEEP = 64 * 1024;

/** What the dashboard printed, trimmed to the last `OUTPUT_KEEP` bytes. */
type Output = { text: string };

/**
 * Waits until the dashboard says where it is serving.
 *
 * The port is the OS's (`--port 0`), so it can only come from the server
 * itself: one dashboard per test then holds a port no other test can have
 * taken or be about to. Reading it from the output also means a dashboard
 * that died on the way up is a failure here, at startup, with what it said —
 * not a page that silently talks to whatever else answers on that port.
 */
function waitForUrl(child: ReturnType<typeof spawn>, output: Output): Promise<string> {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + 20_000;
    const check = () => {
      const found = /^Dashboard: (http:\S+)/m.exec(output.text);
      if (found) {
        clearInterval(timer);
        resolve(found[1]);
        return;
      }
      if (child.exitCode !== null || child.signalCode !== null) {
        clearInterval(timer);
        reject(
          new Error(`the dashboard exited before serving (${describe(child)}): ${tail(output)}`),
        );
        return;
      }
      if (Date.now() > deadline) {
        clearInterval(timer);
        reject(new Error(`the dashboard never said where it serves: ${tail(output)}`));
      }
    };
    const timer = setInterval(check, 25);
    check();
  });
}

function describe(child: ReturnType<typeof spawn>): string {
  return child.signalCode ?? `exit ${child.exitCode}`;
}

function tail(output: Output): string {
  const text = output.text.trim();
  return text === "" ? "(no output)" : `its last output was:\n${text}`;
}

/**
 * Prints the dashboard's own output when a test fails, so a red run explains
 * itself: a panic, a bind error and a dying process all look the same from the
 * page side (a fetch that fails), and only the server can say which it was.
 * Kept to failures: a passing run has nothing worth the lines.
 */
function reportOutput(testInfo: TestInfo, output: Output) {
  if (testInfo.status === testInfo.expectedStatus) return;
  process.stderr.write(
    `--- dashboard output (${testInfo.title}) ---\n${output.text || "(none)"}\n---\n`,
  );
}

function git(cwd: string, ...args: string[]) {
  execFileSync("git", args, { cwd, stdio: "pipe" });
}

export const test = base.extend<{ forge: Forge; runner: boolean; config: string }>({
  /** Run `furmi start` (runner + dashboard) instead of just `furmi web`. */
  runner: [false, { option: true }],
  /** Extra config.toml text appended after the stub agent. */
  config: ["", { option: true }],

  forge: async ({ runner, config }, use, testInfo) => {
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

    // Its own port, picked by the kernel: a test never has to guess one that
    // is free (and an ephemeral port two tests guessed at the same moment is
    // a race that shows up as a dashboard nobody can reach).
    const server = spawn(
      FURMI_BIN,
      ["--home", home, runner ? "start" : "web", "--no-open", "--port", "0"],
      { env, cwd: repo, stdio: ["ignore", "pipe", "pipe"] },
    );
    // Kept, not just drained: a fetch that fails is all the page can say, so
    // the dashboard's own words (a panic, a bind error) are what explains a
    // red run. Both streams are read continuously so a chatty server never
    // blocks on a full pipe.
    const output: Output = { text: "" };
    for (const stream of [server.stdout, server.stderr]) {
      stream.setEncoding("utf8");
      stream.on("data", (chunk: string) => {
        output.text = (output.text + chunk).slice(-OUTPUT_KEEP);
      });
    }

    try {
      const url = await waitForUrl(server, output);
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
      // A dashboard that died on its own takes the page with it, and the test
      // would otherwise report some later assertion as the cause.
      if (server.exitCode !== null || server.signalCode !== null) {
        throw new Error(`the dashboard died mid-test (${describe(server)}): ${tail(output)}`);
      }
    } finally {
      reportOutput(testInfo, output);
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
