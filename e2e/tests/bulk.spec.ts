// Ticking several rows on the Repeating and Templates lists and acting on them
// together, and "Run now" for one row or many: a task goes in the queue at
// once, and the schedule keeps its own times.
import * as fs from "node:fs";
import * as path from "node:path";
import type { Page } from "@playwright/test";
import { count, expect, openDashboard, tab, test } from "./fixtures";

// The row of one item in the list on screen. Every list is in the page, hidden
// or not, and a task queued from a schedule can carry the same words in its id.
function listRow(page: Page, list: "schedules" | "templates", id: string) {
  return page.locator(`#panel-${list} tbody tr`, { hasText: id });
}

function writeSchedule(home: string, id: string, repo: string, enabled = true) {
  fs.mkdirSync(path.join(home, "schedules"), { recursive: true });
  fs.writeFileSync(
    path.join(home, "schedules", `${id}.md`),
    `---\nid: ${id}\ncron: "0 9 * * *"\nrepo: ${repo}\nenabled: ${enabled}\n---\n\nRun ${id}\n`,
  );
}

function writeTemplate(home: string, id: string, repoLine: string) {
  fs.writeFileSync(
    path.join(home, "templates", `${id}.md`),
    `---\nid: ${id}\ndescription: Made for ${id}\n${repoLine}\non_finish: none\n---\n\nDo ${id}\n`,
  );
}

test("ticking repeating tasks offers run now, pause, resume and delete, and acts on each", async ({ page, forge }) => {
  writeSchedule(forge.home, "nightly", forge.repo);
  writeSchedule(forge.home, "weekly", forge.repo);
  writeSchedule(forge.home, "hourly", forge.repo, false);
  await openDashboard(page, forge);
  await tab(page, "schedules").click();

  await page.locator("#select-toggle").click();
  const bar = page.locator("#bulk");
  await expect(bar).toContainText("0 selected");
  await listRow(page, "schedules", "nightly").locator("input[data-pick]").check();
  await listRow(page, "schedules", "weekly").locator("input[data-pick]").check();
  await expect(bar).toContainText("2 selected");
  // Both are running, so the bar offers to pause them, not to resume.
  await expect(bar.locator('button[data-bulk="run"]')).toBeVisible();
  await expect(bar.locator('button[data-bulk="pause"]')).toBeVisible();
  await expect(bar.locator('button[data-bulk="resume"]')).toHaveCount(0);

  await bar.locator('button[data-bulk="run"]').click();
  await expect(page.locator("#toast")).toHaveText("Run now: 2 schedules");
  await expect(count(page, "queue")).toHaveText("2");
  await expect(bar).toContainText("0 selected");
  // The schedules themselves are untouched: still active, still on their own times.
  await expect(listRow(page, "schedules", "nightly")).toContainText("Active");

  await listRow(page, "schedules", "nightly").locator("input[data-pick]").check();
  await listRow(page, "schedules", "weekly").locator("input[data-pick]").check();
  await bar.locator('button[data-bulk="pause"]').click();
  await expect(listRow(page, "schedules", "nightly")).toContainText("Paused");
  await expect(listRow(page, "schedules", "weekly")).toContainText("Paused");

  // Only paused ones now: the bar offers to resume them, not to pause.
  await listRow(page, "schedules", "hourly").locator("input[data-pick]").check();
  await expect(bar.locator('button[data-bulk="pause"]')).toHaveCount(0);
  await bar.locator('button[data-bulk="resume"]').click();
  await expect(listRow(page, "schedules", "hourly")).toContainText("Active");

  await listRow(page, "schedules", "weekly").locator("input[data-pick]").check();
  await listRow(page, "schedules", "hourly").locator("input[data-pick]").check();
  await bar.locator('button[data-bulk="delete"]').click();
  await expect(page.locator("#ask-title")).toHaveText("Delete 2 schedules?");
  await page.locator("#ask-ok").click();
  await expect(listRow(page, "schedules", "weekly")).toHaveCount(0);
  await expect(listRow(page, "schedules", "hourly")).toHaveCount(0);
  await expect(listRow(page, "schedules", "nightly")).toBeVisible();
  expect(fs.existsSync(path.join(forge.home, "schedules", "weekly.md"))).toBe(false);
  expect(fs.existsSync(path.join(forge.home, "schedules", "hourly.md"))).toBe(false);
  expect(fs.existsSync(path.join(forge.home, "schedules", "nightly.md"))).toBe(true);
});

test("a single repeating task can be queued now from its own row", async ({ page, forge }) => {
  writeSchedule(forge.home, "nightly", forge.repo);
  await openDashboard(page, forge);
  await tab(page, "schedules").click();

  await listRow(page, "schedules", "nightly").getByRole("button", { name: "Run now" }).click();
  await expect(page.locator("#toast")).toHaveText("Run now: nightly");
  await expect(count(page, "queue")).toHaveText("1");
  await expect(listRow(page, "schedules", "nightly")).toContainText("Active");
});

test("the header box ticks every repeating task in the list", async ({ page, forge }) => {
  writeSchedule(forge.home, "nightly", forge.repo);
  writeSchedule(forge.home, "weekly", forge.repo);
  await openDashboard(page, forge);
  await tab(page, "schedules").click();

  await page.locator("#select-toggle").click();
  await page.locator("#panel-schedules input[data-pick-all]").check();
  await expect(page.locator("#bulk")).toContainText("2 selected");
  await page.locator("#panel-schedules input[data-pick-all]").uncheck();
  await expect(page.locator("#bulk")).toContainText("0 selected");
});

test("templates can be ticked and deleted, and queued now only where they name a project", async ({ page, forge }) => {
  writeTemplate(forge.home, "with-repo", `repo: ${forge.repo}`);
  writeTemplate(forge.home, "no-repo", "");
  await openDashboard(page, forge);
  await tab(page, "templates").click();

  // A template with no project has no "Run now", in the row or the bar.
  await expect(listRow(page, "templates", "no-repo").getByRole("button", { name: "Run now" })).toHaveCount(0);
  await expect(listRow(page, "templates", "with-repo").getByRole("button", { name: "Run now" })).toBeVisible();

  await page.locator("#select-toggle").click();
  const bar = page.locator("#bulk");
  await listRow(page, "templates", "with-repo").locator("input[data-pick]").check();
  await listRow(page, "templates", "no-repo").locator("input[data-pick]").check();
  await expect(bar).toContainText("2 selected");
  await expect(bar.locator('button[data-bulk="run"]')).toHaveCount(0);
  await expect(bar.locator('button[data-bulk="delete"]')).toBeVisible();

  await listRow(page, "templates", "no-repo").locator("input[data-pick]").uncheck();
  await expect(bar.locator('button[data-bulk="run"]')).toBeVisible();
  await bar.locator('button[data-bulk="run"]').click();
  await expect(page.locator("#toast")).toHaveText("Run now: 1 template");
  await expect(count(page, "queue")).toHaveText("1");

  // Deleting the two: the template files go, and the tasks made from them stay.
  // (Queuing cleared the ticks, so both are ticked again.)
  await listRow(page, "templates", "with-repo").locator("input[data-pick]").check();
  await listRow(page, "templates", "no-repo").locator("input[data-pick]").check();
  await bar.locator('button[data-bulk="delete"]').click();
  await expect(page.locator("#ask-title")).toHaveText("Delete 2 templates?");
  await page.locator("#ask-ok").click();
  await expect(listRow(page, "templates", "with-repo")).toHaveCount(0);
  await expect(listRow(page, "templates", "no-repo")).toHaveCount(0);
  await expect(count(page, "queue")).toHaveText("1");
  expect(fs.existsSync(path.join(forge.home, "templates", "with-repo.md"))).toBe(false);
  expect(fs.existsSync(path.join(forge.home, "templates", "no-repo.md"))).toBe(false);
});
