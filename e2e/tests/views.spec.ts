// Ways around a list that aren't the tabs: the numbers on top, the column
// headers, and the time window — plus the live age of a run and the readable
// view of an agent's JSON output.
import * as fs from "node:fs";
import * as path from "node:path";
import { expect, openDashboard, row, tab, test, type Forge, count, view } from "./fixtures";

/** Move a waiting task into the archive as a finished one, at a given time. */
function finish(forge: Forge, id: string, finishedAt: string) {
  const from = path.join(forge.home, "tasks", `${id}.md`);
  const to = path.join(forge.home, "archive", `${id}.md`);
  fs.writeFileSync(
    to,
    fs
      .readFileSync(from, "utf8")
      .replace(/^status: .*$/m, "status: done")
      .replace(/^(created_at: .*)$/m, `$1\nfinished_at: ${finishedAt}`),
  );
  fs.rmSync(from);
}

test.describe("with the runner off", () => {
  test("the agent and the model get a column and a mark each", async ({ page, forge }) => {
    forge.add("branded", "true", "--agent", "claude", "--model", "claude-sonnet-4");
    // A real-world local model string, which has to match on its family.
    forge.add(
      "local-model",
      "true",
      "--agent",
      "claude",
      "--model",
      "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL",
    );
    forge.add("custom-agent", "true", "--agent", "stub2");
    await openDashboard(page, forge);

    // One column holds both: the agent name, with the model under it.
    const branded = row(page, "branded");
    const agent = (id: string) => row(page, id).locator("td").nth(3).locator(".mark-text").first();
    const model = (id: string) => row(page, id).locator("td").nth(3).locator(".sub");
    await expect(agent("branded")).toHaveText("claude");
    await expect(model("branded")).toHaveText("claude-sonnet-4");
    // A known name gets its brand mark, named in the tooltip.
    await expect(agent("branded")).toHaveAttribute("title", "Claude");
    await expect(model("branded").locator("svg.mark")).toHaveCount(1);
    await expect(model("local-model")).toHaveText("hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL");
    await expect(model("local-model").locator(".mark-text")).toHaveAttribute("title", "QWen");
    // An agent with no mark of its own gets a letter chip, and no model says so.
    await expect(row(page, "custom-agent").locator("td").nth(3).locator(".chip")).toHaveText("S");
    await expect(model("custom-agent")).toHaveText("-");

    // The column sorts like any other.
    await page.locator('#panel-queue th button[data-sort="agent"]').click();
    await expect(page.locator("#panel-queue tbody tr").first()).toContainText("branded");
    await expect(branded).toHaveCount(1);
  });

  test("the column headers sort the waiting list", async ({ page, forge }) => {
    forge.add("bbb", "true");
    forge.add("aaa", "true");
    await openDashboard(page, forge);

    const firstRow = page.locator("#panel-queue tbody tr").first();
    await expect(page.locator("#panel-queue tbody tr")).toHaveCount(2);
    const idHeader = page.locator('#panel-queue th button[data-sort="id"]');

    await idHeader.click();
    await expect(firstRow).toContainText("aaa");
    await expect(page.locator('#panel-queue th[aria-sort="ascending"]')).toHaveCount(1);

    await idHeader.click();
    await expect(firstRow).toContainText("bbb");
    await expect(page.locator('#panel-queue th[aria-sort="descending"]')).toHaveCount(1);
  });

  test("ticking several tasks offers what they all allow, and acts on each", async ({ page, forge }) => {
    forge.add("one", "true");
    forge.add("two", "true");
    forge.add("three", "true");
    await openDashboard(page, forge);

    await page.locator("#select-toggle").click();
    await expect(page.locator("#bulk")).toContainText("0 selected");
    await row(page, "one").locator("input[data-pick]").check();
    await row(page, "two").locator("input[data-pick]").check();
    await expect(page.locator("#bulk")).toContainText("2 selected");

    // Clicking a row ticks it while selecting, rather than opening the task.
    await row(page, "three").locator("td").nth(1).click();
    await expect(page.locator("#bulk")).toContainText("3 selected");
    await expect(page.locator("#task-overlay")).toBeHidden();
    await row(page, "three").locator("input[data-pick]").uncheck();

    // Ticks stay with the list they were made in, whichever way you leave it.
    await page.keyboard.press("2");
    await page.keyboard.press("1");
    await expect(page.locator("#bulk")).toContainText("0 selected");
    await row(page, "one").locator("input[data-pick]").check();
    await row(page, "two").locator("input[data-pick]").check();

    page.once("dialog", (d) => void d.accept());
    await page.locator('#bulk button[data-bulk="cancel"]').click();
    await expect(count(page, "queue")).toHaveText("1");
    await expect(row(page, "three")).toBeVisible();
    await expect(page.locator("#bulk")).toContainText("0 selected");

    await page.keyboard.press("Escape");
    await expect(page.locator("#bulk")).toBeHidden();
  });

  test("the time window hides rows older than it, and the totals stay put", async ({ page, forge }) => {
    forge.add("fresh", "true");
    forge.add("stale", "true");
    finish(forge, "fresh", new Date().toISOString());
    finish(forge, "stale", "2020-01-01T00:05:00+00:00");
    await openDashboard(page, forge);

    await tab(page, "archive").click();
    await expect(page.locator("#panel-archive tbody tr")).toHaveCount(2);

    await page.locator("#age").selectOption("24h");
    await expect(page.locator("#panel-archive tbody tr")).toHaveCount(1);
    await expect(page.locator("#panel-archive tbody tr")).toContainText("fresh");
    await expect(count(page, "archive")).toHaveText("2"); // counted, just not shown

    await page.locator("#toolbar-clear").click();
    await expect(page.locator("#panel-archive tbody tr")).toHaveCount(2);
  });
});

test.describe("with the runner on", () => {
  test.use({ runner: true });

  test("the numbers on top are a way into their list, and the clock ticks", async ({ page, forge }) => {
    forge.add("went-well", "true");
    forge.add("went-bad", "false");
    forge.add("long-run", "sleep 300");
    await openDashboard(page, forge);
    await expect(page).toHaveTitle("Furmiculi");

    await tab(page, "archive").click();
    await expect(row(page, "went-well").locator(".badge")).toHaveText("Done");
    await expect(row(page, "went-bad").locator(".badge")).toHaveText("Failed");

    // "Failed" opens that tab with just those rows...
    await view(page, "failed").click();
    await expect(page.locator("#view-title")).toHaveText("Failed");
    await expect(page.locator("#panel-archive tbody tr")).toHaveCount(1);
    await expect(page.locator("#panel-archive tbody tr")).toContainText("went-bad");

    // ...and "Finished" puts the rest back.
    await view(page, "finished").click();
    await expect(page.locator("#panel-archive tbody tr")).toHaveCount(2);

    // Working: how long it has been going, kept current between refreshes.
    await view(page, "working").click();
    await expect(page.locator("#panel-running tbody tr")).toContainText("long-run");
    const elapsed = row(page, "long-run").locator(".elapsed");
    await expect(elapsed).toHaveText(/^running \d+s$/);
    const first = parseInt((await elapsed.textContent())!.replace(/\D+/g, ""), 10);
    await page.waitForTimeout(2100);
    const later = parseInt((await elapsed.textContent())!.replace(/\D+/g, ""), 10);
    expect(later).toBeGreaterThan(first);

    // An empty list says so rather than showing the last thing it had.
    await view(page, "waiting").click();
    await expect(page.locator("#panel-queue")).toContainText("Nothing is waiting.");
  });

  test("a JSON agent log reads as text, with the raw output one click away", async ({ page, forge }) => {
    forge.add(
      "json-log",
      `printf '%s\\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hello-from-json"},{"type":"toolCall","name":"bash","arguments":{"command":"ls"}}]}}'; sleep 0.2; echo a-warning-from-stderr >&2`,
    );
    await openDashboard(page, forge);

    await tab(page, "archive").click();
    await expect(row(page, "json-log").locator(".badge")).toHaveText("Done");
    await row(page, "json-log").getByRole("button", { name: "Output" }).click();

    // The assistant's text and its tool call, readably; the JSON is not shown,
    // but the line that isn't JSON at all (its stderr) is — that is where a
    // run explains itself when it goes wrong.
    const body = page.locator("#logs-body");
    await expect(body).toContainText("hello-from-json");
    await expect(body).toContainText("bash");
    await expect(body).toContainText("a-warning-from-stderr");
    await expect(body).not.toContainText("message_end");

    await page.locator("#logs-toggle").click();
    await expect(body).toContainText("message_end");
    await page.locator("#logs-toggle").click();
    await expect(body).not.toContainText("message_end");
  });
});
