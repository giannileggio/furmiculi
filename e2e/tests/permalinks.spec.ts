// What is on screen is in the address bar, so a URL can be shared.
import { expect, openDashboard, row, test } from "./fixtures";

test("opening a task puts it in the URL, and the URL opens it again", async ({ page, forge }) => {
  forge.add("shared", "true");
  await openDashboard(page, forge);

  await row(page, "shared").click();
  await expect(page).toHaveURL(/\/tasks\/shared$/);

  // The link works in a fresh page.
  await page.reload();
  await expect(page.locator("#task-overlay.active")).toBeVisible();
  await expect(page.locator("#task-title")).toHaveText("shared");

  // Back closes it.
  await page.goBack();
  await expect(page.locator("#task-overlay.active")).toHaveCount(0);
  await expect(page).not.toHaveURL(/tasks/);
});

test("the list and the search are in the URL too", async ({ page, forge }) => {
  forge.add("alpha", "true");
  await openDashboard(page, forge);
  await page.locator("#search").fill("alp");
  await expect(page).toHaveURL(/\?q=alp$/);
  await page.reload();
  await expect(page.locator("#search")).toHaveValue("alp");
});

test("a log has its own address", async ({ page, forge }) => {
  forge.add("logged", "true");
  await openDashboard(page, forge);
  await page.goto(page.url().replace(/\/[^/]*$/, "") + "/logs/logged");
  await expect(page.locator("#logs-overlay.active")).toBeVisible();
  await expect(page.locator("#logs-title")).toHaveText("logged");
});
