// Repeating tasks: create, read back in plain words, edit, pause, delete.
import * as fs from "node:fs";
import * as path from "node:path";
import { createdId, expect, openDashboard, row, tab, test, count } from "./fixtures";

test("a schedule can be created, edited, paused, resumed and deleted", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await tab(page, "schedules").click();
  await expect(page.getByText("No repeating tasks yet.")).toBeVisible();

  // Create: any "How often?" other than "Just once" makes a schedule.
  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo nightly");
  await page.locator("#f-repeat").selectOption("weekdays");
  await expect(page.locator("#new-title")).toHaveText("New repeating task");
  await expect(page.locator("#f-submit")).toHaveText("Create schedule");
  await expect(page.locator("#f-start-wrap")).toBeHidden(); // "start" only makes sense for one-off tasks
  await page.locator("#f-time").fill("08:30");
  await page.locator("#f-submit").click();

  const id = await createdId(page, "Scheduled");
  await expect(page.locator("#panel-schedules")).toBeVisible(); // jumps to the Repeating tab
  const r = row(page, id);
  await expect(r).toContainText("Every weekday at 08:30");
  await expect(r.locator(".badge")).toHaveText("Active");
  await expect(count(page, "schedules")).toHaveText("1");
  await expect(count(page, "queue")).toHaveText("0"); // nothing is enqueued until it's due

  const file = path.join(forge.home, "schedules", `${id}.md`);
  expect(fs.readFileSync(file, "utf8")).toContain("30 8 * * 1-5");

  // Pause / resume.
  await r.getByRole("button", { name: "Pause" }).click();
  await expect(r.locator(".badge")).toHaveText("Paused");
  await r.getByRole("button", { name: "Resume" }).click();
  await expect(r.locator(".badge")).toHaveText("Active");

  // Edit: the form is filled from the file, and can't turn into "just once".
  await r.getByRole("button", { name: "Edit" }).click();
  await expect(page.locator("#new-title")).toHaveText("Edit schedule");
  await expect(page.locator("#f-prompt")).toHaveValue("echo nightly");
  await expect(page.locator("#f-repeat")).toHaveValue("weekdays");
  await expect(page.locator("#f-time")).toHaveValue("08:30");
  await expect(page.locator("#f-repeat option[value=once]")).toHaveCount(0);
  await page.locator("#f-repeat").selectOption("weekly");
  await page.locator("#f-day").selectOption("5");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText("Saved the schedule");
  await expect(r).toContainText("Every Friday at 08:30");
  expect(fs.readFileSync(file, "utf8")).toContain("30 8 * * 5");

  // A cron expression the friendly choices can't express is shown as written.
  await r.getByRole("button", { name: "Edit" }).click();
  await page.locator("#f-repeat").selectOption("custom");
  await page.locator("#f-cron").fill("*/20 * * * *");
  await page.locator("#f-submit").click();
  await expect(r).toContainText("*/20 * * * *");

  // A bad expression is refused with a reason, and the schedule is untouched.
  await r.getByRole("button", { name: "Edit" }).click();
  await page.locator("#f-cron").fill("not a cron");
  await page.locator("#f-submit").click();
  await expect(page.locator("#f-error")).not.toBeEmpty();
  await page.locator("#new-cancel").click();
  await expect(r).toContainText("*/20 * * * *");

  // Delete asks first.
  page.once("dialog", (d) => void d.dismiss());
  await r.getByRole("button", { name: "Delete" }).click();
  await expect(r).toBeVisible();
  page.once("dialog", (d) => {
    expect(d.message()).toContain(id);
    void d.accept();
  });
  await r.getByRole("button", { name: "Delete" }).click();
  await expect(page.getByText("No repeating tasks yet.")).toBeVisible();
  expect(fs.existsSync(file)).toBe(false);
});

test("the search box is in the toolbar, says what it searches, and reads what the row says", async ({ page, forge }) => {
  forge.add("fix-login", "Fix the redirect loop");
  fs.mkdirSync(path.join(forge.home, "schedules"), { recursive: true });
  fs.writeFileSync(
    path.join(forge.home, "schedules", "nightly.md"),
    `---\nid: nightly\ncron: "0 9 * * *"\nrepo: ${forge.repo}\n---\n\nRun the audit\n`,
  );
  await openDashboard(page, forge);
  await expect(page.locator("#toolbar #search")).toBeVisible();
  await expect(page.locator("#search")).toHaveAttribute("placeholder", "Search waiting tasks");

  await tab(page, "schedules").click();
  await expect(page.locator("#search")).toHaveAttribute("placeholder", "Search repeating tasks");
  // What the row shows ("Active", "Every day at 9:00"), not just the raw cron.
  await page.locator("#search").fill("active");
  await expect(row(page, "nightly")).toBeVisible();
  await expect(page.locator("#toolbar-count")).toHaveText("1 of 1");
  await page.locator("#search").fill("paused");
  await expect(page.locator("#panel-schedules")).toContainText("Nothing matches");
  await expect(page.locator("#toolbar-count")).toHaveText("0 of 1");
});

test("a schedule sets how it runs, and an emptied field follows the defaults", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await page.locator("#f-prompt").fill("echo nightly");
  await page.locator("#f-repeat").selectOption("daily");
  await page.locator("#f-mode").selectOption("headless");
  await page.locator("#f-worktree").selectOption("no");
  await page.locator("#f-retries").fill("1");
  await page.locator("#f-delay").fill("1m");
  await page.locator("#f-timeout").fill("30m");
  await page.locator("#f-submit").click();
  const id = await createdId(page, "Scheduled");
  const file = path.join(forge.home, "schedules", `${id}.md`);

  let text = fs.readFileSync(file, "utf8");
  expect(text).toContain("mode: headless");
  expect(text).toContain("worktree: false");
  expect(text).toContain("retries: 1");
  expect(text).toContain("retry_delay: 1m");
  expect(text).toContain("timeout: 30m");

  // The edit form starts from the file; anything left empty unpins its field.
  await tab(page, "schedules").click();
  await row(page, id).getByRole("button", { name: "Edit" }).click();
  await expect(page.locator("#f-timeout")).toHaveValue("30m");
  await expect(page.locator("#f-retries")).toHaveValue("1");
  await page.locator("#f-timeout").fill("");
  await page.locator("#f-retries").fill("");
  await page.locator("#f-submit").click();
  await expect(page.locator("#toast")).toHaveText("Saved the schedule");

  text = fs.readFileSync(file, "utf8");
  expect(text).not.toContain("timeout:");
  expect(text).not.toContain("retries:");
  expect(text).toContain("mode: headless"); // untouched
  expect(text).toContain("worktree: false");
});
