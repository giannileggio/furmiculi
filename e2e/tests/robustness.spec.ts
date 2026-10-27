// What the page does with hostile or broken input.
import * as fs from "node:fs";
import * as path from "node:path";
import { expect, openDashboard, test, count } from "./fixtures";

test("an unreadable task file is reported without breaking the page", async ({ page, forge }) => {
  forge.add("fine", "echo ok");
  fs.writeFileSync(path.join(forge.home, "tasks", "broken.md"), "this has no frontmatter\n");
  await openDashboard(page, forge);

  const warn = page.locator("#invalid");
  await expect(warn).toBeVisible();
  await expect(warn).toContainText("1 file(s) can't be read");
  await expect(warn).toContainText("broken.md");
  // The good task still shows, and the page keeps refreshing.
  await expect(count(page, "queue")).toHaveText("1");
  await expect(page.locator("tbody tr", { hasText: "fine" })).toBeVisible();

  fs.rmSync(path.join(forge.home, "tasks", "broken.md"));
  await expect(warn).toBeHidden(); // fixed on disk, gone on the page
});

test("text from task files is shown as text, never run as markup", async ({ page, forge }) => {
  // File names and errors come from files anyone could have written.
  const evil = `x"<img src=x onerror="window.__pwned=1">`;
  fs.writeFileSync(path.join(forge.home, "tasks", `${evil}.md`), "garbage\n");
  await openDashboard(page, forge);

  await expect(page.locator("#invalid")).toBeVisible();
  await expect(page.locator("#invalid")).toContainText("<img src=x");
  await expect(page.locator("#invalid img")).toHaveCount(0);
  expect(await page.evaluate(() => (window as any).__pwned)).toBeUndefined();
});

test("a task whose log is empty or missing says so instead of failing", async ({ page, forge }) => {
  forge.add("never-ran", "echo hi");
  await openDashboard(page, forge);

  await page.locator("tbody tr", { hasText: "never-ran" }).getByRole("button", { name: "Output" }).click();
  await expect(page.locator("#logs-overlay")).toHaveClass(/active/);
  await expect(page.locator("#logs-title")).toHaveText("never-ran");
  await expect(page.locator("#logs-body")).not.toHaveText("loading…");
  await page.locator("#logs-close").click();
  await expect(page.locator("#logs-overlay")).not.toHaveClass(/active/);
});

test("the dashboard refuses actions without its page token", async ({ page, forge }) => {
  forge.add("target", "echo hi");
  await openDashboard(page, forge);

  // Same-origin fetch, as a script on the page would, but without the token.
  const status = await page.evaluate(async () => {
    const res = await fetch("/api/tasks/target/cancel", { method: "POST" });
    return res.status;
  });
  expect(status).toBe(403);
  await expect(page.locator("tbody tr", { hasText: "target" }).locator(".badge")).toHaveText("Waiting");

  // Another site's page can't read the token out of the dashboard either.
  const res = await page.request.get(forge.url, { headers: { Host: "evil.example" } });
  expect(res.status()).toBeGreaterThanOrEqual(400);
});
