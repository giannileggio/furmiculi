// The "Which machine?" choice only exists once another machine is merged in;
// on its own, the form is exactly the old one.
import { expect, openDashboard, tab, test } from "./fixtures";

test("with no other machines the form has no machine choice and still submits", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await expect(page.locator("#f-node-wrap")).toBeHidden();
  await page.locator("#f-repeat").selectOption("daily");
  await expect(page.locator("#f-submit")).toBeEnabled();
  await page.locator("#f-repeat").selectOption("once");
  await expect(page.locator("#f-submit")).toBeEnabled();
});

test("another machine's task opens its log from that machine", async ({ page, forge }) => {
  // No machine at the other end: the node's answers are canned, and the log
  // has to come through the same ssh proxy every other view of a node uses.
  forge.fakeNode("box", {
    state: {
      home: "~/home",
      generated_at: new Date().toISOString(),
      counts: { pending: 0, running: 0, done: 1, failed: 0, cancelled: 0, needs_review: 0, schedules: 0, invalid: 0 },
      queue: [],
      running: [],
      archive: [{
        id: "remote-task",
        status: "done",
        when: "2026-10-30 09:00",
        when_at: "2026-10-30T09:00:00+00:00",
        repo: "~/app",
        branch: "main",
        agent: "claude",
        model: "claude-sonnet-4",
        model_source: "agent",
        summary: "ran on the other machine",
        error: null,
      }],
      schedules: [],
      invalid: [],
      runner: { alive: true, last_tick: null },
      form: { repos: [], agents: ["claude"], agent: "claude", on_finish: "none" },
    },
    logs: { log: "hello-from-the-other-machine\n", truncated: false },
  });
  await openDashboard(page, forge);
  await tab(page, "archive").click();

  const remote = page.locator('tbody tr[data-id="remote-task@box"]');
  await expect(remote).toBeVisible();
  await expect(remote).toContainText("box");
  await remote.getByRole("button", { name: "Output" }).click();

  await expect(page.locator("#logs-title")).toHaveText("remote-task@box");
  await expect(page.locator("#logs-body")).toContainText("hello-from-the-other-machine");
});

// ---- "Any machine (least busy)" and its model box
//
// Which machine gets the task is only settled when it is queued, and each
// machine has its own agent with its own CLI. So the model box asks every
// machine that could take the task, in the agent that machine would run it
// with, and offers what they can run between them.
const PROJECT = "github.com/me/app";

/** A machine with nothing on it: enough to be merged into the page's state. */
function idleState() {
  return {
    home: "~/home",
    generated_at: new Date().toISOString(),
    counts: { pending: 0, running: 0, done: 0, failed: 0, cancelled: 0, needs_review: 0, schedules: 0, invalid: 0 },
    queue: [],
    running: [],
    archive: [],
    schedules: [],
    invalid: [],
    runner: { alive: true, last_tick: null },
    form: { repos: [], agents: [], agent: null, on_finish: "none" },
  };
}

/** A machine with a clone of the project and one agent, answering as it would. */
function machine(agent: string, models: string[], canPin = true) {
  return {
    state: idleState(),
    logs: { log: "", truncated: false },
    repos: {
      repos: [{ remote: PROJECT, path: "~/app" }],
      agents: [agent],
      agent,
      running: 0,
      max_parallel: 2,
    },
    models: {
      agent,
      can_pin: canPin,
      default: canPin ? models[0] : null,
      models,
      source: canPin ? "cli" : "none",
    },
  };
}

test("with 'any machine' the model box offers what every machine that could take it can run", async ({ page, forge }) => {
  forge.fakeNode("box", machine("claude", ["claude-sonnet-4", "claude-opus-4"]));
  forge.fakeNode("mini", machine("codex", ["gpt-5", "gpt-5-mini"]));
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-node").selectOption("auto");

  // The form starts from the project both machines have...
  await expect(page.locator("#f-repo")).toHaveValue(PROJECT);
  // ...and offers the models of both, since either could get the task.
  await expect(page.locator("#f-model-wrap")).toBeVisible();
  await expect(page.locator('#f-models option[value="claude-opus-4"]')).toHaveCount(1);
  await expect(page.locator('#f-models option[value="gpt-5-mini"]')).toHaveCount(1);
  await expect(page.locator("#f-model-hint")).toContainText("box and mini");
});

test("with 'any machine' a machine that can't be handed a model takes the box away", async ({ page, forge }) => {
  // Aider has no `model_args`: a pinned model would make the task unrunnable on
  // whichever machine got it, so the box goes rather than offer one that may
  // land where it can't be passed on.
  forge.fakeNode("box", machine("claude", ["claude-sonnet-4"]));
  forge.fakeNode("mini", machine("aider", [], false));
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-node").selectOption("auto");
  await expect(page.locator("#f-model-wrap")).toBeHidden();
});
