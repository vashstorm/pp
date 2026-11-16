import { describe, expect, test } from "bun:test";
import {
  duration,
  h,
  liveAge,
  money,
  percent,
  quantile,
  relative,
  short,
  sum,
  uncached,
} from "../../src/web/format.js";

describe("format & escape utilities", () => {
  test("h() escapes all HTML special characters including XSS payloads", () => {
    const payload = `<script>alert("xss")</script>'&`;
    const escaped = h(payload);
    expect(escaped).toBe("&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;&#39;&amp;");
    expect(h(null)).toBe("");
    expect(h(undefined)).toBe("");
    expect(h(42)).toBe("42");
  });

  test.each([
    [null, "—"],
    [undefined, "—"],
    [0, "0"],
    [999, "999"],
    [1250, "1.3K"],
    [12_400, "12K"],
    [3_500_000, "3.5M"],
    [2_000_000_000, "2B"],
    [2_100_000_000, "2.1B"],
  ])("short(%p) -> %p", (input, expected) => {
    expect(short(input)).toBe(expected);
  });

  test.each([
    [null, "—"],
    [0, "$0"],
    [0.000123, "$0.000123"],
    [1.5, "$1.50"],
  ])("money(%p) -> %p", (input, expected) => {
    expect(money(input)).toBe(expected);
  });

  test("uncached() returns null for missing, negative, or overflowing cache counts", () => {
    expect(uncached({ input_tokens: 100, cached_tokens: 40, cache_write_tokens: 10 })).toBe(50);
    expect(uncached({ input_tokens: 100, cached_tokens: null, cache_write_tokens: 10 })).toBeNull();
    expect(uncached({ input_tokens: 100, cached_tokens: 60, cache_write_tokens: 41 })).toBeNull();
    expect(uncached({ input_tokens: 100, cached_tokens: -1, cache_write_tokens: 0 })).toBeNull();
  });

  test("sum() and quantile() preserve unknown-only null semantics", () => {
    expect(sum([])).toBe(0);
    expect(sum([null, undefined])).toBeNull();
    expect(sum([10, null, 5])).toBe(15);
    expect(quantile([null], 0.5)).toBeNull();
    expect(quantile([100, 10, 50, null], 0.5)).toBe(50);
    expect(duration(250)).toBe("0.25s");
    expect(duration(1500)).toBe("1.5s");
    expect(percent(1, 2000)).toBe("0.05%");
  });

  test("relative() and liveAge() boundaries", () => {
    const now = 1_700_000_000_000;
    expect(relative(now - 10_000, now)).toBe("less than a minute ago");
    expect(relative(now - 60_000, now)).toBe("1 minute ago");
    expect(relative(now - 2 * 3_600_000, now)).toBe("about 2 hours ago");
    expect(liveAge(now - 10_000, now)).toBe("just now");
    expect(liveAge(now - 5 * 60_000, now)).toBe("5m ago");
    expect(liveAge(now - 3 * 3_600_000, now)).toBe("3h ago");
  });
});
