import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";

function tmuxEnv(): NodeJS.ProcessEnv {
  const env = { ...process.env };
  delete env.TMUX;
  delete env.TMUX_PANE;
  return env;
}

test("live harness cleanup does not kill inherited tmux server", async ({}, testInfo) => {
  test.skip(spawnSync("tmux", ["-V"], { stdio: "ignore" }).status !== 0, "tmux unavailable");

  const root = mkdtempSync(join("/tmp", "aoe-parent-tmux-"));
  const socket = join(root, "sentinel.sock");
  const baseEnv = tmuxEnv();
  const create = spawnSync("tmux", ["-S", socket, "new-session", "-d", "-s", "sentinel", "sleep", "600"], {
    env: baseEnv,
    encoding: "utf8",
  });
  expect(create.status, create.stderr).toBe(0);

  const oldTmux = process.env.TMUX;
  const oldTmuxPane = process.env.TMUX_PANE;
  let handle: ServeHandle | undefined;
  let stopped = false;
  try {
    process.env.TMUX = `${socket},0,0`;
    process.env.TMUX_PANE = "%0";

    handle = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });
    await handle.stop();
    stopped = true;

    const alive = spawnSync("tmux", ["-S", socket, "has-session", "-t", "sentinel"], {
      env: baseEnv,
      encoding: "utf8",
    });
    expect(alive.status, alive.stderr).toBe(0);
  } finally {
    if (oldTmux === undefined) {
      delete process.env.TMUX;
    } else {
      process.env.TMUX = oldTmux;
    }
    if (oldTmuxPane === undefined) {
      delete process.env.TMUX_PANE;
    } else {
      process.env.TMUX_PANE = oldTmuxPane;
    }
    if (handle && !stopped) {
      await handle.stop();
    }
    spawnSync("tmux", ["-S", socket, "kill-server"], { env: baseEnv, stdio: "ignore" });
    rmSync(root, { recursive: true, force: true });
  }
});
