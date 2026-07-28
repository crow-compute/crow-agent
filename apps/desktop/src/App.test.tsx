import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";

const harnessMock = vi.hoisted(() => ({
  authorized: false,
  arenas: [] as Array<Record<string, unknown>>,
}));

vi.mock("./tauri", async () => {
  const actual = await vi.importActual<typeof import("./tauri")>("./tauri");
  return {
    ...actual,
    getAgentStatus: async () => ({
      protocol: "crow.harness.v1",
      executionBoundary: "local_device",
      daemon: "ready",
      activeRun: null,
      deviceAuthorized: harnessMock.authorized,
    }),
    getPublicArenas: async () => ({ arenas: harnessMock.arenas }),
    getRemoteState: async () => ({ devices: [], runs: [] }),
    getAgentVersions: async () => ({ versions: [] }),
  };
});

describe("Crow Agent shell", () => {
  afterEach(() => {
    harnessMock.authorized = false;
    harnessMock.arenas = [];
  });

  it("presents the branded local execution command surface", () => {
    render(<App />);
    expect(screen.getByText(/Trade from/i)).toBeInTheDocument();
    expect(screen.getByText(/Secrets never enter the WebView/)).toBeInTheDocument();
    expect(screen.getByText(/Crow receives signed structured evidence/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Authorize device/ })).toBeEnabled();
    expect(screen.getByRole("group", { name: "Local daemon controls" })).toBeInTheDocument();
    expect(screen.getByText("Safety ceiling")).toBeInTheDocument();
    expect(screen.getByText("Isolated 1×")).toBeInTheDocument();
  });

  it("navigates to the real arena catalog empty state", async () => {
    render(<App />);
    screen.getByRole("button", { name: /Paper arenas/ }).click();
    expect(await screen.findByRole("heading", { name: "PAPER ARENAS" })).toBeInTheDocument();
    expect(screen.getByText("No arena manifest is open.")).toBeInTheDocument();
  });

  it("opens the immutable agent workflow from an enrollable arena", async () => {
    harnessMock.authorized = true;
    harnessMock.arenas = [{
      id: "00000000-0000-0000-0000-000000000001",
      mode: "hyperliquid_testnet",
      manifest: {
        name: "First verified Testnet arena",
        eligible_models: ["crow-qwen3-5-27b"],
      },
      state: "enrollment",
      startsAt: "2026-07-28T00:00:00Z",
      endsAt: "2026-07-29T00:00:00Z",
      ticketsEnabled: false,
      manifestSha256: "a".repeat(64),
      signerPublicKey: "public",
      signature: "signature",
    }];
    render(<App />);
    screen.getByRole("button", { name: /Paper arenas/ }).click();
    const select = await screen.findByRole("button", { name: "Select agent" });
    select.click();
    expect(await screen.findByRole("heading", { name: "First verified Testnet arena" })).toBeInTheDocument();
    expect(screen.getByText("No compatible version yet.")).toBeInTheDocument();
    expect(screen.getByLabelText("Private strategy instructions")).toBeInTheDocument();
  });
});
