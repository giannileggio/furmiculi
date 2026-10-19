// The main promise of the tool, from the browser: describe work in the form,
// the runner picks it up, an agent does it, and the result shows up.
import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
import { createdId, expect, openDashboard, row, tab, test } from "./fixtures";

test.use({ runner: true });

test("a task queued from the form runs to Finished with its output and commit", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await expect(page.locator("#runner")).toContainText("runner: on");
  await expect(page.locator("#runner-notice")).toBeHidden();
  await expect(page.getByText("No tasks yet")).toBeVisible();

  // Use the empty state's own button, as a first-time user would.
  await page.locator("#panel-queue").getByRole("button", { name: "+ New task" }).click();
  await expect(page.locator("#new-title")).toHaveText("New task");
  await expect(page.locator("#f-repo")).not.toHaveValue(""); // the repo `furmi` was started in
  await page.locator("#f-prompt").fill("echo hello-from-agent; echo data > out.txt");
  await page.locator("#f-finish").selectOption("commit");
  await page.locator("#f-submit").click();

  const id = await createdId(page, "Queued");
  await expect(page.locator("#new-overlay")).not.toHaveClass(/active/);

  // The runner (not the test) moves it along: Waiting -> Working -> Finished.
  await tab(page, "archive").click();
  await expect(row(page, id).locator(".badge")).toHaveText("Done");
  await expect(page.locator("#c-archive")).toHaveText("1");
  await expect(page.locator("#c-queue")).toHaveText("0");

  await row(page, id).getByRole("button", { name: "Output" }).click();
  await expect(page.locator("#logs-title")).toHaveText(id);
  await expect(page.locator("#logs-body")).toContainText("hello-from-agent");
  await page.keyboard.press("Escape");
  await expect(page.locator("#logs-overlay")).not.toHaveClass(/active/);

  // The work really happened: committed on the task's own branch.
  const wt = path.join(forge.home, "worktrees", id);
  const show = execFileSync("git", ["show", "--name-only", "--format=", "HEAD"], { cwd: wt }).toString();
  expect(show.trim()).toBe("out.txt");
});

test("a failed task says why, and Try again reruns it in the same workspace", async ({ page, forge }) => {
  // Fails until ok.txt exists in the workspace, which we then provide.
  forge.add("needs-file", "test -f ok.txt || { echo missing-ok >&2; exit 3; }");
  await openDashboard(page, forge);

  await tab(page, "archive").click();
  const r = row(page, "needs-file");
  await expect(r.locator(".badge")).toHaveText("Failed");
  await expect(r.locator(".why")).toContainText("status 3");
  await expect(r.getByRole("button", { name: "Try again" })).toBeVisible();
  await expect(r.getByRole("button", { name: "Mark done" })).toHaveCount(0); // not offered once failed

  fs.writeFileSync(path.join(forge.home, "worktrees", "needs-file", "ok.txt"), "");
  await r.getByRole("button", { name: "Try again" }).click();

  await expect(row(page, "needs-file").locator(".badge")).toHaveText("Done");
  await expect(row(page, "needs-file").locator(".why")).toHaveCount(0);
});

test("a running task can be marked done or failed from its row, and dropped before it starts", async ({ page, forge }) => {
  forge.add("finish-me", "sleep 300");
  forge.add("quit-me", "sleep 300");
  forge.add("drop-me", "sleep 300");
  await openDashboard(page, forge);

  await tab(page, "running").click();
  await expect(page.locator("#c-running")).toHaveText("3");
  for (const id of ["finish-me", "quit-me", "drop-me"]) {
    await expect(row(page, id).locator(".badge")).toHaveText("Working");
  }

  // Work that ran is either done or failed; "cancelled" is for work that
  // never started, so a running row doesn't offer it.
  await expect(row(page, "drop-me").getByRole("button", { name: "Drop it" })).toHaveCount(0);

  await row(page, "finish-me").getByRole("button", { name: "Mark done" }).click();

  // "Mark failed" asks for a reason, which is kept on the task.
  page.once("dialog", (d) => {
    expect(d.type()).toBe("prompt");
    void d.accept("changed my mind");
  });
  await row(page, "quit-me").getByRole("button", { name: "Mark failed" }).click();

  // Dismissing the reason prompt leaves the task alone.
  page.once("dialog", (d) => void d.dismiss());
  await row(page, "drop-me").getByRole("button", { name: "Mark failed" }).click();
  await expect(row(page, "drop-me").locator(".badge")).toHaveText("Working");

  page.once("dialog", (d) => {
    expect(d.message()).toContain("drop-me");
    void d.accept("");
  });
  await row(page, "drop-me").getByRole("button", { name: "Mark failed" }).click();

  await expect(page.locator("#c-running")).toHaveText("0");
  await tab(page, "archive").click();
  await expect(row(page, "finish-me").locator(".badge")).toHaveText("Done");
  await expect(row(page, "quit-me").locator(".badge")).toHaveText("Failed");
  await expect(row(page, "quit-me").locator(".why")).toContainText("changed my mind");
  await expect(row(page, "drop-me").locator(".badge")).toHaveText("Failed");
  // No reason typed, so it says who failed it rather than showing nothing.
  await expect(row(page, "drop-me").locator(".why")).toContainText("marked failed by hand");
});
