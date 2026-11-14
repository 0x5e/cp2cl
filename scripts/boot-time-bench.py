#!/usr/bin/env python3
#
# Cold-start timing for wireless CarPlay, measured from the board's own log.
#
# A round waits for the page to answer, then for the log to have the whole of a
# run in it, reads the stages out of it, and reboots the board so the next round
# starts from nothing. The first round measures the run that is already up; the
# last one is left running.
#
# The output is markdown: one row per round as it finishes, and the smallest, the
# mean and the largest of each stage after the last one.
#
# For a round to be worth measuring: the phone paired and in range, the head
# unit attached — nothing is published without a CarLife session, so the log
# would have no Bluetooth or CarPlay line at all — and the board on power.
#
# The timestamps are CLOCK_BOOTTIME: seconds since the kernel started counting,
# not since the board was plugged in. The settings below are the whole of what a
# run is changed by.

import time
import urllib.error
import urllib.request

HOST = "192.168.4.1"
ROUNDS = 10

# How long the board is given to answer again, to reach the last stage, and to
# go away after a reboot.
REACH_TIMEOUT = 180
STAGE_TIMEOUT = 180
REBOOT_TIMEOUT = 60

# The stages, in the order they happen: the name the table prints, and the line
# in the log that says the stage was reached — the area column and the event
# under it, one dot apart. A line is `<seconds> <LEVEL> [<area>]\t<event>
# <fields>`, so each marker is split in two to be looked for: `main.started` is
# written as `[main]  started`, not under the name the program logs it by.
STAGES = [
    ("程序启动", "main.started"),
    ("引导页显示", "guide.loaded"),
    ("蓝牙已连接", "bt.channel_opened"),
    ("CarPlay 已连接", "carplay.airplay.connected"),
    ("首帧渲染", "carplay.video.first_frame"),
]

BASE = f"http://{HOST}"


def answers():
    """Whether the page answers, which is the only thing worth waiting for: a
    board whose kernel is running but whose program is not has no log to read."""
    try:
        with urllib.request.urlopen(BASE + "/api/status", timeout=2) as answer:
            return answer.getcode() == 200
    except (urllib.error.URLError, OSError):
        return False


def wait_until(up, timeout, complaint):
    deadline = time.monotonic() + timeout
    while answers() != up:
        if time.monotonic() >= deadline:
            raise SystemExit(complaint)
        time.sleep(1)


def reached(line, marker):
    """Whether a line is the one a stage is read at."""
    area, event = marker.split(".", 1)
    return f"[{area}]" in line and event in line


def read(text):
    """The timestamp of each stage in one run, and None where it never got there.

    Only the last run in the file counts: a program restarted inside one boot
    wrote more than one `started`, and the last is the boot the numbers are
    about.
    """
    lines = text.splitlines()
    start = 0
    for index, line in enumerate(lines):
        if reached(line, STAGES[0][1]):
            start = index
    values = []
    for _, marker in STAGES:
        for line in lines[start:]:
            if reached(line, marker):
                values.append(float(line.split(" ", 1)[0]))
                break
        else:
            values.append(None)
    return values


def measure():
    """One round: wait for the page, then for the run to reach its last stage.

    The page answers while the run is still starting — the log has `started` in
    it and nothing else — so the log is read until every stage is there, or
    until the boot is given up on as not reaching one.

    A read that cannot be made is not the end of the run: the board answers the
    page one moment and drops the connection the next — a board that has just
    come up, an access point that is still settling — so the read is asked for
    again without a word until the stage clock runs out.
    """
    wait_until(True, REACH_TIMEOUT, f"{BASE} did not answer within {REACH_TIMEOUT}s")
    deadline = time.monotonic() + STAGE_TIMEOUT
    values = [None] * len(STAGES)
    while time.monotonic() < deadline:
        try:
            values = read(download())
        except (urllib.error.URLError, OSError):
            time.sleep(1)
            continue
        if None not in values:
            break
        time.sleep(1)
    return values


def download():
    """The whole log as one body. The board is a few metres away and the copy is
    capped at a few megabytes, so a read that takes this long is one that is not
    coming — the caller asks again."""
    with urllib.request.urlopen(BASE + "/api/log/download", timeout=10) as answer:
        return answer.read().decode("utf-8", "replace")


def reboot():
    """Asks the board to restart and waits for it to go.

    The answer is written before the reboot, but the connection can still be
    dropped mid-body, so whether it went is the wait's question rather than the
    request's.
    """
    try:
        urllib.request.urlopen(
            urllib.request.Request(BASE + "/api/reboot", method="POST"), timeout=5
        ).read()
    except (urllib.error.URLError, OSError):
        pass
    wait_until(
        False,
        REBOOT_TIMEOUT,
        f"{BASE} still answers after {REBOOT_TIMEOUT}s; the reboot did not take",
    )


def cells(values):
    """One row of a markdown table."""
    return "| " + " | ".join(values) + " |"


def row(values):
    """One round as a row, `--` where the run never reached the stage."""
    print(cells([f"{value:.2f}" if value is not None else "--" for value in values]))


def summary(rows):
    """The stages as rows, each with the smallest, the mean and the largest of
    the rounds that reached it."""
    print()
    print(cells(["阶段", "最小值", "平均值", "最大值"]))
    print(cells(["----"] * 4))
    for index, (name, _) in enumerate(STAGES):
        got = [values[index] for values in rows if values[index] is not None]
        if not got:
            print(cells([name, "--", "--", "--"]))
            continue
        low, mean, high = min(got), sum(got) / len(got), max(got)
        print(cells([name] + [f"{value:.2f}" for value in (low, mean, high)]))


def main():
    print(cells([name for name, _ in STAGES]))
    print(cells(["----"] * len(STAGES)))
    rows = []
    for index in range(1, ROUNDS + 1):
        rows.append(measure())
        row(rows[-1])
        # The last round has nobody waiting on it, so it is left running: a
        # benchmark that ends by taking the board away is one to wait for.
        if index < ROUNDS:
            reboot()
    summary(rows)


if __name__ == "__main__":
    main()
