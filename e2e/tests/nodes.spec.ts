// The "Which machine?" choice only exists once another machine is merged in;
// on its own, the form is exactly the old one.
import { expect, openDashboard, test } from "./fixtures";

test("with no other machines the form has no machine choice and still submits", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#new-task-open").click();
  await expect(page.locator("#f-node-wrap")).toBeHidden();
  await page.locator("#f-repeat").selectOption("daily");
  await expect(page.locator("#f-submit")).toBeEnabled();
  await page.locator("#f-repeat").selectOption("once");
  await expect(page.locator("#f-submit")).toBeEnabled();
});
