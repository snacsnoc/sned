#!/usr/bin/env python3

import fcntl
import os
import pty
import re
import select
import shutil
import signal
import struct
import sys
import tempfile
import termios
import time


ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def clean_output(data):
    return ANSI_RE.sub("", data.decode("utf-8", "replace")).replace("\r", "\n")


def visible_tail(text, rows=24):
    lines = [line for line in text.split("\n") if line.strip()]
    return "\n".join(lines[-rows:])


def report(checks, success):
    for passed, failure in checks:
        if not passed:
            print(f"TUI_TEST_FAIL {failure}")
            return
    print(f"TUI_TEST_PASS {success}")


class PtySession:
    def __init__(
        self,
        prefix,
        env_overrides=None,
        args=None,
        use_wrapper=True,
        rows=24,
        columns=80,
    ):
        self.repo = os.environ["REPO_ROOT"]
        self.sned_bin = os.environ["SNED_BIN"]
        self.verbose = os.environ.get("VERBOSE") == "1"
        self.tmp = tempfile.mkdtemp(prefix=prefix)
        self.buf = b""
        self.exit_code = None
        self.reaped = False
        self.timed_out = False

        env = os.environ.copy()
        env.update(
            {
                "SNED_DIR": self.tmp,
                "SNED_DATA_DIR": os.path.join(self.tmp, "data"),
            }
        )
        if use_wrapper:
            env["SNED_NO_ALTERNATE_SCREEN"] = "1"
        if env_overrides:
            env.update(env_overrides)

        args = list(args or ["--provider", "mock"])
        if use_wrapper:
            command = [
                os.path.join(self.repo, "scripts", "sned-pty-helper"),
                str(rows),
                str(columns),
                self.sned_bin,
                *args,
            ]
        else:
            command = [self.sned_bin, *args]

        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(self.repo)
            os.execvpe(command[0], command, env)

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc_value, _traceback):
        if not self.reaped:
            self.terminate()
            self.wait_for_exit(0.5)
        if not self.reaped:
            self.kill()
            self.wait_for_exit(0.5)
        try:
            os.close(self.fd)
        except OSError:
            pass
        shutil.rmtree(self.tmp, ignore_errors=True)

    @property
    def text(self):
        return self.buf.decode("utf-8", "replace")

    def send(self, data):
        os.write(self.fd, data)

    def read(self, interval, size):
        readable, _, _ = select.select([self.fd], [], [], interval)
        if self.fd not in readable:
            return
        try:
            data = os.read(self.fd, size)
        except OSError:
            return
        if not data:
            return
        self.buf += data
        if b"\x1b[6n" in data:
            self.send(b"\x1b[1;1R")

    def poll(self):
        if self.exit_code is not None:
            return True
        try:
            ended, status = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            self.exit_code = 0
            self.reaped = True
            return True
        if ended:
            self.exit_code = os.waitstatus_to_exitcode(status)
            self.reaped = True
            return True
        return False

    def wait_for_exit(self, timeout):
        deadline = time.time() + timeout
        while not self.reaped and time.time() < deadline:
            if self.poll():
                break
            time.sleep(0.05)

    def terminate(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
        except (ChildProcessError, ProcessLookupError):
            if self.exit_code is None:
                self.exit_code = 0

    def kill(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except (ChildProcessError, ProcessLookupError):
            if self.exit_code is None:
                self.exit_code = 0

    def run(self, timeout, tick, interval=0.1, read_size=4096):
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.read(interval, read_size)
            tick(self)
            if self.poll():
                break
        else:
            self.timed_out = True
            self.terminate()

        if not self.reaped:
            self.wait_for_exit(1.0)
        if not self.reaped and self.timed_out:
            self.kill()
            self.wait_for_exit(0.5)
        if self.timed_out:
            self.exit_code = None

    def dump_if_verbose(self):
        if self.verbose:
            print(self.text)


def startup_exit():
    sent_exit = False

    def tick(session):
        nonlocal sent_exit
        if b"type a prompt" in session.buf and not sent_exit:
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession("sned-tui-smoke.") as session:
        session.run(8, tick)
        trace_match = re.search(
            r"TUI mode: tracing output redirected to ([^\n]+)",
            clean_output(session.buf),
        )
        trace_text = ""
        if trace_match:
            try:
                with open(trace_match.group(1).strip(), encoding="utf-8") as trace_file:
                    trace_text = trace_file.read()
            except OSError:
                pass
        session.dump_if_verbose()
        report(
            [
                ("type a prompt" in session.text, "startup banner not rendered"),
                (sent_exit, "/exit was not sent"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
                (trace_match is not None, "TUI trace path was not reported"),
                ("TUI session started" in trace_text, "TUI trace start event missing"),
                ("TUI session ended" in trace_text, "TUI trace end event missing"),
            ],
            "ratatui startup, tracing, and /exit path worked",
        )


def user_echo():
    sent_prompt = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_exit
        if b"type a prompt" in session.buf and not sent_prompt:
            session.send(b"hello world\r")
            sent_prompt = True
        if sent_prompt and b"hello world" in session.buf and not sent_exit:
            time.sleep(0.5)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession("sned-user-echo.") as session:
        session.run(10, tick)
        session.dump_if_verbose()
        report(
            [
                (sent_prompt, "prompt was not sent"),
                (b"\xe2\x9d\xaf" in session.buf or "❯" in session.text, "user message echo missing ❯ prefix in transcript"),
                (sent_exit, "/exit was not sent"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "user message ❯ prefix appeared in transcript",
        )


def turn_indicators():
    sent_prompt = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_exit
        if b"type a prompt" in session.buf and not sent_prompt:
            session.send(b"hello\r")
            sent_prompt = True
        if sent_prompt and b"Mock provider" in session.buf and not sent_exit:
            time.sleep(1.0)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession("sned-turn-ind.") as session:
        session.run(12, tick)
        session.dump_if_verbose()
        report(
            [
                (sent_prompt, "prompt was not sent"),
                (b"\xe2\x9c\xa6" in session.buf or "♦" in session.text, "assistant turn indicator ♦ missing from transcript"),
                (b"\xe2\x94\x80" in session.buf or "─" in session.text, "turn separator ─ missing from transcript"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "turn indicators (♦, ─) appeared in transcript",
        )


def approval_scroll():
    sent_user_prompt = False
    sent_scroll = False
    sent_approve = False
    sent_exit = False
    prompt_visible = False

    def tick(session):
        nonlocal sent_user_prompt, sent_scroll, sent_approve, sent_exit, prompt_visible
        text = session.text
        if "type a prompt" in text and not sent_user_prompt:
            session.send(b"trigger approval scroll\r")
            sent_user_prompt = True
        if "approval scroll line 15" in text and not sent_scroll:
            session.send(b"\x1b[5~\x1b[5~\x1b[5~")
            sent_scroll = True
        tail = visible_tail(clean_output(session.buf))
        if "Execute this tool?" in tail:
            prompt_visible = True
        if "Execute this tool?" in text and not sent_approve:
            session.send(b"y\r")
            sent_approve = True
        if sent_approve and "Task Completed" in text and not sent_exit:
            time.sleep(0.25)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession(
        "sned-approval-scroll.", {"SNED_MOCK_APPROVAL_SCROLL": "1"}
    ) as session:
        session.run(18, tick)
        session.dump_if_verbose()
        report(
            [
                (sent_user_prompt, "initial user prompt was not sent"),
                (sent_scroll, "PageUp was not sent"),
                ("Execute this tool?" in session.text, "approval prompt did not appear after scrolling"),
                (prompt_visible, "approval prompt appeared in transcript but not in the visible viewport"),
                (sent_approve, "approval prompt was not acknowledged"),
                ("approval-scroll smoke test complete" in visible_tail(clean_output(session.buf)), "completion result did not appear in visible viewport after approval"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "approval prompt stayed visible after scrolling",
        )


def approval_under_backpressure():
    command_marker = "/tmp/sned-approval-backpressure-smoke"
    blocked_probe = "BLOCKED_PROBE_555"
    sent_prompt = False
    sent_blocked_input = False
    sent_approve = False
    sent_exit = False
    prompt_visible = False
    overflow_visible = False
    reasoning_tail_visible = False
    completion_visible = False
    blocked_input_sent_at = None
    approve_offset = None

    def tick(session):
        nonlocal sent_prompt, sent_blocked_input, sent_approve, sent_exit
        nonlocal prompt_visible, overflow_visible, reasoning_tail_visible
        nonlocal completion_visible, blocked_input_sent_at, approve_offset
        clean = clean_output(session.buf)
        tail = visible_tail(clean)
        if "type a prompt" in clean and not sent_prompt:
            session.send(b"trigger approval under backpressure\r")
            sent_prompt = True
        if re.search(r"output overflow \(([1-9][0-9]*) dro", clean):
            overflow_visible = True
        if "APPROVAL_BACKPRESSURE_REASONING_TAIL" in clean:
            reasoning_tail_visible = True
        if "Execute this tool?" in tail and not sent_blocked_input:
            prompt_visible = True
            session.send(blocked_probe.encode())
            sent_blocked_input = True
            blocked_input_sent_at = time.time()
        if sent_blocked_input and not sent_approve and time.time() - blocked_input_sent_at >= 0.25:
            approve_offset = len(session.buf)
            session.send(b"y\r")
            sent_approve = True
        if "APPROVAL_BACKPRESSURE_COMPLETION" in tail:
            completion_visible = True
            if not sent_exit:
                session.send(b"/exit\r")
                sent_exit = True

    try:
        with PtySession(
            "sned-approval-backpressure.",
            {
                "SNED_MOCK_APPROVAL_BACKPRESSURE": "1",
                "SNED_OUTPUT_CHANNEL_CAPACITY": "1",
            },
        ) as session:
            session.run(24, tick, interval=0.05, read_size=8192)
            blocked_input_rendered = approve_offset is not None and blocked_probe in clean_output(
                session.buf[approve_offset:]
            )
            session.dump_if_verbose()
            report(
                [
                    (sent_prompt, "initial user prompt was not sent"),
                    (overflow_visible, "bounded output channel did not report dropped output"),
                    (reasoning_tail_visible, "priority reasoning tail was lost under backpressure"),
                    (prompt_visible, "approval overlay was not visible before input"),
                    (sent_blocked_input, "blocked-input probe was not sent"),
                    (not blocked_input_rendered, "ordinary typing mutated the input during approval"),
                    (sent_approve, "approval shortcut was not sent after rendering"),
                    (os.path.exists(command_marker), "approved command did not execute"),
                    (completion_visible, "completion was not visible after approval"),
                    (sent_exit, "/exit was not sent after completion"),
                    (not session.timed_out, "sned did not exit before timeout"),
                    (session.exit_code == 0, f"sned exited with {session.exit_code}"),
                ],
                "approval remained actionable under output backpressure",
            )
    finally:
        try:
            os.unlink(command_marker)
        except FileNotFoundError:
            pass


def long_completion_navigation():
    sent_prompt = False
    sent_scroll_up = False
    scroll_up_offset = None
    top_boundary_fell_through = False
    scroll_down_count = 0
    next_scroll_down_at = None
    completion_bottom_visible = False
    sent_bottom_scroll = False
    boundary_offset = None
    boundary_fell_through = False
    completion_stayed_visible = False
    sent_exit = False
    scroll_up = b"\x1b[5~"
    scroll_down = b"\x1b[6~"

    def tick(session):
        nonlocal sent_prompt, sent_scroll_up, scroll_up_offset, top_boundary_fell_through
        nonlocal scroll_down_count, next_scroll_down_at, completion_bottom_visible
        nonlocal sent_bottom_scroll, boundary_offset, boundary_fell_through
        nonlocal completion_stayed_visible, sent_exit
        clean = clean_output(session.buf)
        tail = visible_tail(clean)
        if "type a prompt" in clean and not sent_prompt:
            session.send(b"trigger long completion navigation\r")
            sent_prompt = True
        if "COMPLETION_NAV_TOP" in tail and "COMPLETION_NAV_BOTTOM" not in tail and not sent_scroll_up:
            scroll_up_offset = len(session.buf)
            time.sleep(0.2)
            session.send(scroll_up * 3 + b"\r")
            time.sleep(0.1)
            os.kill(session.pid, signal.SIGWINCH)
            sent_scroll_up = True
        if sent_scroll_up and not top_boundary_fell_through:
            phase = clean_output(session.buf[scroll_up_offset:])
            if "TRANSCRIPT_NAV_OLDER" in phase:
                top_boundary_fell_through = True
                next_scroll_down_at = time.time()
        if top_boundary_fell_through and not completion_bottom_visible and scroll_down_count < 8 and time.time() >= next_scroll_down_at:
            session.send(scroll_down + b"\r")
            scroll_down_count += 1
            next_scroll_down_at = time.time() + 0.15
        if top_boundary_fell_through and "COMPLETION_NAV_BOTTOM" in tail:
            completion_bottom_visible = True
            if not sent_bottom_scroll:
                boundary_offset = len(session.buf)
                session.send(scroll_down * 3 + b"\r")
                time.sleep(0.1)
                fcntl.ioctl(session.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 81, 0, 0))
                sent_bottom_scroll = True
        if sent_bottom_scroll:
            boundary_phase = clean_output(session.buf[boundary_offset:])
            if "TRANSCRIPT_NAV_RECENT" in boundary_phase:
                boundary_fell_through = True
            if "COMPLETION_NAV_BOTTOM" in boundary_phase:
                completion_stayed_visible = True
            if boundary_fell_through and completion_stayed_visible and not sent_exit:
                time.sleep(0.2)
                session.send(b"/exit\r")
                sent_exit = True

    with PtySession(
        "sned-long-completion.", {"SNED_MOCK_LONG_COMPLETION": "1"}
    ) as session:
        session.run(24, tick, interval=0.05, read_size=8192)
        clean = clean_output(session.buf)
        session.dump_if_verbose()
        report(
            [
                (sent_prompt, "initial user prompt was not sent"),
                (sent_scroll_up, "long completion did not open at its top"),
                (top_boundary_fell_through, "upward navigation at completion top did not scroll transcript"),
                (completion_bottom_visible, "downward navigation did not reach completion bottom"),
                (sent_bottom_scroll, "downward navigation was not sent at completion bottom"),
                (boundary_fell_through, "downward navigation at completion bottom did not scroll transcript"),
                (completion_stayed_visible, "completion disappeared after transcript fallthrough"),
                (not re.search(r"✓\s+COMPLETION_NAV_TOP", clean), "completion also rendered as a generic tool result"),
                (sent_exit, "/exit was not sent after navigation"),
                (not session.timed_out, "sned did not exit before timeout"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            "long completion and transcript navigation shared boundaries",
        )


def ctrlc_quit_empty():
    sent_first = False
    sent_second = False

    def tick(session):
        nonlocal sent_first, sent_second
        if b"type a prompt" in session.buf and not sent_first:
            time.sleep(0.2)
            session.send(b"\x03")
            sent_first = True
        elif sent_first and not sent_second:
            time.sleep(0.3)
            session.send(b"\x03")
            sent_second = True

    with PtySession("sned-ctrlc-smoke.") as session:
        session.run(8, tick)
        session.dump_if_verbose()
        report(
            [
                ("type a prompt" in session.text, "startup banner not rendered"),
                (sent_first, "first Ctrl+C was not sent"),
                (sent_second, "second Ctrl+C was not sent"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "double Ctrl+C quits from idle",
        )


def busy_exit():
    sent_prompt = False
    sent_exit = False
    exit_sent_at = None

    def tick(session):
        nonlocal sent_prompt, sent_exit, exit_sent_at
        text = clean_output(session.buf)
        if "type a prompt" in text and not sent_prompt:
            session.send(b"keep streaming\r")
            sent_prompt = True
        if "busy stream chunk 005" in text and not sent_exit:
            session.send(b"/exit\r")
            sent_exit = True
            exit_sent_at = time.time()

    with PtySession(
        "sned-busy-exit-smoke.", {"SNED_MOCK_BUSY_STREAM": "1"}
    ) as session:
        session.run(8, tick)
        session.dump_if_verbose()
        report(
            [
                (sent_prompt, "busy-stream prompt was not sent"),
                (sent_exit, "/exit was not sent while provider was busy"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
                (exit_sent_at is not None, "exit send timestamp missing"),
                (exit_sent_at is not None and time.time() - exit_sent_at <= 2.5, "/exit did not stop the busy TUI promptly"),
            ],
            "/exit interrupted busy streaming output promptly",
        )


def json_no_prompt():
    with PtySession(
        "sned-json-smoke.", args=["--json"], use_wrapper=False
    ) as session:
        session.run(3, lambda _session: None)
        session.dump_if_verbose()
        has_tui_markers = any(
            marker in session.text for marker in ["type a prompt", "sned ", "Input"]
        )
        report(
            [
                (not session.timed_out, "--json no-prompt timed out"),
                (not has_tui_markers, "--json no-prompt started the TUI"),
                (session.exit_code in (0, None), f"--json no-prompt exited with {session.exit_code}"),
            ],
            "--json no-prompt did not start TUI",
        )


def history_navigation():
    sent_first = False
    sent_second = False
    sent_up = False
    sent_exit = False
    up_offset = None

    def tick(session):
        nonlocal sent_first, sent_second, sent_up, sent_exit, up_offset
        text = clean_output(session.buf)
        compact_text = re.sub(r"\s+", "", text)
        if "type a prompt" in text and not sent_first:
            session.send(b"first command\r")
            sent_first = True
        response_count = compact_text.count("taskcompletedsuccessfully")
        if sent_first and response_count >= 1 and not sent_second:
            time.sleep(0.3)
            session.send(b"second command\r")
            sent_second = True
        if sent_second and response_count >= 2 and not sent_up:
            time.sleep(0.3)
            session.send(b"\x1b[A")
            up_offset = len(session.buf)
            sent_up = True
        if sent_up and not sent_exit:
            time.sleep(0.5)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession("sned-history-nav.") as session:
        session.run(15, tick)
        session.dump_if_verbose()
        post_up_text = clean_output(session.buf[up_offset:]) if up_offset else ""
        compact_post_up_text = re.sub(r"\s+", "", post_up_text)
        report(
            [
                (sent_first, "first prompt was not sent"),
                (sent_second, "second prompt was not sent"),
                (sent_up, "Up arrow was not sent"),
                ("secondcommand" in compact_post_up_text, "previous prompt 'second command' not found after Up arrow"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "history navigation via Up arrow worked",
        )


def slash_commands():
    sent_unknown = False
    sent_help = False
    searched_help = False
    inserted_exit = False
    sent_exit = False

    def tick(session):
        nonlocal sent_unknown, sent_help, searched_help, inserted_exit, sent_exit
        text = clean_output(session.buf)
        compact_text = re.sub(r"\s+", "", text)
        if "type a prompt" in text and not sent_unknown:
            session.send(b"/workflow\r")
            sent_unknown = True
            return
        if sent_unknown and "Unknowncommand/workflow" in compact_text and not sent_help:
            session.send(b"/help\r")
            sent_help = True
            return
        if sent_help and "CommandHelp" in compact_text and not searched_help:
            session.send(b"exit")
            searched_help = True
            return
        if searched_help and "Exittheinteractiveshell" in compact_text and not inserted_exit:
            session.send(b"\r")
            inserted_exit = True
            return
        if inserted_exit and not sent_exit:
            session.send(b"\r")
            sent_exit = True

    with PtySession("sned-slash-cmd.") as session:
        session.run(10, tick)
        session.dump_if_verbose()
        report(
            [
                (sent_help, "/help was not sent"),
                (searched_help, "searchable help overlay did not render"),
                (inserted_exit, "filtered command details did not render"),
                (sent_unknown, "unknown slash command was not sent"),
                (
                    "Unknowncommand/workflow"
                    in re.sub(r"\s+", "", clean_output(session.buf)),
                    "unknown slash command was not rejected locally",
                ),
                (sent_exit, "/exit was not sent after local rejection"),
                (not session.timed_out, "sned did not exit before timeout"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            "/help search and unknown-command rejection worked",
        )


def auto_scroll():
    prompt_count = 0
    banner_seen = False
    pending_prompt = None
    next_prompt_ready_at = 0.0
    next_prompt = 1
    sent_scroll = False
    sent_exit = False

    def tick(session):
        nonlocal prompt_count, banner_seen, pending_prompt, next_prompt_ready_at
        nonlocal next_prompt, sent_scroll, sent_exit
        text = session.text
        tail = text.replace("\r", "\n")
        if "type a prompt" in text:
            banner_seen = True
        if pending_prompt is None and banner_seen and next_prompt <= 5 and time.time() >= next_prompt_ready_at:
            session.send(f"prompt {next_prompt}\r".encode())
            prompt_count += 1
            pending_prompt = next_prompt
        if pending_prompt is not None and f"prompt {pending_prompt}" in tail:
            pending_prompt = None
            next_prompt += 1
            next_prompt_ready_at = time.time() + 0.5
        if prompt_count >= 5 and "Mock provider" in text and not sent_scroll:
            session.send(b"\x1b[5~\x1b[5~\x1b[5~")
            sent_scroll = True
        if sent_scroll and not sent_exit:
            session.send(b"final prompt\r")
            time.sleep(1.0)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession("sned-auto-scroll.") as session:
        session.run(30, tick)
        session.dump_if_verbose()
        viewport = visible_tail(clean_output(session.buf))
        report(
            [
                (prompt_count >= 5, f"only sent {prompt_count} prompts, expected 5"),
                (sent_scroll, "PageUp scroll not sent"),
                ("final prompt" in viewport, "latest prompt not in visible viewport — auto-scroll failed"),
                ("Mock provider" in viewport, "mock response not in visible viewport — auto-scroll failed"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "auto-scroll kept viewport at bottom for new output",
        )


def model_switch():
    sent_model = False
    sent_exit = False

    def tick(session):
        nonlocal sent_model, sent_exit
        if "type a prompt" in session.text and not sent_model:
            session.send(b"/model mock/mock-model\r")
            sent_model = True
        if sent_model and "Model switched to" in session.text and not sent_exit:
            time.sleep(0.25)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession("sned-model-switch.") as session:
        session.run(10, tick)
        clean = ANSI_RE.sub(" ", session.text)
        session.dump_if_verbose()
        report(
            [
                (sent_model, "/model command was not sent"),
                ("Model switched to mock/mock-model" in clean, "'Model switched to' message not found in output"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "/model command rendered switch confirmation",
        )


SCENARIOS = {
    "tui-startup-exit": startup_exit,
    "tui-user-echo": user_echo,
    "tui-turn-indicators": turn_indicators,
    "tui-approval-scroll": approval_scroll,
    "tui-approval-under-backpressure": approval_under_backpressure,
    "tui-long-completion-navigation": long_completion_navigation,
    "tui-history-navigation": history_navigation,
    "tui-slash-commands": slash_commands,
    "tui-auto-scroll": auto_scroll,
    "tui-model-switch": model_switch,
    "tui-busy-exit": busy_exit,
    "json-no-prompt": json_no_prompt,
    "ctrlc-quit-empty": ctrlc_quit_empty,
}


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in SCENARIOS:
        print("TUI_TEST_FAIL unknown pty scenario")
        return 2
    SCENARIOS[sys.argv[1]]()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
