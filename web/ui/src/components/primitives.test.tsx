import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { Badge } from "./badge";
import { Button } from "./button";
import { Input, Label } from "./input";
import { Logo } from "./logo";
import { Segmented } from "./segmented";

describe("Button", () => {
  it("renders a button with variant classes", () => {
    render(<Button variant="destructive">Trennen</Button>);
    expect(screen.getByRole("button", { name: "Trennen" })).toHaveClass("bg-destructive");
  });

  it("renders its child element with asChild", () => {
    render(
      <Button asChild size="sm">
        <a href="/x">Link</a>
      </Button>,
    );
    const link = screen.getByRole("link", { name: "Link" });
    expect(link).toHaveAttribute("href", "/x");
    expect(link).toHaveClass("h-8");
  });
});

describe("Badge", () => {
  it("styles online and offline states", () => {
    render(
      <>
        <Badge variant="online">Online</Badge>
        <Badge variant="offline">Offline</Badge>
        <Badge>Fedora</Badge>
      </>,
    );
    expect(screen.getByText("Online")).toHaveClass("text-success");
    expect(screen.getByText("Offline")).toHaveClass("text-muted-foreground");
    expect(screen.getByText("Fedora")).toHaveClass("border-border");
  });
});

describe("Input and Label", () => {
  it("are associated", async () => {
    render(
      <>
        <Label htmlFor="id">Geräte-ID</Label>
        <Input id="id" />
      </>,
    );
    const input = screen.getByLabelText("Geräte-ID");
    await userEvent.type(input, "123");
    expect(input).toHaveValue("123");
  });
});

describe("Logo", () => {
  it("is decorative and sized", () => {
    const { container } = render(<Logo size={40} />);
    const el = container.firstElementChild as HTMLElement;
    expect(el).toHaveAttribute("aria-hidden");
    expect(el.style.width).toBe("40px");
  });
});

describe("Segmented", () => {
  it("is a radio group that reports the chosen value", async () => {
    const onChange = vi.fn();
    render(
      <Segmented
        label="Filter"
        value="all"
        onChange={onChange}
        options={[
          { value: "all", label: "Alle" },
          { value: "online", label: "Online" },
        ]}
      />,
    );
    expect(screen.getByRole("radiogroup", { name: "Filter" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "Alle" })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("radio", { name: "Online" })).toHaveAttribute("aria-checked", "false");
    await userEvent.click(screen.getByRole("radio", { name: "Online" }));
    expect(onChange).toHaveBeenCalledWith("online");
  });
});
