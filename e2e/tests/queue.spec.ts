// Managing work that hasn't started: the runner switch, the new-task form and
// editing or cancelling a waiting task. The runner is off so tasks stay put.
import * as fs from "node:fs";
import { createdId, expect, openDashboard, row, tab, test, count } from "./fixtures";

test("with the runner off, a banner offers to start it, and it can be stopped again", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await expect(page.locator("#runner")).toContainText("Runner off");
  await expect(page.locator("#runner-notice")).toBeVisible();

  await page.getByRole("button", { name: "Start the runner" }).click();
  await expect(page.locator("#toast")).toHaveText("Runner started");
  await expect(page.locator("#runner")).toContainText("Runner on");
  await expect(page.locator("#runner-notice")).toBeHidden();
  expect(forge.furmi("status")).toContain("runner     running (pid");

  // Stopping asks first; declining changes nothing.
  page.once("dialog", (d) => void d.dismiss());
  await page.locator("#runner").getByRole("button", { name: "Stop" }).click();
  await expect(page.locator("#runner")).toContainText("Runner on");

  page.once("dialog", (d) => void d.accept());
  await page.locator("#runner").getByRole("button", { name: "Stop" }).click();
  await expect(page.locator("#runner")).toContainText("Runner off");
  await expect(page.locator("#runner-notice")).toBeVisible();
  expect(forge.furmi("status")).toContain("runner     NOT running");
});

test("a command task shows as a command, and its command can be edited", async ({ page, forge }) => {
  forge.furmi("add", "--repo", forge.repo, "--id", "chore", "--run", "echo first");
  await openDashboard(page, forge);

  const chore = row(page, "chore");
  await expect(chore).toContainText("command");
  await expect(chore).toContainText("echo first");

  await chore.getByRole("button", { name: "Edit" }).click();
  await expect(page.locator("#f-prompt-label")).toHaveText("What command should it run?");
  await expect(page.locator("#f-prompt")).toHaveValue("echo first");
  await expect(page.locator("#f-agent-wrap")).toBeHidden();
  // A command runs no agent, so it has no mode to run in. What it does have
  // is how often it may be retried and how long it gets.
  await expect(page.locator("#f-mode-wrap")).toBeHidden();
  await expect(page.locator("#f-retries-wrap")).toBeVisible();
  await expect(page.locator("#f-timeout-wrap")).toBeVisible();
  await page.locator("#f-prompt").fill("echo second");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText("Saved chore");

  const file = fs.readFileSync(`${forge.home}/tasks/chore.md`, "utf8");
  expect(file).toContain("run: echo second");
  expect(file).not.toContain("agent:");
  await expect(chore).toContainText("echo second");

  // The task's own view names the command, not an agent and a model.
  await chore.locator(".summary").click();
  await expect(page.locator("#task-body")).toContainText("The command it runs");
  await expect(page.locator("#task-body")).toContainText("echo second");
  await expect(page.locator("#task-body dt", { hasText: "Model" })).toHaveCount(0);
});

test("a waiting task can be edited, then cancelled and tried again", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo first version");
  await page.locator("#f-submit").click();
  const id = await createdId(page, "Queued");

  await expect(row(page, id).locator(".badge")).toHaveText("Waiting");
  await expect(count(page, "queue")).toHaveText("1");

  await row(page, id).getByRole("button", { name: "Edit" }).click();
  await expect(page.locator("#new-title")).toHaveText(`Edit ${id}`);
  await expect(page.locator("#f-prompt")).toHaveValue("echo first version");
  await expect(page.locator("#f-submit")).toHaveText("Save changes");
  await expect(page.locator("#f-repeat-wrap")).toBeHidden(); // a task can't become a schedule
  await page.locator("#f-prompt").fill("echo second version");
  await page.locator("#f-start").selectOption("3h");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText(`Saved ${id}`);
  expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("echo second version");
  await expect(row(page, id).locator("td").nth(4)).not.toHaveText(/^\s*$/); // it has a start time now

  page.once("dialog", (d) => void d.accept());
  await row(page, id).getByRole("button", { name: "Drop it" }).click();
  await expect(count(page, "queue")).toHaveText("0");
  await tab(page, "archive").click();
  await expect(row(page, id).locator(".badge")).toHaveText("Cancelled");
  await expect(row(page, id).getByRole("button", { name: "Edit" })).toHaveCount(0); // only waiting tasks

  await row(page, id).getByRole("button", { name: "Try again" }).click();
  await tab(page, "queue").click();
  await expect(row(page, id).locator(".badge")).toHaveText("Waiting");
});

test("the form rejects bad input and keeps what you typed", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();

  // The browser itself refuses an empty prompt.
  await page.locator("#f-submit").click();
  await expect(page.locator("#new-overlay")).toHaveClass(/active/);
  expect(await page.locator("#f-prompt").evaluate((el: HTMLTextAreaElement) => el.validity.valueMissing)).toBe(true);

  // The server refuses a folder that isn't a git repo, and the form says so.
  await page.locator("#f-prompt").fill("do something");
  await page.locator("#f-repo").fill("/definitely/not/a/repo");
  await page.locator("#f-submit").click();
  await expect(page.locator("#f-error")).not.toBeEmpty();
  await expect(page.locator("#new-overlay")).toHaveClass(/active/);
  await expect(page.locator("#f-submit")).toBeEnabled();
  await expect(count(page, "queue")).toHaveText("0");

  // Closing without queueing keeps the draft for next time.
  await page.keyboard.press("Escape");
  await expect(page.locator("#new-overlay")).not.toHaveClass(/active/);
  await page.locator("#new-task-open").click();
  await expect(page.locator("#f-prompt")).toHaveValue("do something");
  await page.locator("#new-cancel").click();
});

test("the form's keyboard shortcut submits and the tabs switch panels", async ({ page, forge }) => {
  forge.add("already-here", "echo hi");
  await openDashboard(page, forge);
  await expect(count(page, "queue")).toHaveText("1");
  await expect(page.locator("#panel-queue")).toBeVisible();
  await expect(page.locator("#panel-archive")).toBeHidden();

  await tab(page, "archive").click();
  await expect(page.locator("#panel-archive")).toBeVisible();
  await expect(page.locator("#panel-queue")).toBeHidden();
  await expect(page.getByText("Nothing has finished yet.")).toBeVisible();

  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo via shortcut");
  await page.locator("#f-prompt").press("Control+Enter");
  await expect(page.locator("#toast")).toContainText("Queued ");
  await expect(count(page, "queue")).toHaveText("2");
});

test("the form sets how a task runs, and an emptied field follows the defaults", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();

  // Nothing pinned: every one of these is the defaults' to decide.
  await expect(page.locator("#f-mode")).toHaveValue("");
  await expect(page.locator("#f-worktree")).toHaveValue("");
  await expect(page.locator("#f-branch")).toHaveValue("");
  await expect(page.locator("#f-retries")).toHaveValue("");

  // A task with no project has no branch and no worktree to set.
  await page.locator("#f-scratch").check();
  await expect(page.locator("#f-worktree-wrap")).toBeHidden();
  await expect(page.locator("#f-branch-wrap")).toBeHidden();
  await expect(page.locator("#f-timeout-wrap")).toBeVisible();
  await page.locator("#f-scratch").uncheck();
  await expect(page.locator("#f-worktree-wrap")).toBeVisible();

  await page.locator("#f-prompt").fill("echo run it differently");
  await page.locator("#f-mode").selectOption("interactive");
  await page.locator("#f-worktree").selectOption("yes");
  await page.locator("#f-branch").fill("fix/login");
  await page.locator("#f-retries").fill("2");
  await page.locator("#f-delay").fill("10m");
  await page.locator("#f-timeout").fill("45m");
  await page.locator("#f-submit").click();
  const id = await createdId(page, "Queued");

  let text = fs.readFileSync(forge.taskFile(id)!, "utf8");
  expect(text).toContain("mode: interactive");
  expect(text).toContain("worktree: true");
  expect(text).toContain("branch: fix/login");
  expect(text).toContain("retries: 2");
  expect(text).toContain("retry_delay: 10m");
  expect(text).toContain("timeout: 45m");

  // The edit form starts from what the file pins.
  await row(page, id).getByRole("button", { name: "Edit" }).click();
  await expect(page.locator("#f-mode")).toHaveValue("interactive");
  await expect(page.locator("#f-worktree")).toHaveValue("yes");
  await expect(page.locator("#f-branch")).toHaveValue("fix/login");
  await expect(page.locator("#f-retries")).toHaveValue("2");
  await expect(page.locator("#f-delay")).toHaveValue("10m");
  await expect(page.locator("#f-timeout")).toHaveValue("45m");

  // Emptying one unpins it, so `[defaults]` decides again.
  await page.locator("#f-mode").selectOption("");
  await page.locator("#f-worktree").selectOption("");
  await page.locator("#f-branch").fill("");
  await page.locator("#f-retries").fill("");
  await page.locator("#f-delay").fill("");
  await page.locator("#f-timeout").fill("");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText(`Saved ${id}`);
  text = fs.readFileSync(forge.taskFile(id)!, "utf8");
  for (const key of ["mode:", "worktree:", "branch:", "retries:", "retry_delay:", "timeout:"]) {
    expect(text).not.toContain(key);
  }
});

test("a duration the file can't hold is refused, and kept for another try", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo soon");
  await page.locator("#f-timeout").fill("whenever");
  await page.locator("#f-submit").click();
  await expect(page.locator("#f-error")).toContainText("as a timeout");
  await expect(page.locator("#new-overlay")).toHaveClass(/active/);
  await expect(count(page, "queue")).toHaveText("0");

  await page.locator("#f-timeout").fill("30m");
  await page.locator("#f-submit").click();
  const id = await createdId(page, "Queued");
  expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("timeout: 30m");
});

// A model is the agent's to know — its CLI's version, the providers signed in —
// so the form asks that CLI (here a `printf`) and offers what it says. An agent
// that can't be handed a model gets no box at all.
test.describe("choosing a model", () => {
  test.use({
    config: `
[agents.modeled]
headless = ["sh", "-c", "{prompt}"]
interactive = ["sh", "-c", "{prompt}"]
model_args = ["--model", "{model}"]
models_cmd = ["printf", "p/one\\np/two\\n"]
`,
  });

  test("the form offers the agent's models and remembers the one picked", async ({ page, forge }) => {
    await openDashboard(page, forge);
    await page.locator("#new-task-open").click();

    // The stub agents have no `model_args`, so there is no model to offer.
    await expect(page.locator("#f-agent")).toHaveValue("stub");
    await expect(page.locator("#f-model-wrap")).toBeHidden();

    await page.locator("#f-agent").selectOption("modeled");
    await expect(page.locator("#f-model-wrap")).toBeVisible();
    await expect(page.locator('#f-models option[value="p/two"]')).toHaveCount(1);

    await page.locator("#f-prompt").fill("echo with a model");
    await page.locator("#f-model").fill("p/two");
    await page.locator("#f-submit").click();
    const id = await createdId(page, "Queued");
    expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("model: p/two");

    // The edit form starts from what the task pins, and an empty box means
    // "the agent's own model": the pin goes away.
    await row(page, id).getByRole("button", { name: "Edit" }).click();
    await expect(page.locator("#f-model")).toHaveValue("p/two");
    await page.locator("#f-model").fill("");
    await page.locator("#f-submit").click();
    await expect(page.locator("#toast")).toHaveText(`Saved ${id}`);
    expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).not.toContain("model:");
  });
});
