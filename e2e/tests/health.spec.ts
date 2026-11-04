// The housekeeping banner: what the runner's last pass left in health.json.
import * as fs from "node:fs";
import * as path from "node:path";
import { expect, openDashboard, test } from "./fixtures";

test("Health findings show in a banner and clear when they go away", async ({ page, forge }) => {
  await openDashboard(page, forge);
  const banner = page.locator("#health");
  await expect(banner).toBeHidden();

  const file = path.join(forge.home, "health.json");
  fs.writeFileSync(
    file,
    JSON.stringify([
      { level: "warn", message: "t1 is done but its worktree has <b>changes</b>" },
      {
        level: "info",
        message: "branch furmi/t2 is merged into main and its task is archived",
        command: "git -C /repo branch -d furmi/t2",
      },
    ]),
  );
  await expect(banner).toContainText("Housekeeping: 1 problem, 1 suggestion");
  await banner.locator("summary").click();
  // Escaped, not rendered as markup.
  await expect(banner.locator("li").first()).toContainText("<b>changes</b>");
  await expect(banner.locator("code")).toHaveText("git -C /repo branch -d furmi/t2");

  fs.writeFileSync(file, "[]");
  await expect(banner).toBeHidden();
});
