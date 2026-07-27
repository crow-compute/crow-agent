import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { App } from "./App";

describe("Crow Agent shell", () => {
  it("states the local credential boundary", () => {
    render(<App />);
    expect(screen.getByText("Keys and strategy stay on this device.")).toBeInTheDocument();
    expect(screen.getByText("Outbound only")).toBeInTheDocument();
  });
});

