// How the dashboard feels to use: asking before something can't be undone and
// not asking when it can, times a person can read, a task that says what went
// wrong and what it did, a log you can work with, and a Setup that explains
// itself.
import * as fs from "node:fs";
import * as path from "node:path";
import { count, expect, openDashboard, row, tab, test, view } from "./fixtures";

test.describe("with the runner off", () => {
  test("dropping a waiting task needs no question, and Undo puts it back", async ({ page, forge }) => {
    forge.add("oops", "echo hi");
    await openDashboard(page, forge);

    await row(page, "oops").getByRole("button", { name: "Drop it" }).click();
    await expect(page.locator("#ask-overlay")).not.toHaveClass(/active/);
    await expect(page.locator("#toast")).toContainText("Dropped oops");
    await expect(count(page, "queue")).toHaveText("0");

    await page.locator("#toast").getByRole("button", { name: "Undo" }).click();
    await expect(page.locator("#toast")).toContainText("oops is waiting again");
    await expect(count(page, "queue")).toHaveText("1");
    await expect(row(page, "oops").locator(".badge")).toHaveText("Waiting");
    expect(fs.readFileSync(forge.taskFile("oops")!, "utf8")).toContain("status: pending");
  });

  test("a question defaults to the safe answer, closes on Escape and keeps Tab inside", async ({ page, forge }) => {
    forge.add("keep-me", "echo hi", "--draft");
    await openDashboard(page, forge);
    await tab(page, "drafts").click();

    await row(page, "keep-me").getByRole("button", { name: "Discard" }).click();
    const ask = page.locator("#ask-overlay");
    await expect(ask).toHaveClass(/active/);
    // Enter must not be the dangerous answer.
    await expect(page.locator("#ask-cancel")).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(ask).not.toHaveClass(/active/);
    expect(forge.taskFile("keep-me")).toBeDefined();

    await row(page, "keep-me").getByRole("button", { name: "Discard" }).click();
    for (let i = 0; i < 4; i++) {
      await page.keyboard.press("Tab");
      await expect(page.locator("#ask-overlay .modal")).toContainText(""); // still there
      expect(await page.evaluate(() => !!document.activeElement?.closest("#ask-overlay"))).toBe(true);
    }
    await page.keyboard.press("Escape");
    await expect(ask).not.toHaveClass(/active/);
    expect(forge.taskFile("keep-me")).toBeDefined();
  });

  test("dialogs say what they are to a screen reader", async ({ page, forge }) => {
    await openDashboard(page, forge);
    for (const id of ["setup-overlay", "new-overlay", "logs-overlay", "task-overlay", "keys-overlay", "ask-overlay"]) {
      const modal = page.locator(`#${id} .modal`);
      await expect(modal).toHaveAttribute("role", /dialog/);
      await expect(modal).toHaveAttribute("aria-modal", "true");
      await expect(modal).toHaveAttribute("aria-labelledby", /.+/);
    }
    await expect(page.locator("#toast")).toHaveAttribute("aria-live", "polite");
  });

  test("a time reads as how long ago or from now, with the exact one under it", async ({ page, forge }) => {
    forge.add("later", "echo hi", "--in", "2h");
    await openDashboard(page, forge);
    const when = row(page, "later").locator(".rel");
    await expect(when).toHaveText(/^in (1|2) h$/);
    await expect(when).toHaveAttribute("title", /\d{4}-\d{2}-\d{2} \d{2}:\d{2}/);
    await expect(row(page, "later").locator(".exact")).toHaveText(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  });

  test("a task that waits for another says so, and the form can set it up", async ({ page, forge }) => {
    await openDashboard(page, forge);
    await page.locator("#new-task-open").click();
    await page.locator("#f-prompt").fill("echo after the other one");
    await page.locator("#f-repo").fill(forge.repo);
    await expect(page.locator("#f-more")).toBeVisible();
    await page.locator("#f-more summary").click();
    await page.locator("#f-base").fill("main");
    await page.locator("#f-after").fill("the-first-one");
    await page.locator("#f-setup").fill("echo ready");
    await page.locator("#f-priority").fill("3");
    await page.locator("#f-submit").click();
    await expect(page.locator("#toast")).toContainText("Queued ");
    const id = (await page.locator("#toast").textContent())!.replace("Queued ", "").trim();

    const file = fs.readFileSync(forge.taskFile(id)!, "utf8");
    expect(file).toMatch(/^base: main$/m);
    expect(file).toMatch(/^setup: echo ready$/m);
    expect(file).toMatch(/^priority: 3$/m);
    expect(file).toContain("the-first-one");
    await expect(row(page, id)).toContainText("the-first-one");
    await expect(row(page, id)).toContainText("priority 3");
  });

  test("the form only offers More options for a new one-off task", async ({ page, forge }) => {
    await openDashboard(page, forge);
    await page.locator("#new-task-open").click();
    await expect(page.locator("#f-more")).toBeVisible();
    await page.locator("#f-repeat").selectOption("daily");
    await expect(page.locator("#f-more")).toBeHidden();
  });

  test("a schedule can be run now, and says how its last run went", async ({ page, forge }) => {
    forge.furmi("schedule", "add", "--cron", "0 3 * * *", "--repo", forge.repo, "--id", "nightly", "echo hi");
    await openDashboard(page, forge);
    await tab(page, "schedules").click();
    await expect(row(page, "nightly").getByText("not yet")).toBeVisible();

    await row(page, "nightly").getByRole("button", { name: "Run now" }).click();
    await expect(page.locator("#toast")).toContainText("Run now: nightly");
    await expect(count(page, "queue")).toHaveText("1");
    await expect(row(page, "nightly").locator("button[data-action=open-task] .badge")).toHaveText("Waiting");
    // The schedule itself is untouched: it still fires at its own time.
    expect(fs.readFileSync(path.join(forge.home, "schedules/nightly.md"), "utf8")).toContain("0 3 * * *");
  });

  test("a link to a task that isn't there says so and leads back", async ({ page, forge }) => {
    await page.goto(`${forge.url}/tasks/no-such-task`);
    await expect(page.locator("#task-body")).toContainText("There's no task called no-such-task");
    await page.getByRole("button", { name: "Back to the list" }).click();
    await expect(page.locator("#task-overlay")).not.toHaveClass(/active/);
    await expect(page).toHaveURL(`${forge.url}/`);
  });

  test("an address the dashboard doesn't have gets a page that leads back", async ({ page, forge }) => {
    const res = await page.goto(`${forge.url}/no/such/page`);
    expect(res!.status()).toBe(404);
    await expect(page.getByRole("heading")).toHaveText("There's nothing at this address");
    await page.getByRole("link", { name: "Back to the dashboard" }).click();
    await expect(page.locator("#home")).toHaveText("~/home");
  });

  test("Setup says how far each agent may go, and keeps the answer in config.toml", async ({ page, forge }) => {
    await openDashboard(page, forge);
    await page.locator("#setup-open").click();
    const claude = page.locator("select.allow[data-allow=claude]");
    await expect(claude).toBeVisible();
    await expect(page.locator("#allow-hint")).toBeVisible();
    await claude.selectOption("edits");
    await expect(page.locator("#toast")).toContainText("claude may now edit files in its workspace");
    const config = fs.readFileSync(path.join(forge.home, "config.toml"), "utf8");
    expect(config).toMatch(/\[unattended\]\nclaude = "edits"/);
    // The stub agents take no flags, so they are not asked about.
    await expect(page.locator("select.allow[data-allow=stub]")).toHaveCount(0);
  });

  test("Setup saves who hears about an ending, and says what is missing", async ({ page, forge }) => {
    await openDashboard(page, forge);
    await page.locator("#setup-open").click();
    await page.locator('#notify-form input[value="failed"]').check();
    await page.locator("#nt-topic").fill("my-furmi");
    await page.locator("#nt-url").fill("https://ntfy.example");
    await page.locator('#notify-form button[type="submit"]').click();
    await expect(page.locator("#toast")).toHaveText("Notification settings saved");
    const config = fs.readFileSync(path.join(forge.home, "config.toml"), "utf8");
    expect(config).toContain('topic = "my-furmi"');
    expect(config).toMatch(/on = \["failed"\]/);

    // A bad address is refused, with the reason.
    await page.locator("#nt-url").fill("ntfy.example");
    await page.locator('#notify-form button[type="submit"]').click();
    await expect(page.locator("#toast")).toContainText("must start with https://");
  });

  test("Setup lists the other machines and the daily token budget is saved", async ({ page, forge }) => {
    forge.fakeNode("box", { state: { version: "0.8.0", home: "~/h", counts: {}, queue: [], drafts: [], running: [], archive: [], schedules: [], templates: [], invalid: [], runner: { alive: true, paused: false }, form: { repos: [], agents: [], agent: null, on_finish: "none" }, health: [] }, logs: { log: "", truncated: false } });
    await openDashboard(page, forge);
    await page.locator("#setup-open").click();
    await expect(page.locator("#nodes-list")).toContainText("box");
    await page.locator("#lim-budget").fill("250000");
    await page.locator("#limits-form button[type=submit]").click();
    await expect(page.locator("#toast")).toHaveText("Limits saved");
    expect(fs.readFileSync(path.join(forge.home, "config.toml"), "utf8")).toContain("daily_token_budget = 250000");
  });

  test("the sign-out controls are only there when the dashboard asks to sign in", async ({ page, forge }) => {
    await openDashboard(page, forge);
    await page.locator("#setup-open").click();
    // The test dashboard trusts this machine, so there is nothing to sign out of.
    await expect(page.locator("#signin-wrap")).toBeHidden();
  });

  test("on a phone, Setup is still there and the sidebar chips hint at more", async ({ page, forge }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openDashboard(page, forge);
    await expect(page.locator("#setup-open")).toBeVisible();
    await expect(page.locator("aside#side nav").first()).toHaveCSS("mask-image", /linear-gradient/);
  });
});

test.describe("with the runner on", () => {
  test.use({ runner: true });

  test("a task that failed shows what it printed last, right in its view", async ({ page, forge }) => {
    forge.add("boom", "echo about to break >&2; echo the-real-reason >&2; exit 3", "--retries", "0");
    await openDashboard(page, forge);
    await view(page, "failed").click();
    await expect(row(page, "boom").locator(".badge")).toHaveText("Failed");
    await row(page, "boom").locator(".summary").click();
    await expect(page.locator("#task-body")).toContainText("What it printed last");
    await expect(page.locator("#task-body pre.tail")).toContainText("the-real-reason");
  });

  test("a link to a task opens the list that holds it", async ({ page, forge }) => {
    forge.add("boom", "exit 3", "--retries", "0");
    await openDashboard(page, forge);
    await view(page, "failed").click();
    await expect(row(page, "boom").locator(".badge")).toHaveText("Failed");

    await page.goto(`${forge.url}/tasks/boom`);
    await expect(page.locator("#task-overlay")).toHaveClass(/active/);
    await expect(view(page, "failed")).toHaveClass(/active/);
    await page.keyboard.press("Escape");
    await expect(page.locator("#panel-archive")).toHaveClass(/active/);
  });

  test("a finished task lists the files it changed", async ({ page, forge }) => {
    forge.add("writer", "echo hi > a.txt; echo more > b.txt", "--on-finish", "commit");
    await openDashboard(page, forge);
    await tab(page, "archive").click();
    await expect(row(page, "writer").locator(".badge")).toHaveText("Done");
    await row(page, "writer").locator(".summary").click();
    await expect(page.locator("#task-body")).toContainText("What it changed");
    await expect(page.locator("#task-body ul.changed")).toContainText("a.txt");
    await expect(page.locator("#task-body ul.changed")).toContainText("b.txt");
    await expect(page.locator("#task-body .stat")).toContainText("2 files changed");
  });

  test("a task waits for the one it runs after, and says so", async ({ page, forge }) => {
    forge.add("first", "sleep 4; echo one");
    forge.add("second", "echo two", "--after", "first");
    await openDashboard(page, forge);
    await expect(row(page, "second")).toContainText("waits for `first` to finish");
    await expect(count(page, "running")).toHaveText("1"); // only the first: the second is held
    await tab(page, "archive").click();
    // Scoped to the list on screen: the waiting row of `second` also names `first`.
    const finished = (id: string) => page.locator("#panel-archive tbody tr", { hasText: id }).locator(".badge");
    await expect(finished("first")).toHaveText("Done", { timeout: 30_000 });
    await expect(finished("second")).toHaveText("Done", { timeout: 30_000 });
  });

  test("the log has colour, a wrap switch, a download, and an attempt for each try", async ({ page, forge }) => {
    forge.add("coloured", "printf '\\033[31mred words\\033[0m\\nplain words\\n'; sleep 1", "--retries", "0");
    await openDashboard(page, forge);
    await tab(page, "archive").click();
    await expect(row(page, "coloured").locator(".badge")).toHaveText("Done");
    await row(page, "coloured").getByRole("button", { name: "Output" }).click();

    const body = page.locator("#logs-body");
    await expect(body.locator(".ansi-31")).toHaveText("red words");
    await expect(body).not.toContainText("[31m");
    await expect(page.locator("#logs-attempt")).toBeHidden(); // one try: nothing to choose

    const wrap = page.locator("#logs-wrap");
    await expect(wrap).toHaveAttribute("aria-pressed", "true");
    await wrap.click();
    await expect(wrap).toHaveAttribute("aria-pressed", "false");
    await expect(body).toHaveClass(/nowrap/);

    const download = page.waitForEvent("download");
    await page.locator("#logs-download").click();
    expect((await download).suggestedFilename()).toBe("coloured.log");
  });

  test("a task that was tried twice lets you read either try", async ({ page, forge }) => {
    forge.add("twice", "echo try-number-$(ls .tries 2>/dev/null | wc -l); mkdir -p .tries; touch .tries/$$; exit 1", "--retries", "1");
    await openDashboard(page, forge);
    await view(page, "failed").click();
    await expect(row(page, "twice").locator(".badge")).toHaveText("Failed", { timeout: 30_000 });
    await row(page, "twice").getByRole("button", { name: "Output" }).click();
    const picker = page.locator("#logs-attempt");
    await expect(picker).toBeVisible();
    await expect(picker.locator("option")).toHaveCount(2);
    await expect(page.locator("#logs-body")).toContainText("try-number-1");
    await picker.selectOption("1");
    await expect(page.locator("#logs-body")).toContainText("try-number-0");
  });
});
