// Drafts (saved, not queued) and templates (tasks to start new ones from). The
// runner is off, so a task that is queued stays put and a draft is told apart
// from it by its list, not by anything happening.
import * as fs from "node:fs";
import { createdId, count, expect, openDashboard, row, tab, test } from "./fixtures";

test("the form can save a draft, which waits in its own list until it is queued", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo look this over first");
  await page.locator("#f-draft").click();
  const id = await createdId(page, "Saved the draft");

  // Not waiting: the Drafts list has it, the Waiting list doesn't.
  expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("status: draft");
  await expect(count(page, "drafts")).toHaveText("1");
  await expect(count(page, "queue")).toHaveText("0");
  await expect(row(page, id).locator(".badge")).toHaveText("Draft");

  // It can be edited like any task that hasn't started.
  await row(page, id).getByRole("button", { name: "Edit" }).click();
  await page.locator("#f-prompt").fill("echo second thoughts");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText(`Saved ${id}`);
  expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("echo second thoughts");
  expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("status: draft");

  await row(page, id).getByRole("button", { name: "Queue" }).click();
  await expect(count(page, "drafts")).toHaveText("0");
  await expect(count(page, "queue")).toHaveText("1");
  expect(fs.readFileSync(forge.taskFile(id)!, "utf8")).toContain("status: pending");
});

test("a draft made on the command line shows up, and discarding it deletes it", async ({ page, forge }) => {
  forge.add("from-cli", "echo hi", "--draft");
  await openDashboard(page, forge);
  await tab(page, "drafts").click();
  await expect(row(page, "from-cli").locator(".badge")).toHaveText("Draft");

  page.once("dialog", (d) => {
    expect(d.message()).toContain("Discard the draft");
    void d.accept();
  });
  await row(page, "from-cli").getByRole("button", { name: "Discard" }).click();
  await expect(row(page, "from-cli")).toHaveCount(0);
  expect(forge.taskFile("from-cli")).toBeUndefined();
});

test("a runner leaves drafts alone", async ({ page, forge }) => {
  forge.add("not-yet", "echo should not run", "--draft");
  forge.furmi("run", "--once");
  expect(fs.readFileSync(forge.taskFile("not-yet")!, "utf8")).toContain("status: draft");
  await openDashboard(page, forge);
  await expect(count(page, "drafts")).toHaveText("1");
});

test("any task can be duplicated into a draft or saved as a template, then used", async ({ page, forge }) => {
  forge.add("original", "echo the original prompt");
  await openDashboard(page, forge);
  await page.locator("#panel-queue tbody tr", { hasText: "original" }).locator(".summary").click();
  await page.getByRole("button", { name: "Duplicate as new draft" }).click();
  await expect(page.locator("#toast")).toContainText("Saved the draft");
  await expect(count(page, "drafts")).toHaveText("1");
  const copies = fs.readdirSync(`${forge.home}/tasks`).filter((f) => f !== "original.md");
  expect(copies).toHaveLength(1);
  expect(fs.readFileSync(`${forge.home}/tasks/${copies[0]}`, "utf8")).toContain("status: draft");

  // Saved as a template, it is on the Templates page, next to the starters.
  await tab(page, "queue").click();
  await page.locator("#panel-queue tbody tr", { hasText: "original" }).locator(".summary").click();
  await page.getByRole("button", { name: "Save as template" }).click();
  await expect(page.locator("#toast")).toHaveText("Saved the template original");
  await page.locator("#task-close").click();
  await tab(page, "templates").click();
  const templates = page.locator("#panel-templates tbody tr");
  await expect(templates.filter({ hasText: "fix-failing-test" })).toBeVisible();
  await expect(templates.filter({ hasText: "original" })).toBeVisible();

  // Using it opens the New task form filled in, and can save a draft.
  await templates.filter({ hasText: "original" }).getByRole("button", { name: "Use" }).click();
  await expect(page.locator("#new-title")).toHaveText("New task from original");
  await expect(page.locator("#f-prompt")).toHaveValue("echo the original prompt");
  await expect(page.locator("#f-repo")).toHaveValue("~/repo"); // HOME is the test root
  await page.locator("#f-draft").click();
  await createdId(page, "Saved the draft");
  await expect(count(page, "drafts")).toHaveText("2");
});

test("a starter template needs a project, which the form asks for", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await tab(page, "templates").click();
  await expect(row(page, "review-the-diff")).toContainText("you choose");
  await row(page, "review-the-diff").getByRole("button", { name: "Use" }).click();
  await expect(page.locator("#f-prompt")).toHaveValue(/Review the changes/);
  await expect(page.locator("#f-finish")).toHaveValue("none");
  await page.locator("#f-repo").fill(forge.repo);
  await page.locator("#f-submit").click();
  const id = await createdId(page, "Queued");
  const text = fs.readFileSync(forge.taskFile(id)!, "utf8");
  expect(text).toContain("status: pending");
  expect(text).toContain("Review the changes");
});

test("a template can be deleted from its page", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await tab(page, "templates").click();
  page.once("dialog", (d) => void d.accept());
  await row(page, "review-the-diff").getByRole("button", { name: "Delete" }).click();
  await expect(row(page, "review-the-diff")).toHaveCount(0);
  expect(fs.existsSync(`${forge.home}/templates/review-the-diff.md`)).toBe(false);
});

test("a template can be edited, and keeps what the form doesn't show", async ({ page, forge }) => {
  fs.writeFileSync(
    `${forge.home}/templates/tuned.md`,
    "---\nid: tuned\ndescription: Tuned by hand\nmodel: a-model\non_finish: none\n---\n\nFirst version.\n",
  );
  await openDashboard(page, forge);
  await tab(page, "templates").click();
  const tuned = page.locator("#panel-templates tbody tr", { hasText: "tuned" });
  await tuned.getByRole("button", { name: "Edit" }).click();
  await expect(page.locator("#new-title")).toHaveText("Edit the template tuned");
  await expect(page.locator("#f-desc")).toHaveValue("Tuned by hand");
  await expect(page.locator("#f-prompt")).toHaveValue("First version.");
  await expect(page.locator("#f-repo")).toHaveValue(""); // no project, and none forced on it
  await expect(page.locator("#f-start-wrap")).toBeHidden();
  await expect(page.locator("#f-draft")).toBeHidden();

  await page.locator("#f-desc").fill("Tuned again");
  await page.locator("#f-prompt").fill("Second version.");
  await page.locator("#f-finish").selectOption("commit");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText("Saved the template tuned");
  const text = fs.readFileSync(`${forge.home}/templates/tuned.md`, "utf8");
  expect(text).toContain("Second version.");
  expect(text).toContain("description: Tuned again");
  expect(text).toContain("on_finish: commit");
  expect(text).toContain("model: a-model");
  await expect(tuned).toContainText("Tuned again");
});

test("a template can be written from scratch, with or without a project", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await tab(page, "templates").click();
  await page.locator("#tpl-new").click();
  await expect(page.locator("#new-title")).toHaveText("New template");
  await expect(page.locator("#f-submit")).toHaveText("Create template");
  await page.locator("#f-desc").fill("Check the docs");
  await page.locator("#f-prompt").fill("Read the docs and list what is out of date.");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText("Created the template check-the-docs");
  const text = fs.readFileSync(`${forge.home}/templates/check-the-docs.md`, "utf8");
  expect(text).toContain("Read the docs and list what is out of date.");
  expect(text).not.toContain("repo:");
  await expect(page.locator("#panel-templates tbody tr", { hasText: "check-the-docs" })).toContainText("you choose");

  // The button belongs to this page only.
  await tab(page, "queue").click();
  await expect(page.locator("#tpl-new")).toBeHidden();

  // An empty prompt is refused, and nothing is written.
  await tab(page, "templates").click();
  const before = fs.readdirSync(`${forge.home}/templates`).length;
  await page.locator("#tpl-new").click();
  await page.locator("#f-prompt").fill("   ");
  await page.locator("#f-submit").click();
  await expect(page.locator("#f-error")).not.toHaveText("");
  expect(fs.readdirSync(`${forge.home}/templates`)).toHaveLength(before);
});
