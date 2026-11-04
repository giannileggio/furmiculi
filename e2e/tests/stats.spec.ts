// The Stats view: what the finished tasks add up to, under the same toolbar as
// the lists.
import * as fs from "node:fs";
import * as path from "node:path";
import { expect, openDashboard, test, view, type Forge } from "./fixtures";

const minutesAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString();

/** Move a waiting task into the archive, finished as told. */
function finish(forge: Forge, id: string, status: string, ranSecs: number, extra = "", endedMinutesAgo = 5) {
  const from = path.join(forge.home, "tasks", `${id}.md`);
  const end = minutesAgo(endedMinutesAgo);
  const start = new Date(Date.parse(end) - ranSecs * 1000).toISOString();
  fs.writeFileSync(
    path.join(forge.home, "archive", `${id}.md`),
    fs
      .readFileSync(from, "utf8")
      .replace(/^status: .*$/m, `status: ${status}`)
      .replace(/^(created_at: .*)$/m, `$1\nstarted_at: ${start}\nfinished_at: ${end}${extra}`),
  );
  fs.rmSync(from);
}

const tile = (page: import("@playwright/test").Page, name: string) =>
  page.locator(".tile", { has: page.locator(".name", { hasText: name }) });

test("adds up the finished tasks, and follows the toolbar", async ({ page, forge }) => {
  forge.add("one", "true", "--agent", "stub");
  forge.add("two", "true", "--agent", "stub");
  forge.add("three", "true", "--agent", "stub2");
  forge.add("old", "true", "--agent", "stub2");
  forge.add("waiting", "true");
  finish(forge, "one", "done", 60, "\ntokens_in: 1000\ntokens_out: 500");
  finish(forge, "two", "done", 180);
  finish(forge, "three", "failed", 120);
  finish(forge, "old", "cancelled", 10, "", 3 * 24 * 60);

  await openDashboard(page, forge);
  await view(page, "stats").click();
  await expect(page).toHaveURL(/\/views\/stats$/);
  await expect(page.locator("#view-title")).toHaveText("Stats");

  await expect(tile(page, "Finished").locator(".value")).toHaveText("4");
  await expect(tile(page, "Finished").locator(".sub")).toHaveText("2 done, 1 failed, 1 other");
  // Cancelled says nothing about whether the work succeeds.
  await expect(tile(page, "Success rate").locator(".value")).toHaveText("67%");
  await expect(tile(page, "Median run time").locator(".value")).toHaveText("1m 30s");
  await expect(tile(page, "Tokens").locator(".value")).toHaveText("1.5K");
  await expect(tile(page, "Tokens").locator(".sub")).toContainText("Reported by 1 of 4");

  const agents = page.locator(".stat-block", { hasText: "By agent" });
  await expect(agents.locator("tbody tr")).toHaveCount(2);
  await expect(agents.locator("tbody tr").filter({ has: page.getByRole("cell", { name: "stub2", exact: true }) }).locator('td[data-label="Failed"]')).toHaveText("1");

  // A bar's numbers are one hover away.
  const bars = page.locator(".col .stack");
  await expect(bars.first()).toBeVisible();
  await bars.last().hover();
  await expect(page.locator("#stats-tip")).toBeVisible();
  await expect(page.locator("#stats-tip")).toContainText("tasks");

  // The time window narrows it, and the address keeps it.
  await page.locator("#age").selectOption("24h");
  await expect(tile(page, "Finished").locator(".value")).toHaveText("3");
  await expect(page).toHaveURL(/\/views\/stats\?age=24h$/);
  await page.reload();
  await expect(page.locator("#view-title")).toHaveText("Stats");
  await expect(tile(page, "Finished").locator(".value")).toHaveText("3");

  // So does the search.
  await page.locator("#search").fill("three");
  await expect(tile(page, "Finished").locator(".value")).toHaveText("1");
  await expect(page.locator("#toolbar-count")).toHaveText("1 of 4");
});

test("a command task counts, but has no model to report", async ({ page, forge }) => {
  forge.furmi("add", "--repo", forge.repo, "--id", "cmd", "--run", "true");
  finish(forge, "cmd", "done", 30);
  await openDashboard(page, forge);
  await view(page, "stats").click();
  await expect(tile(page, "Finished").locator(".value")).toHaveText("1");
  await expect(page.locator(".stat-block", { hasText: "By agent" })).toContainText("command");
  await expect(page.locator(".stat-block", { hasText: "By model" })).toHaveCount(0);
});

test("says so when nothing has finished", async ({ page, forge }) => {
  forge.add("waiting", "true");
  await openDashboard(page, forge);
  await view(page, "stats").click();
  await expect(page.locator("#panel-stats")).toContainText("Nothing has finished yet.");
  await expect(page.locator("#select-toggle")).toBeHidden();
});
