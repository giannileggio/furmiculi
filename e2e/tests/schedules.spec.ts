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
