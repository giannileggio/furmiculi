// The header says which version serves the page, and a page left open across
// an upgrade says it is out of date.
import { expect, openDashboard, test } from "./fixtures";

test("the header shows the serving version, with no reload notice", async ({ page, forge }) => {
  const version = forge.furmi("--version").trim().split(" ").pop()!;
  await openDashboard(page, forge);
  await expect(page.locator("#version")).toHaveText(`v${version}`);
  await expect(page.locator("#reload-notice")).toBeHidden();
});

test("a page from before an upgrade offers a reload, which brings the new one", async ({ page, forge }) => {
  const version = forge.furmi("--version").trim().split(" ").pop()!;
  // The page as an older server sent it; everything after it comes from the
  // real, newer server, which is what a page left open across an upgrade sees.
  const index = new URL("/", forge.url).href;
  await page.route(index, async (route) => {
    const response = await route.fetch();
    const body = await response.text();
    const old = body.replace(`const PAGE_VERSION = "${version}"`, 'const PAGE_VERSION = "0.0.1"');
    expect(old).not.toBe(body);
    await route.fulfill({ response, body: old });
  });
  await openDashboard(page, forge);

  await expect(page.locator("#version")).toHaveText("v0.0.1");
  const notice = page.locator("#reload-notice");
  await expect(notice).toBeVisible();
  await expect(notice).toContainText("furmiculi was updated.");
  await expect(notice).toContainText(`This page is from 0.0.1; the server is now ${version}.`);

  await page.unroute(index);
  await page.getByRole("button", { name: "Reload" }).click();
  await expect(page.locator("#version")).toHaveText(`v${version}`);
  await expect(notice).toBeHidden();
});
