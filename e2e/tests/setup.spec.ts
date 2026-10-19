// The Setup dialog: which coding agents exist, picking the default, and the
// health checks `furmi doctor` runs.
import * as fs from "node:fs";
import * as path from "node:path";
import { expect, openDashboard, test } from "./fixtures";

test("Setup lists agents and checks, and lets you pick the default agent", async ({ page, forge }) => {
  await openDashboard(page, forge);
  await page.locator("#setup-open").click();
  const overlay = page.locator("#setup-overlay");
  await expect(overlay).toHaveClass(/active/);

  // Built-in presets are always listed, installed or not.
  const agents = overlay.locator("#agent-list li");
  // Rows are matched on their name cell: "stub" must not also match "stub2".
  const agentRow = (name: string) =>
    agents.filter({ has: page.locator(".name", { hasText: new RegExp(`^${name}$`) }) });
  for (const name of ["claude", "codex", "gemini", "opencode", "pi"]) {
    await expect(agentRow(name)).toHaveCount(1);
  }
  const stub = agentRow("stub");
  const stub2 = agentRow("stub2");
  await expect(stub).toContainText("installed");
  await expect(stub).toContainText("default");
  await expect(stub2.getByRole("button", { name: "Use this one" })).toBeVisible();

  // The checks are what `furmi doctor` reports, one line each.
  await expect(overlay.locator("#setup-list li").first()).not.toHaveText("checking…");
  await expect(overlay.locator("#setup-list")).toContainText("config.toml is valid");

  await stub2.getByRole("button", { name: "Use this one" }).click();
  await expect(page.locator("#toast")).toHaveText("stub2 is now the default agent");
  await expect(stub2).toContainText("default");
  await expect(stub.getByRole("button", { name: "Use this one" })).toBeVisible();
  expect(fs.readFileSync(path.join(forge.home, "config.toml"), "utf8")).toMatch(/agent = "stub2"/);

  // With a real choice of agents, the task form offers it, preselecting the default.
  await page.keyboard.press("Escape");
  await expect(overlay).not.toHaveClass(/active/);
  await page.locator("#new-task-open").click();
  await expect(page.locator("#f-agent-wrap")).toBeVisible();
  await expect(page.locator("#f-agent")).toHaveValue("stub2");
});

test("Setup closes from its button, the backdrop and Escape", async ({ page, forge }) => {
  await openDashboard(page, forge);
  const overlay = page.locator("#setup-overlay");

  await page.locator("#setup-open").click();
  await expect(overlay).toHaveClass(/active/);
  await page.locator("#setup-close").click();
  await expect(overlay).not.toHaveClass(/active/);

  await page.locator("#setup-open").click();
  await overlay.click({ position: { x: 5, y: 5 } }); // the backdrop, outside the dialog
  await expect(overlay).not.toHaveClass(/active/);

  await page.locator("#setup-open").click();
  await page.keyboard.press("Escape");
  await expect(overlay).not.toHaveClass(/active/);
});
