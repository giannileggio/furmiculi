// The housekeeping banner: what the runner's last pass left in health.json.
import { execFileSync } from "node:child_process";
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

test("the banner's buttons fix the item they belong to", async ({ page, forge }) => {
  // A task that gave up and is archived, and a merged branch waiting to go.
  forge.add("gave-up", "echo hi");
  const taskFile = path.join(forge.home, "archive", "gave-up.md");
  fs.writeFileSync(
    taskFile,
    fs.readFileSync(forge.taskFile("gave-up")!, "utf8").replace(/^status: .*$/m, "status: failed"),
  );
  fs.rmSync(forge.taskFile("gave-up")!);
  execFileSync("git", ["branch", "furmi/old"], { cwd: forge.repo });

  const file = path.join(forge.home, "health.json");
  fs.writeFileSync(
    file,
    JSON.stringify([
      {
        level: "warn",
        message: "gave-up has used both of its automatic requeues",
        command: "furmi retry gave-up",
        fix: { kind: "retry", id: "gave-up" },
      },
      {
        level: "info",
        message: "branch furmi/old is merged into main and its task is archived",
        command: "git -C repo branch -d furmi/old",
        fix: { kind: "delete_branch", repo: forge.repo, branch: "furmi/old" },
      },
    ]),
  );

  await openDashboard(page, forge);
  const banner = page.locator("#health");
  await banner.locator("summary").click();
  await expect(banner.locator("li").nth(0).getByRole("button")).toHaveText("Retry");
  await expect(banner.locator("li").nth(1).getByRole("button")).toHaveText("Delete branch");

  // One item, from its own button: the task is queued again and leaves the list.
  await banner.locator("li").nth(0).getByRole("button", { name: "Retry" }).click();
  await expect(page.locator("#toast")).toHaveText("Fixed");
  expect(fs.readFileSync(forge.taskFile("gave-up")!, "utf8")).toContain("status: pending");
  expect(fs.existsSync(taskFile)).toBe(false);
  await expect(banner).toContainText("Housekeeping: 1 suggestion");

  // The rest in one go, and the branch goes with `git branch -d`.
  await banner.getByRole("button", { name: "Fix it" }).click();
  await expect(page.locator("#toast")).toHaveText("Fixed");
  await expect(banner).toBeHidden();
  const branches = execFileSync("git", ["branch", "--list", "furmi/*"], { cwd: forge.repo })
    .toString()
    .trim();
  expect(branches).toBe("");
  expect(JSON.parse(fs.readFileSync(file, "utf8"))).toEqual([]);
});

test("a dirty worktree and a hidden pile of branches have buttons too", async ({
  page,
  forge,
}) => {
  // A done task whose worktree still holds work.
  forge.add("left-work", "echo hi");
  const archived = path.join(forge.home, "archive", "left-work.md");
  fs.writeFileSync(
    archived,
    fs
      .readFileSync(forge.taskFile("left-work")!, "utf8")
      .replace(/^status: .*$/m, "status: done"),
  );
  fs.rmSync(forge.taskFile("left-work")!);
  const dir = path.join(forge.home, "worktrees", "left-work");
  execFileSync("git", ["worktree", "add", "-q", "-b", "furmi/left-work", dir], {
    cwd: forge.repo,
  });
  fs.writeFileSync(path.join(dir, "left.txt"), "kept\n");

  // Two merged branches the banner only summarises.
  for (const branch of ["furmi/a", "furmi/b"]) {
    execFileSync("git", ["branch", branch], { cwd: forge.repo });
  }

  const file = path.join(forge.home, "health.json");
  fs.writeFileSync(
    file,
    JSON.stringify([
      {
        level: "warn",
        message: "left-work is done but its worktree has uncommitted changes, so it was kept",
        fix: { kind: "commit_worktree", id: "left-work", dir },
      },
      {
        level: "info",
        message: "...and 2 more merged furmi/* branches",
        fix: {
          kind: "delete_branches",
          branches: [
            { repo: forge.repo, branch: "furmi/a" },
            { repo: forge.repo, branch: "furmi/b" },
          ],
        },
      },
    ]),
  );

  await openDashboard(page, forge);
  const banner = page.locator("#health");
  await banner.locator("summary").click();
  await expect(banner.locator("li").nth(0).getByRole("button")).toHaveText(
    "Commit and remove",
  );
  await expect(banner.locator("li").nth(1).getByRole("button")).toHaveText(
    "Delete all merged",
  );

  // The work is committed on the task's branch before the worktree goes.
  await banner
    .locator("li")
    .nth(0)
    .getByRole("button", { name: "Commit and remove" })
    .click();
  await expect(page.locator("#toast")).toHaveText("Fixed");
  expect(fs.existsSync(dir)).toBe(false);
  const shown = execFileSync("git", ["show", "--stat", "furmi/left-work"], {
    cwd: forge.repo,
  }).toString();
  expect(shown).toContain("work left in the worktree of left-work");
  expect(shown).toContain("left.txt");

  await banner.getByRole("button", { name: "Delete all merged" }).click();
  await expect(banner).toBeHidden();
  const branches = execFileSync("git", ["branch", "--list", "furmi/*"], {
    cwd: forge.repo,
  }).toString();
  expect(branches).toContain("furmi/left-work");
  expect(branches).not.toContain("furmi/a");
  expect(branches).not.toContain("furmi/b");
});
