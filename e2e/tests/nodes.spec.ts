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
