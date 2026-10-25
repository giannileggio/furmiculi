// Managing work that hasn't started: the runner switch, the new-task form and
// editing or cancelling a waiting task. The runner is off so tasks stay put.
import * as fs from "node:fs";
import { createdId, expect, openDashboard, row, tab, test } from "./fixtures";

test("with the runner off, a banner offers to start it, and it can be stopped again", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await expect(page.locator("#runner")).toHaveText("runner: off");
  await expect(page.locator("#runner-notice")).toBeVisible();

  await page.getByRole("button", { name: "Start the runner" }).click();
  await expect(page.locator("#toast")).toHaveText("Runner started");
  await expect(page.locator("#runner")).toContainText("runner: on");
  await expect(page.locator("#runner-notice")).toBeHidden();
  expect(forge.furmi("status")).toContain("runner     running (pid");

  // Stopping asks first; declining changes nothing.
  page.once("dialog", (d) => void d.dismiss());
  await page.locator("#runner").getByRole("button", { name: "stop" }).click();
  await expect(page.locator("#runner")).toContainText("runner: on");

  page.once("dialog", (d) => void d.accept());
  await page.locator("#runner").getByRole("button", { name: "stop" }).click();
  await expect(page.locator("#runner")).toHaveText("runner: off");
  await expect(page.locator("#runner-notice")).toBeVisible();
  expect(forge.furmi("status")).toContain("runner     NOT running");
});

test("a waiting task can be edited, then cancelled and tried again", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo first version");
  await page.locator("#f-submit").click();
  const id = await createdId(page, "Queued");

  await expect(row(page, id).locator(".badge")).toHaveText("Waiting");
  await expect(page.locator("#c-queue")).toHaveText("1");

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
  await expect(row(page, id).locator("td").nth(2)).not.toHaveText(/^\s*$/); // it has a start time now

  page.once("dialog", (d) => void d.accept());
  await row(page, id).getByRole("button", { name: "Drop it" }).click();
  await expect(page.locator("#c-queue")).toHaveText("0");
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
  await expect(page.locator("#c-queue")).toHaveText("0");

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
  await expect(page.locator("#c-queue")).toHaveText("1");
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
  await expect(page.locator("#c-queue")).toHaveText("2");
});
