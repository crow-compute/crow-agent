import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { App } from "./App";

describe("Crow Agent shell", () => {
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
});
