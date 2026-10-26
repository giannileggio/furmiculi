// The dashboard as a thing you work in: what a row says about a task, the view
// behind a click, the search box, the keyboard, and the two ways it adapts
// (narrow screens, dark mode).
import * as fs from "node:fs";
import * as path from "node:path";
import { expect, openDashboard, row, tab, test, type Forge } from "./fixtures";

/** Rewrites a waiting task's file: status, plus any extra front matter. */
function rewrite(forge: Forge, id: string, status: string, extra: string[] = []) {
  const file = forge.taskFile(id)!;
  const text = fs
    .readFileSync(file, "utf8")
    .replace(/^status: .*$/m, `status: ${status}`)
    .replace(/^(status: .*)$/m, (m) => [m, ...extra].join("\n"));
  fs.writeFileSync(file, text);
}

/** Moves a waiting task into the archive as a finished one. */
function finish(forge: Forge, id: string, extra: string[] = []) {
  const from = forge.taskFile(id)!;
  const to = path.join(forge.home, "archive", `${id}.md`);
  rewrite(forge, id, "done", ["finished_at: 2026-10-25T10:00:00+02:00", ...extra]);
  fs.renameSync(from, to);
}

test.describe("with the runner off", () => {
  test("a row carries the ask under the id, and the id copies from the row", async ({ page, forge }) => {
    forge.add("fix-login", "Fix the redirect loop after login, and add a test for it");
    await openDashboard(page, forge);

    // The first line of the prompt, so two tasks in one repo can be told apart.
    await expect(row(page, "fix-login").locator(".summary")).toHaveText(
      "Fix the redirect loop after login, and add a test for it",
    );

    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await row(page, "fix-login").locator(".copy").click({ force: true });
    await expect(page.locator("#toast")).toHaveText("Copied");
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe("fix-login");
  });

  test("the search box narrows the tables, and Esc gives them back", async ({ page, forge }) => {
    forge.add("fix-login", "Fix the redirect loop");
    forge.add("docs-typo", "Fix a typo in the README");
    await openDashboard(page, forge);
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(2);

    // Every word has to match, over the id, the ask, the repo, the agent.
    await page.locator("#search").fill("typo");
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(1);
    await expect(row(page, "docs-typo")).toBeVisible();
    await page.locator("#search").fill("typo login");
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(0);
    await expect(page.locator("#panel-queue")).toContainText("No tasks matching");

    // Esc clears the search rather than leaving you stranded in an empty list.
    await page.locator("#search").press("Escape");
    await expect(page.locator("#search")).toHaveValue("");
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(2);

    // The toolbar's way out clears it too.
    await page.locator("#search").fill("login");
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(1);
    await page.locator("#toolbar-clear").click();
    await expect(page.locator("#search")).toHaveValue("");
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(2);

    // "/" jumps to the box from anywhere on the page.
    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.press("/");
    await expect(page.locator("#search")).toBeFocused();
  });

  test("a model the task didn't pin says where the name came from", async ({ page, forge }) => {
    // Pinned on the task (the stub agent takes no --model flag, so say it in
    // the file, the way a task written by hand would).
    forge.add("pinned", "true");
    rewrite(forge, "pinned", "pending", ["model: claude-sonnet-4"]);
    forge.add("reported", "true");
    finish(forge, "reported", ["model_used: opencode-go/deepseek-v4.1-flash"]);
    await openDashboard(page, forge);

    // Set on the task: it reads like any other cell.
    const pinned = row(page, "pinned").locator("td").nth(3).locator(".sub");
    await expect(pinned).toHaveText("claude-sonnet-4");
    await expect(pinned.locator(".model-inferred")).toHaveCount(0);
    // Set on the task: no footnote needed, the brand mark says the rest.
    await expect(pinned.locator(".mark-text")).toHaveAttribute("title", "Claude");

    // Reported by the agent: dimmed, with the provenance on hover.
    await tab(page, "archive").click();
    const reported = row(page, "reported").locator("td").nth(3).locator(".sub");
    await expect(reported).toHaveText("opencode-go/deepseek-v4.1-flash");
    await expect(reported.locator(".mark-text")).toHaveClass(/model-inferred/);
    await expect(reported.locator(".mark-text")).toHaveAttribute(
      "title",
      /What the agent itself said it ran with/,
    );
    // The provider in front doesn't hide the family: this is a DeepSeek model,
    // not an OpenCode one.
    await expect(reported.locator(".mark-text")).toHaveAttribute("title", /^DeepSeek/);
  });

  test("a run from before furmi kept its model still says what it ran on", async ({ page, forge }) => {
    // No `model_used` in the file (it predates the field); the log is the only
    // place left that names the model, and the task view goes and reads it.
    forge.add("old-run", "true");
    finish(forge, "old-run");
    fs.writeFileSync(
      path.join(forge.home, "logs", "old-run.1.log"),
      '{"type":"message_end","message":{"role":"assistant","provider":"opencode-go","model":"deepseek-v4.1-flash","usage":{"input":10}}}\n',
    );

    await openDashboard(page, forge);
    await tab(page, "archive").click();
    await expect(row(page, "old-run").locator("td").nth(3).locator(".sub")).toHaveText("-");
    await row(page, "old-run").click();
    const model = page.locator("#task-body dd").nth(5);
    await expect(model).toContainText("opencode-go/deepseek-v4.1-flash");
    await expect(model.locator(".mark-text")).toHaveAttribute("title", /Read out of the run's saved log/);
  });

  test("a row opens the task: the whole ask, the workspace, and its buttons", async ({ page, forge }) => {
    forge.add("big-ask", "First line of the ask\n\nand the rest of it, which the table hasn't room for");
    await openDashboard(page, forge);

    await row(page, "big-ask").click();
    const view = page.locator("#task-body");
    await expect(page.locator("#task-overlay")).toBeVisible();
    await expect(view.locator(".prompt")).toContainText("which the table hasn't room for");
    await expect(view).toContainText("no separate worktree");

    // The same buttons the row has, plus the log.
    await expect(view.getByRole("button", { name: "Edit" })).toBeVisible();
    await expect(view.getByRole("button", { name: "Drop it" })).toBeVisible();
    await view.getByRole("button", { name: "Output" }).click();
    await expect(page.locator("#logs-overlay")).toBeVisible();
    // The log closes back to the task it came from, not to the table.
    await page.locator("#logs-close").click();
    await expect(page.locator("#logs-overlay")).toBeHidden();
    await expect(page.locator("#task-overlay")).toBeVisible();

    await page.locator("#task-close").click();
    await expect(page.locator("#task-overlay")).toBeHidden();
  });

  test("j/k walk the table, Enter opens, Esc backs out", async ({ page, forge }) => {
    forge.add("aaa", "first");
    forge.add("bbb", "second");
    await openDashboard(page, forge);

    await page.keyboard.press("j");
    await expect(page.locator("#panel-queue tr.selected")).toHaveCount(1);
    await expect(row(page, "aaa")).toHaveClass(/selected/);
    await page.keyboard.press("j");
    await expect(row(page, "bbb")).toHaveClass(/selected/);
    // The highlight stops at the end rather than wrapping around.
    await page.keyboard.press("j");
    await expect(row(page, "bbb")).toHaveClass(/selected/);
    await page.keyboard.press("k");
    await expect(row(page, "aaa")).toHaveClass(/selected/);

    await page.keyboard.press("Enter");
    await expect(page.locator("#task-body")).toContainText("first");
    await page.keyboard.press("Escape");
    await expect(page.locator("#task-overlay")).toBeHidden();

    // "o" goes straight to the log from the highlighted row.
    await page.keyboard.press("o");
    await expect(page.locator("#logs-overlay")).toBeVisible();
    await page.locator("#logs-close").click();

    // Digits are the tabs; "?" lists all of this.
    await page.keyboard.press("4");
    await expect(tab(page, "schedules")).toHaveClass(/active/);
    await page.keyboard.press("?");
    await expect(page.locator("#keys-overlay")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("#keys-overlay")).toBeHidden();
  });

  test("the theme follows the system until you pick one, and the pick is kept", async ({ page, forge }) => {
    await forge.add("aaaa", "true");
    await openDashboard(page, forge);

    // Following the system: no choice recorded, nothing set on the document.
    await expect(page.locator("#theme-toggle")).toHaveText("Auto");
    expect(await page.evaluate(() => document.documentElement.dataset.theme)).toBeUndefined();

    // Auto -> Light -> Dark -> Auto, and the page really changes colour.
    await page.locator("#theme-toggle").click();
    await expect(page.locator("#theme-toggle")).toHaveText("Light");
    expect(await page.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe(
      "rgb(242, 242, 243)",
    );
    await page.locator("#theme-toggle").click();
    await expect(page.locator("#theme-toggle")).toHaveText("Dark");
    expect(await page.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe(
      "rgb(20, 22, 23)",
    );
    await expect(page.locator("#logo")).toHaveAttribute("src", "/logo-dark.png");

    // The ant in the header is the README's logo, carried in the binary: it
    // loads (so the page's policy allows its own images) and it changes with
    // the page, since its artwork is two-coloured.
    const logo = page.locator("#logo");
    const mark = await page.request.get(`${forge.url}/logo-dark.png`);
    expect(mark.status()).toBe(200);
    expect(mark.headers()["content-type"]).toBe("image/png");
    expect((await mark.body()).subarray(1, 4).toString()).toBe("PNG");
    await expect(logo).toHaveAttribute("src", "/logo-dark.png");
    expect(await logo.evaluate((img) => img.naturalWidth)).toBe(160);

    // A reload still knows; the choice is on this machine, in this browser.
    await page.reload();
    await expect(page.locator("#theme-toggle")).toHaveText("Dark");
    await page.locator("#theme-toggle").click();
    await expect(page.locator("#theme-toggle")).toHaveText("Auto");
    await expect(page.locator("#logo")).toHaveAttribute("src", "/logo.png");
    await page.reload();
    await expect(page.locator("#theme-toggle")).toHaveText("Auto");
  });

  test("a narrow screen turns rows into labelled cards", async ({ page, forge }) => {
    forge.add("narrow", "true");
    await page.setViewportSize({ width: 420, height: 780 });
    await openDashboard(page, forge);

    await expect(page.locator("#panel-queue thead")).toBeHidden();
    const cells = row(page, "narrow").locator("td");
    await expect(cells.nth(2)).toHaveAttribute("data-label", "Project");
    // The buttons are still there, with nothing to label them.
    await expect(row(page, "narrow").getByRole("button", { name: "Drop it" })).toBeVisible();
    // The stats and the toolbar still fit.
    await expect(page.locator("#toolbar")).toBeVisible();
    await expect(page.locator("#search")).toBeVisible();
  });
});

test.describe("with the runner on", () => {
  test.use({ runner: true });

  test("a running task's log follows the agent as it writes", async ({ page, forge }) => {
    forge.add(
      "chatters",
      "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo tick-$i; sleep 1; done",
    );
    await openDashboard(page, forge);

    await tab(page, "running").click();
    await expect(row(page, "chatters").locator(".badge")).toHaveText("Working");
    await row(page, "chatters").getByRole("button", { name: "Output" }).click();

    const body = page.locator("#logs-body");
    await expect(body).toContainText("tick-2");
    // No reopening, no reload: the same open view picks up the next lines.
    await expect(body).toContainText("tick-5", { timeout: 15_000 });
    await expect(page.locator("#logs-live")).toBeVisible();
  });
});

test.describe("with the runner off", () => {
  test("the end of a long log is shown, saying that is what it is", async ({ page, forge }) => {
    forge.add("firehose", "true");
    // A finished-looking run with a log far longer than the page will fetch,
    // so the view has to ask for the tail of it.
    rewrite(forge, "firehose", "running", ["attempts: 1", "started_at: 2026-10-25T10:00:00+02:00"]);
    const log = path.join(forge.home, "logs", "firehose.1.log");
    fs.mkdirSync(path.dirname(log), { recursive: true });
    fs.writeFileSync(log, `first-line-of-the-log\n${"x".repeat(300_000)}\nlast-line-of-the-log\n`);

    await openDashboard(page, forge);
    await tab(page, "running").click();
    await row(page, "firehose").getByRole("button", { name: "Output" }).click();

    await expect(page.locator("#logs-body")).toContainText("last-line-of-the-log");
    await expect(page.locator("#logs-body")).not.toContainText("first-line-of-the-log");
    await expect(page.locator("#logs-cut")).toBeVisible();
  });
});
