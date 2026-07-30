import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  App,
  arenaAcceptsSetup,
  arenaLaunchFailure,
  credentialUnlockFailure,
  nextDecisionCountdown,
  parseHandoffSnapshot,
} from "./App";

const harnessMock = vi.hoisted(() => ({
  authorized: false,
  daemon: "ready",
  activeRun: null as string | null,
  arenas: [] as Array<Record<string, unknown>>,
  arenaFetches: 0,
  unlockCalls: 0,
  versions: [] as Array<Record<string, unknown>>,
  launchFailure: "",
  journal: {
    runs: [] as Array<Record<string, unknown>>,
    selectedRunId: null as string | null,
    events: [] as Array<Record<string, unknown>>,
  },
}));

vi.mock("./tauri", async () => {
  const actual = await vi.importActual<typeof import("./tauri")>("./tauri");
  return {
    ...actual,
    getAgentStatus: async () => ({
      protocol: "crow.harness.v1",
      executionBoundary: "local_device",
      daemon: harnessMock.daemon,
      activeRun: harnessMock.activeRun,
      deviceAuthorized: harnessMock.authorized,
    }),
    getPublicArenas: async () => {
      harnessMock.arenaFetches += 1;
      return { arenas: harnessMock.arenas };
    },
    getRemoteState: async () => ({ devices: [], runs: [] }),
    getLocalRunJournal: async (runId: string | null) => ({
      ...harnessMock.journal,
      selectedRunId: runId ?? harnessMock.journal.selectedRunId,
    }),
    getAgentVersions: async () => ({ versions: harnessMock.versions }),
    prepareHyperliquidWallet: async () => ({
      address: "0x1111111111111111111111111111111111111111",
      approvalUrl: "https://app.hyperliquid-testnet.xyz/API",
    }),
    enrollArena: async () => undefined,
    startLocalArena: async () => {
      if (harnessMock.launchFailure) throw new Error(harnessMock.launchFailure);
      return {
        protocol: "crow.harness.v1",
        executionBoundary: "local_device",
        daemon: "paused",
        activeRun: "run",
        deviceAuthorized: true,
      };
    },
    unlockDeviceCredentials: async () => {
      harnessMock.unlockCalls += 1;
      return { deviceId: "device", accessExpiresAt: "2026-07-28T00:00:00Z" };
    },
  };
});

describe("Crow Agent shell", () => {
  afterEach(() => {
    harnessMock.authorized = false;
    harnessMock.daemon = "ready";
    harnessMock.activeRun = null;
    harnessMock.arenas = [];
    harnessMock.arenaFetches = 0;
    harnessMock.unlockCalls = 0;
    harnessMock.versions = [];
    harnessMock.launchFailure = "";
    harnessMock.journal = { runs: [], selectedRunId: null, events: [] };
    vi.useRealTimers();
  });

  it("touches the credential vault only after an explicit unlock", async () => {
    render(<App />);
    expect(harnessMock.unlockCalls).toBe(0);
    screen.getByRole("button", { name: /Unlock device/ }).click();
    await waitFor(() => expect(harnessMock.unlockCalls).toBe(1));
  });

  it("presents the branded local execution command surface", () => {
    render(<App />);
    const logo = screen.getByRole("img", { name: "Crow" });
    expect(logo).toHaveClass("brand-logo");
    expect(logo.getAttribute("src")).toMatch(/crow-logo/);
    expect(screen.queryByText("LOCAL AGENT")).not.toBeInTheDocument();
    expect(screen.getByText(/Trade from/i)).toBeInTheDocument();
    expect(screen.getByText(/Secrets never enter the WebView/)).toBeInTheDocument();
    expect(screen.getByText(/Crow receives signed structured evidence/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Unlock device/ })).toBeEnabled();
    expect(screen.getByRole("button", { name: /Authorize new/ })).toBeEnabled();
    expect(screen.getByRole("group", { name: "Local daemon controls" })).toBeInTheDocument();
    expect(screen.getByText("Safety ceiling")).toBeInTheDocument();
    expect(screen.getByText("Isolated 1×")).toBeInTheDocument();
  });

  it("navigates to the real arena catalog empty state", async () => {
    render(<App />);
    await act(async () => {
      screen.getByRole("button", { name: /Paper arenas/ }).click();
    });
    expect(await screen.findByRole("heading", { name: "PAPER ARENAS" })).toBeInTheDocument();
    expect(screen.getByText("No arena manifest is open.")).toBeInTheDocument();
  });

  it("shows Studio-style local trade evidence without private payload fields", async () => {
    harnessMock.authorized = true;
    harnessMock.daemon = "paused";
    harnessMock.activeRun = "00000000-0000-0000-0000-000000000007";
    harnessMock.journal = {
      runs: [{
        runId: harnessMock.activeRun,
        arenaId: "00000000-0000-0000-0000-000000000008",
        state: "paused",
        startedAt: "2026-07-30T01:00:00Z",
        latestAt: "2026-07-30T01:15:00Z",
        arenaStartsAt: "2099-07-30T01:00:00Z",
        arenaEndsAt: "2099-07-30T01:30:00Z",
        decisionIntervalSeconds: 900,
        eventCount: 6,
        cycleCount: 1,
        orderCount: 1,
        fillCount: 1,
        allReceipted: true,
      }],
      selectedRunId: harnessMock.activeRun,
      events: [{
        sequence: 6,
        cycleId: "00000000-0000-0000-0000-000000000009",
        eventType: "fill",
        occurredAt: "2026-07-30T01:15:00Z",
        receipted: true,
        details: {
          fills: [{
            coin: "BTC",
            px: "118000",
            sz: "0.001",
            fee: "0.02",
          }],
        },
      }],
    };

    render(<App />);
    screen.getByRole("button", { name: /Trades/ }).click();
    expect(await screen.findByRole("heading", { name: "TRADES" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: /paused \/ 1 fills/i })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "FILL" })).toBeInTheDocument();
    expect(screen.getByText("BTC")).toBeInTheDocument();
    expect(screen.getByText("0.02")).toBeInTheDocument();
    expect(screen.getByText("CHAIN RECEIPTED")).toBeInTheDocument();
    expect(screen.getByText("PAUSED — RESUME BEFORE START")).toBeInTheDocument();
    expect(screen.queryByText(/raw prompt/i)).not.toBeInTheDocument();
  });

  it("renders a useful paused zero-trade journal state", async () => {
    harnessMock.authorized = true;
    harnessMock.daemon = "paused";
    harnessMock.activeRun = "00000000-0000-0000-0000-000000000010";
    harnessMock.journal = {
      runs: [{
        runId: harnessMock.activeRun,
        arenaId: "00000000-0000-0000-0000-000000000011",
        state: "paused",
        startedAt: "2026-07-30T02:00:00Z",
        latestAt: "2026-07-30T02:00:01Z",
        arenaStartsAt: "2099-07-30T02:00:00Z",
        arenaEndsAt: "2099-07-30T02:30:00Z",
        decisionIntervalSeconds: 900,
        eventCount: 2,
        cycleCount: 0,
        orderCount: 0,
        fillCount: 0,
        allReceipted: true,
      }],
      selectedRunId: harnessMock.activeRun,
      events: [],
    };

    render(<App />);
    screen.getByRole("button", { name: /Trades/ }).click();
    expect(await screen.findByRole("heading", { name: /paused \/ no fills yet/i })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "No events in this filter yet." })).toBeInTheDocument();
  });

  it("derives decision windows from the arena schedule and handles every lifecycle state", () => {
    const run = {
      runId: "00000000-0000-0000-0000-000000000020",
      arenaId: "00000000-0000-0000-0000-000000000021",
      state: "running" as const,
      startedAt: "2026-07-30T03:55:00Z",
      latestAt: "2026-07-30T03:55:00Z",
      arenaStartsAt: "2026-07-30T04:00:00Z",
      arenaEndsAt: "2026-07-30T04:45:00Z",
      decisionIntervalSeconds: 900,
      eventCount: 1,
      cycleCount: 0,
      orderCount: 0,
      fillCount: 0,
      allReceipted: true,
    };
    expect(nextDecisionCountdown(run, Date.parse("2026-07-30T03:59:59Z"))).toMatchObject({
      label: "ARENA STARTS IN",
      value: "00:01",
      boundaryAt: "2026-07-30T04:00:00.000Z",
    });
    expect(nextDecisionCountdown(run, Date.parse("2026-07-30T04:00:00Z"))).toMatchObject({
      label: "NEXT DECISION",
      value: "15:00",
      boundaryAt: "2026-07-30T04:15:00.000Z",
    });
    expect(nextDecisionCountdown(
      { ...run, state: "paused" },
      Date.parse("2026-07-30T04:05:00Z"),
    )).toMatchObject({
      label: "PAUSED — RESUME BEFORE NEXT WINDOW",
      value: "10:00",
    });
    expect(nextDecisionCountdown(run, Date.parse("2026-07-30T04:15:00Z"))).toMatchObject({
      label: "NEXT DECISION",
      value: "15:00",
      boundaryAt: "2026-07-30T04:30:00.000Z",
    });
    expect(nextDecisionCountdown(run, Date.parse("2026-07-30T04:30:00Z"))).toMatchObject({
      label: "DECISION WINDOWS COMPLETE",
      value: "00:00",
    });
    expect(nextDecisionCountdown(run, Date.parse("2026-07-30T04:45:00Z"))).toMatchObject({
      label: "ARENA ENDED",
      value: "00:00",
    });
    expect(nextDecisionCountdown(
      { ...run, state: "stopped" },
      Date.parse("2026-07-30T04:05:00Z"),
    )).toMatchObject({ label: "RUN STOPPED", value: "—" });
    expect(nextDecisionCountdown(
      { ...run, arenaStartsAt: null },
      Date.parse("2026-07-30T04:05:00Z"),
    )).toMatchObject({ label: "SCHEDULE UNAVAILABLE", value: "—" });
  });

  it("ticks the selected run countdown once per second", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(Date.parse("2026-07-30T03:59:58Z"));
    harnessMock.authorized = true;
    harnessMock.daemon = "running";
    harnessMock.activeRun = "00000000-0000-0000-0000-000000000022";
    harnessMock.journal = {
      runs: [{
        runId: harnessMock.activeRun,
        arenaId: "00000000-0000-0000-0000-000000000023",
        state: "running",
        startedAt: "2026-07-30T03:55:00Z",
        latestAt: "2026-07-30T03:55:00Z",
        arenaStartsAt: "2026-07-30T04:00:00Z",
        arenaEndsAt: "2026-07-30T04:30:00Z",
        decisionIntervalSeconds: 900,
        eventCount: 1,
        cycleCount: 0,
        orderCount: 0,
        fillCount: 0,
        allReceipted: true,
      }],
      selectedRunId: harnessMock.activeRun,
      events: [],
    };

    render(<App />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
      screen.getByRole("button", { name: /Trades/ }).click();
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByRole("timer")).toHaveAccessibleName("ARENA STARTS IN: 00:02");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.getByRole("timer")).toHaveAccessibleName("ARENA STARTS IN: 00:01");
  });

  it("refreshes the arena catalog without restarting or reopening the credential vault", async () => {
    vi.useFakeTimers();
    render(<App />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(harnessMock.arenaFetches).toBe(1);
    expect(harnessMock.unlockCalls).toBe(0);

    await act(async () => {
      screen.getByRole("button", { name: /Paper arenas/ }).click();
    });
    expect(screen.getByText("No arena manifest is open.")).toBeInTheDocument();
    harnessMock.arenas = [{
      id: "00000000-0000-0000-0000-000000000002",
      mode: "hyperliquid_testnet",
      manifest: {
        name: "Fresh verified Testnet arena",
        eligible_models: ["crow-qwen3-5-27b"],
      },
      state: "enrollment",
      startsAt: "2026-07-29T03:15:00Z",
      endsAt: "2099-07-29T03:45:00Z",
      ticketsEnabled: false,
      manifestSha256: "b".repeat(64),
      signerPublicKey: "public",
      signature: "signature",
    }];

    await act(async () => {
      await vi.advanceTimersByTimeAsync(15_000);
    });

    expect(screen.getByRole("heading", { name: "Fresh verified Testnet arena" }))
      .toBeInTheDocument();
    expect(harnessMock.arenaFetches).toBe(2);
    expect(harnessMock.unlockCalls).toBe(0);
  });

  it("does not present an idle paused companion as an active run", async () => {
    harnessMock.authorized = true;
    harnessMock.daemon = "paused";
    render(<App />);
    expect(await screen.findByRole("heading", { name: "Ready when you are" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Resume" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Stop" })).toBeDisabled();
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
      endsAt: "2099-07-29T00:00:00Z",
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

  it("fails closed for an arena whose immutable end time passed", () => {
    const arena = {
      id: "00000000-0000-0000-0000-000000000001",
      mode: "hyperliquid_testnet",
      manifest: {},
      state: "enrollment",
      startsAt: "2026-07-28T00:00:00Z",
      endsAt: "2026-07-28T00:30:00Z",
      ticketsEnabled: false,
      manifestSha256: "a".repeat(64),
      signerPublicKey: "public",
      signature: "signature",
    };
    expect(arenaAcceptsSetup(arena, Date.parse("2026-07-28T00:29:59Z"))).toBe(true);
    expect(arenaAcceptsSetup(arena, Date.parse("2026-07-28T00:30:00Z"))).toBe(false);
  });

  it("accepts only structured fixed-point handoff snapshots", () => {
    expect(parseHandoffSnapshot("")).toBeNull();
    expect(parseHandoffSnapshot('{"equity_micro_usdc":1000000,"positions":[{"quantity_e8":-42}]}'))
      .toEqual({ equity_micro_usdc: 1000000, positions: [{ quantity_e8: -42 }] });
    expect(() => parseHandoffSnapshot('{"equity":1.25}')).toThrow("handoff_snapshot_invalid");
    expect(() => parseHandoffSnapshot("[1,2,3]")).toThrow("handoff_snapshot_invalid");
  });

  it("renders bounded launch diagnostics without leaking raw errors", () => {
    expect(arenaLaunchFailure("device_authorization_failed")).toMatch(/Reauthorize/);
    expect(arenaLaunchFailure(new Error("agent_version_invalid"))).toMatch(/encrypted agent version/);
    expect(arenaLaunchFailure("unexpected secret-shaped detail")).toBe(
      "Arena launch failed closed. No order was submitted.",
    );
  });

  it("shows a bounded launch failure inside the open setup dialog", async () => {
    harnessMock.authorized = true;
    harnessMock.arenas = [{
      id: "00000000-0000-0000-0000-000000000003",
      mode: "hyperliquid_testnet",
      manifest: {
        name: "Operator-created arena",
        eligible_models: ["crow-qwen3-5-27b"],
      },
      state: "enrollment",
      startsAt: "2099-07-29T03:15:00Z",
      endsAt: "2099-07-29T03:45:00Z",
      ticketsEnabled: false,
      manifestSha256: "c".repeat(64),
      signerPublicKey: "public",
      signature: "signature",
    }];
    harnessMock.versions = [{
      id: "00000000-0000-0000-0000-000000000004",
      agentId: "00000000-0000-0000-0000-000000000005",
      version: 1,
      modelId: "crow-qwen3-5-27b",
      configurationSha256: "d".repeat(64),
      createdAt: "2026-07-29T00:00:00Z",
    }];
    harnessMock.launchFailure = "arena_operation_failed";

    render(<App />);
    screen.getByRole("button", { name: /Paper arenas/ }).click();
    (await screen.findByRole("button", { name: "Select agent" })).click();
    (await screen.findByRole("button", { name: /Continue to venue/ })).click();
    const account = await screen.findByLabelText("Hyperliquid master account");
    fireEvent.change(account, {
      target: { value: "0x2222222222222222222222222222222222222222" },
    });
    screen.getByRole("button", { name: /stage paused/ }).click();

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Could not stage arena");
    expect(alert).toHaveTextContent("Crow rejected the enrollment");
    expect(screen.getByRole("dialog", { name: "Operator-created arena" })).toContainElement(alert);
  });

  it("explains that a denied credential unlock will not retry", () => {
    expect(credentialUnlockFailure("credential_store_unavailable")).toMatch(
      /No more requests will be made this session/,
    );
    expect(credentialUnlockFailure("device_authorization_not_started")).toMatch(
      /Authorize new/,
    );
    expect(credentialUnlockFailure("unexpected secret-shaped detail")).toBe(
      "The local credential vault could not be unlocked. No background retry will run.",
    );
  });
});
