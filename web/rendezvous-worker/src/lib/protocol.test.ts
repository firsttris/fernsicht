import { describe, expect, it } from "vitest";

import { MAX_MESSAGE, parseClientMessage, parseHostMessage, toClient, toHost } from "./protocol";

describe("messages from hosts", () => {
  it("are taken in their shapes only", () => {
    expect(parseHostMessage('{"type":"register","key":"ab"}')).toEqual({
      type: "register",
      key: "ab",
    });
    expect(parseHostMessage('{"type":"proof","mac":"00"}')).toEqual({ type: "proof", mac: "00" });
    expect(parseHostMessage('{"type":"answer","connection":3,"answer":"QUJD"}')).toEqual({
      type: "answer",
      connection: 3,
      answer: "QUJD",
    });
    expect(parseHostMessage('{"type":"refuse","connection":3}')).toEqual({
      type: "refuse",
      connection: 3,
    });
    for (const bad of [
      "{",
      "[]",
      "null",
      '{"type":"register"}',
      '{"type":"answer","connection":-1,"answer":"QUJD"}',
      '{"type":"answer","connection":1.5,"answer":"QUJD"}',
      '{"type":"answer","connection":1,"answer":"<script>"}',
      '{"type":"answer","connection":1,"answer":""}',
      '{"type":"offer","connection":1}',
    ]) {
      expect(parseHostMessage(bad), bad).toBeNull();
    }
    expect(parseHostMessage(new ArrayBuffer(4))).toBeNull();
    expect(parseHostMessage(`{"type":"proof","mac":"${"0".repeat(MAX_MESSAGE)}"}`)).toBeNull();
  });
});

describe("messages from clients", () => {
  it("carry one sealed offer", () => {
    expect(parseClientMessage('{"type":"connect","offer":"QUJD"}')).toEqual({
      type: "connect",
      offer: "QUJD",
    });
    expect(parseClientMessage('{"type":"connect"}')).toBeNull();
    expect(parseClientMessage(`{"type":"connect","offer":"${"A".repeat(20_000)}"}`)).toBeNull();
  });
});

describe("messages to both", () => {
  it("are JSON in the Rust side's shapes", () => {
    expect(toHost({ type: "offer", connection: 3, offer: "b64", iceServers: [] })).toBe(
      '{"type":"offer","connection":3,"offer":"b64","iceServers":[]}',
    );
    expect(toClient({ type: "error", code: "rateLimited", message: "slow down" })).toBe(
      '{"type":"error","code":"rateLimited","message":"slow down"}',
    );
  });
});
