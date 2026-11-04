// Pausing the runner from the header: nothing new starts, then Resume lets it.
import * as fs from "node:fs";
import * as path from "node:path";
import { count, expect, openDashboard, row, tab, test } from "./fixtures";

test.use({ runner: true });

test("Pause holds queued work until Resume, and shows in the header", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await expect(page.locator("#runner")).toContainText("Runner on");

  await page.locator('#runner [data-action="runner-pause"]').click();
  await expect(page.locator("#runner")).toContainText("Runner paused");
  expect(fs.existsSync(path.join(forge.home, "paused"))).toBe(true);

  forge.add("held", "echo ran");
  await expect(count(page, "queue")).toHaveText("1");
  await page.waitForTimeout(3000); // several 1s runner passes
  await expect(row(page, "held").locator(".badge")).toHaveText("Waiting");

  await page.locator('#runner [data-action="runner-resume"]').click();
  await expect(page.locator("#runner")).toContainText("Runner on");
  expect(fs.existsSync(path.join(forge.home, "paused"))).toBe(false);

  await tab(page, "archive").click();
  await expect(row(page, "held").locator(".badge")).toHaveText("Done");
});
