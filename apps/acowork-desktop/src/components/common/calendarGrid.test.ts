//! The calendar arithmetic behind `DateTimePicker`, pinned without a renderer.
//!
//! What is checked here is exactly what a hand-rolled picker gets wrong:
//! month rollover, leap Februaries, the Monday-first offset (the grid the
//! native WebView2 popup drew for `2026年11月` started at `26 27 28 29 30 31 1`,
//! which is this function's output for that month), and never touching UTC.
//!
//! The value contract itself — a local calendar day written `YYYY-MM-DD` — is
//! `useTaskForm`'s, so these functions are the bridge between the stored
//! instant and the grid, not a second date model.

import { describe, expect, it } from "vitest";
import {
  addDays,
  addMonths,
  buildMonthGrid,
  daysInMonth,
  formatMonthTitle,
  GRID_SIZE,
  mondayFirstWeekday,
  nowDateTimeField,
  parseDateField,
  toDateField,
  toDateTimeField,
  todayDateField,
  weekdayLabels,
} from "./calendarGrid";

describe("parseDateField / toDateField", () => {
  it("round-trips a local calendar day", () => {
    expect(parseDateField("2026-11-06")).toEqual({ year: 2026, month: 11, day: 6 });
    expect(toDateField(2026, 11, 6)).toBe("2026-11-06");
  });

  it("rejects the empty field and anything that is not a padded date", () => {
    for (const bad of ["", "2026-11-6", "26-11-06", "2026/11/06", "2026-11-06T00:00:00Z"]) {
      expect(parseDateField(bad), bad).toBeNull();
    }
  });

  it("rejects a day that does not exist instead of rolling it forward", () => {
    // Silently turning 02-30 into 03-02 would hide the bug that wrote it.
    expect(parseDateField("2026-02-30")).toBeNull();
    expect(parseDateField("2026-04-31")).toBeNull();
    expect(parseDateField("2026-13-01")).toBeNull();
    expect(parseDateField("2026-00-10")).toBeNull();
    // 2024 is a leap year, so 02-29 is a real day.
    expect(parseDateField("2024-02-29")).not.toBeNull();
    expect(parseDateField("2026-02-29")).toBeNull();
  });
});

describe("daysInMonth", () => {
  it("counts the month, including the leap February", () => {
    expect(daysInMonth(2026, 1)).toBe(31);
    expect(daysInMonth(2026, 2)).toBe(28);
    expect(daysInMonth(2024, 2)).toBe(29);
    expect(daysInMonth(2100, 2)).toBe(28); // century, not a leap year
    expect(daysInMonth(2000, 2)).toBe(29); // every 400 years it is
    expect(daysInMonth(2026, 4)).toBe(30);
    expect(daysInMonth(2026, 12)).toBe(31);
  });
});

describe("addMonths", () => {
  it("rolls the year over in both directions", () => {
    expect(addMonths(2026, 1, -1)).toEqual({ year: 2025, month: 12 });
    expect(addMonths(2026, 12, 1)).toEqual({ year: 2027, month: 1 });
    expect(addMonths(2026, 6, 0)).toEqual({ year: 2026, month: 6 });
    expect(addMonths(2026, 3, -4)).toEqual({ year: 2025, month: 11 });
    expect(addMonths(2026, 3, 24)).toEqual({ year: 2028, month: 3 });
  });
});

describe("mondayFirstWeekday", () => {
  it("maps Monday to 0 and Sunday to 6", () => {
    expect(mondayFirstWeekday(new Date(2024, 0, 1))).toBe(0); // Monday
    expect(mondayFirstWeekday(new Date(2026, 10, 1))).toBe(6); // Sunday
    expect(mondayFirstWeekday(new Date(2026, 10, 6))).toBe(4); // Friday
  });
});

describe("buildMonthGrid", () => {
  it("always produces a fixed 6×7 box, so the popup never resizes", () => {
    expect(buildMonthGrid(2026, 11)).toHaveLength(GRID_SIZE);
    expect(buildMonthGrid(2026, 2)).toHaveLength(GRID_SIZE);
    // A month that starts on Monday (2024-01) and one starting on Sunday
    // (2026-11) both fit: 31 + 6 leading days needs 37 cells, 42 are given.
    expect(buildMonthGrid(2024, 1)).toHaveLength(GRID_SIZE);
  });

  it("lays 2026-11 out Monday-first, matching the native popup it replaces", () => {
    const grid = buildMonthGrid(2026, 11);
    // The screenshot this task came from showed `26 27 28 29 30 31 1` on the
    // first row: November 1st 2026 is a Sunday, so six October days lead in.
    expect(grid.slice(0, 7).map((c) => c.day)).toEqual([26, 27, 28, 29, 30, 31, 1]);
    expect(grid[0].value).toBe("2026-10-26");
    expect(grid[0].inMonth).toBe(false);
    expect(grid[6].value).toBe("2026-11-01");
    expect(grid[6].inMonth).toBe(true);
  });

  it("borrows trailing days from the next month", () => {
    const grid = buildMonthGrid(2026, 11);
    const last = grid[GRID_SIZE - 1];
    expect(last.value).toBe("2026-12-06");
    expect(last.inMonth).toBe(false);
  });

  it("contains every day of the month exactly once, in order", () => {
    for (const [year, month, days] of [
      [2026, 11, 30],
      [2024, 2, 29],
      [2026, 2, 28],
      [2026, 12, 31],
    ] as const) {
      const inMonth = buildMonthGrid(year, month).filter((c) => c.inMonth);
      expect(inMonth).toHaveLength(days);
      expect(new Set(inMonth.map((c) => c.value)).size).toBe(days);
      expect(inMonth[0].day).toBe(1);
      expect(inMonth[days - 1].day).toBe(days);
      // Monday-first means each row is a contiguous run of days.
      const values = inMonth.map((c) => c.value);
      expect(values).toEqual([...values].sort());
    }
  });

  it("crosses a year boundary without a UTC shift", () => {
    const grid = buildMonthGrid(2027, 1);
    expect(grid[0].value).toBe("2026-12-28"); // 2027-01-01 is a Friday
    expect(grid[0].inMonth).toBe(false);
    const last = grid[GRID_SIZE - 1];
    expect(last.value).toBe("2027-02-07");
  });
});

describe("addDays", () => {
  it("steps days across month and year boundaries", () => {
    expect(addDays("2026-11-06", 1)).toBe("2026-11-07");
    expect(addDays("2026-11-30", 1)).toBe("2026-12-01");
    expect(addDays("2026-12-31", 1)).toBe("2027-01-01");
    expect(addDays("2026-01-01", -1)).toBe("2025-12-31");
    expect(addDays("2026-11-06", 7)).toBe("2026-11-13");
    expect(addDays("2026-11-06", -7)).toBe("2026-10-30");
  });

  it("leaves an unparseable value alone rather than inventing a date", () => {
    expect(addDays("", 1)).toBe("");
    expect(addDays("2026-02-30", 1)).toBe("2026-02-30");
  });
});

describe("todayDateField", () => {
  it("reads the local calendar day, not the UTC one", () => {
    // 00:30 local on the 6th is still the 5th in UTC west of Greenwich; the
    // field must keep the day the user's calendar shows.
    expect(todayDateField(new Date(2026, 10, 6, 0, 30))).toBe("2026-11-06");
    expect(todayDateField(new Date(2026, 10, 6, 23, 30))).toBe("2026-11-06");
  });
});

describe("toDateTimeField / nowDateTimeField", () => {
  it("writes a local wall clock with no timezone suffix", () => {
    // A `Z` here would mean UTC and would shift the day for half the world.
    expect(toDateTimeField("2026-11-06", 8, 5)).toBe("2026-11-06T08:05");
    expect(toDateTimeField("2026-11-06", 23, 59)).toBe("2026-11-06T23:59");
    expect(toDateTimeField("2026-01-01", 0, 0)).toBe("2026-01-01T00:00");
  });

  it("reads the current local minute", () => {
    expect(nowDateTimeField(new Date(2026, 10, 6, 8, 5))).toBe("2026-11-06T08:05");
  });
});

describe("formatMonthTitle / weekdayLabels", () => {
  it("writes the month title in the app's fixed date grammar", () => {
    expect(formatMonthTitle(2026, 11)).toBe("2026-11");
    expect(formatMonthTitle(2026, 1)).toBe("2026-01");
  });

  it("gives seven localized single-character weekday initials, Monday first", () => {
    expect(weekdayLabels("en")).toEqual(["M", "T", "W", "T", "F", "S", "S"]);
    expect(weekdayLabels("zh-CN")).toHaveLength(7);
    expect(weekdayLabels("zh-CN")[0]).not.toBe(weekdayLabels("zh-CN")[6]);
  });
});
