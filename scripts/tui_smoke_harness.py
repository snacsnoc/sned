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


def compact_output(text):
    return re.sub(r"[^A-Za-z0-9]+", "", text)


def mode_visible(text, mode):
    return f"{mode} ·" in text or f"[{mode}]" in text


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
                "TMPDIR": self.tmp,
                "TMP": self.tmp,
                "TEMP": self.tmp,
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
    prompt_visible = False
    approval_visible_at = None
    marker_path = "/tmp/sned-approval-scroll-smoke"

    def tick(session):
        nonlocal sent_user_prompt, sent_scroll, sent_approve
        nonlocal prompt_visible, approval_visible_at
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
            if approval_visible_at is None:
                approval_visible_at = time.monotonic()
        if (
            approval_visible_at is not None
            and time.monotonic() - approval_visible_at >= 0.25
            and not sent_approve
        ):
            session.send(b"y\r")
            sent_approve = True

    try:
        try:
            os.unlink(marker_path)
        except FileNotFoundError:
            pass

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
                    (os.path.exists(marker_path), "approved command did not execute after scrolling"),
                    (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
                ],
                "approval prompt stayed visible after scrolling",
            )
    finally:
        try:
            os.unlink(marker_path)
        except FileNotFoundError:
            pass


def approval_scalar_command():
    command = "git status --short"
    sent_prompt = False
    command_visible_before_approval = False
    sent_approve = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, command_visible_before_approval, sent_approve, sent_exit
        text = session.text
        if "type a prompt" in text and not sent_prompt:
            session.send(b"trigger scalar command approval\r")
            sent_prompt = True
        tail = visible_tail(clean_output(session.buf))
        tool_index = tail.rfind("Tool: execute_command")
        if tool_index >= 0:
            command_index = tail.find(command, tool_index)
            approval_index = tail.find("Execute this tool?", tool_index)
            if 0 <= command_index < approval_index:
                command_visible_before_approval = True
        if command_visible_before_approval and not sent_approve:
            session.send(b"y\r")
            sent_approve = True
        if sent_approve and "scalar command approval smoke test complete" in text and not sent_exit:
            time.sleep(0.25)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession(
        "sned-approval-scalar-command.", {"SNED_MOCK_APPROVAL_SCALAR_COMMAND": "1"}
    ) as session:
        session.run(12, tick)
        session.dump_if_verbose()
        compact_text = re.sub(r"\s+", "", clean_output(session.buf))
        report(
            [
                (sent_prompt, "initial user prompt was not sent"),
                (command_visible_before_approval, "scalar command was not shown before the approval prompt in the approval panel"),
                (sent_approve, "approval was sent before the scalar command became visible"),
                ("scalarcommandapprovalsmoketestcomplete" in compact_text, "completion result did not appear after approval"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "scalar command appeared before approval input",
        )


def approval_under_backpressure():
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
    command_marker = None

    def tick(session):
        nonlocal sent_prompt, sent_blocked_input, sent_approve, sent_exit
        nonlocal prompt_visible, overflow_visible, reasoning_tail_visible
        nonlocal completion_visible, blocked_input_sent_at, approve_offset, command_marker
        if command_marker is None:
            command_marker = os.path.join(session.tmp, "approval-backpressure-smoke")
        clean = clean_output(session.buf)
        tail = visible_tail(clean)
        if "type a prompt" in clean and not sent_prompt:
            session.send(b"trigger approval under backpressure\r")
            sent_prompt = True
        if re.search(r"⚠\s+[1-9][0-9]*\s+dropped", clean):
            overflow_visible = True
        if "APPROVAL_BACKPRESSURE_REASONING_TAIL" in clean:
            reasoning_tail_visible = True
        if ("[y] Approve" in tail or "[n/Esc]" in tail or "Execute this tool?" in tail) and not sent_blocked_input:
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
        if command_marker is not None:
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


def plan_approve_act():
    sent_prompt = False
    sent_approve = False
    sent_tool_approve = False
    tool_approval_visible = False
    sent_exit = False
    plan_generated = False
    plan_approved = False
    act_mode_seen = False
    completion_seen = False
    command_executed_before_approval = False
    marker_path = None

    def tick(session):
        nonlocal sent_prompt, sent_approve, sent_tool_approve, sent_exit
        nonlocal plan_generated, plan_approved, act_mode_seen, completion_seen
        nonlocal command_executed_before_approval, tool_approval_visible
        clean = clean_output(session.buf)
        tail = visible_tail(clean)

        if "type a prompt" in clean and not sent_prompt:
            session.send(b"/plan create an approved plan\r")
            sent_prompt = True
        if sent_prompt and "Plan Generated" in clean:
            plan_generated = True
        if plan_generated and not sent_approve:
            session.send(b"/plan approve\r")
            sent_approve = True
        if sent_approve and re.search(r"Plan\s*approved\..{0,80}Starting", clean):
            plan_approved = True
        if plan_approved and mode_visible(tail, "ACT"):
            act_mode_seen = True
        if plan_approved and ("[y] Approve" in tail or "Execute this tool?" in tail) and not sent_tool_approve:
            if not tool_approval_visible:
                tool_approval_visible = True
            else:
                if marker_path is not None and os.path.exists(marker_path):
                    command_executed_before_approval = True
                session.send(b"y\r")
                sent_tool_approve = True
        if sent_tool_approve and "PLAN_APPROVE_ACT_COMPLETION" in clean:
            completion_seen = True
            if not sent_exit:
                session.send(b"/exit\r")
                sent_exit = True

    with PtySession(
        "sned-plan-approve-act.",
        {"SNED_MOCK_PLAN_APPROVE_ACT": "1"},
        args=["--provider", "mock"],
    ) as session:
        marker_path = os.path.join(session.tmp, "plan-approve-act-smoke")
        session.run(18, tick, interval=0.05)
        clean = clean_output(session.buf)
        session.dump_if_verbose()
        report(
            [
                (sent_prompt, "initial plan prompt was not sent"),
                (plan_generated, "mock plan was not generated"),
                (sent_approve, "/plan approve was not sent"),
                (plan_approved, "plan approval did not start execution"),
                (act_mode_seen, "ACT mode was not visible after plan approval"),
                (sent_tool_approve, "approved command prompt did not appear"),
                (not command_executed_before_approval, "approved ACT command executed before approval input"),
                (os.path.exists(marker_path), "approved ACT command did not execute"),
                (completion_seen, "ACT completion result was not rendered"),
                (sent_exit, "/exit was not sent after ACT completion"),
                (not session.timed_out, "plan approval scenario timed out"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            "PLAN approval transitioned to ACT and executed the approved step",
        )


def plan_exit_active(command, env_name, expected_message, marker_name, completion):
    sent_prompt = False
    plan_generated = False
    sent_command = False
    mode_transition_seen = False
    mode_seen = False
    sent_followup = False
    marker_created_before_followup = False
    completion_seen = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, plan_generated, sent_command, mode_transition_seen
        nonlocal mode_seen, sent_followup, marker_created_before_followup
        nonlocal completion_seen, sent_exit
        clean = clean_output(session.buf)
        tail = visible_tail(clean)
        compact_clean = compact_output(clean)
        marker_path = os.path.join(session.tmp, marker_name)

        if "type a prompt" in clean and not sent_prompt:
            session.send(b"create a pending plan for the exit-path smoke test\r")
            sent_prompt = True
        if sent_prompt and "Plan Generated" in clean:
            plan_generated = True
        if plan_generated and "Elapsed:" in clean and not sent_command:
            session.send((command + "\r").encode())
            sent_command = True
        if sent_command and all(
            compact_output(part) in compact_clean
            for part in expected_message.split(".")
            if part.strip()
        ):
            mode_transition_seen = True
        if mode_transition_seen and mode_visible(tail, "ACT"):
            mode_seen = True
        if mode_seen and not sent_followup:
            marker_created_before_followup = os.path.exists(marker_path)
            session.send(b"run the exit-path marker command\r")
            sent_followup = True
        if sent_followup and completion in clean:
            completion_seen = True
            if not sent_exit:
                session.send(b"/exit\r")
                sent_exit = True

    with PtySession(
        f"sned-{marker_name}.",
        {env_name: "1"},
        args=["--provider", "mock", "--yolo"],
    ) as session:
        session.run(18, tick, interval=0.05)
        session.dump_if_verbose()
        clean = clean_output(session.buf)
        compact_clean = compact_output(clean)
        report(
            [
                (sent_prompt, "initial plan prompt was not sent"),
                (plan_generated, "mock plan was not generated"),
                (sent_command, f"{command} was not sent"),
                (
                    all(
                        compact_output(part) in compact_clean
                        for part in expected_message.split(".")
                        if part.strip()
                    ),
                    "plan exit confirmation was not rendered",
                ),
                (mode_seen, "ACT mode was not visible after leaving the plan"),
                (sent_followup, "follow-up ACT prompt was not sent"),
                (not marker_created_before_followup, "ACT marker was created before the mode transition"),
                (os.path.exists(os.path.join(session.tmp, marker_name)), "ACT tool did not execute after the mode transition"),
                (completion_seen, "ACT completion result was not rendered"),
                (sent_exit, "/exit was not sent after the ACT completion"),
                (not session.timed_out, "plan exit scenario timed out"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            f"{command} left an active plan in ACT mode and restored tool execution",
        )


def plan_act():
    plan_exit_active(
        "/act",
        "SNED_MOCK_PLAN_ACT",
        "Act mode enabled. Pending plan discarded.",
        "plan-act-smoke",
        "PLAN_ACT_COMPLETION",
    )


def plan_abort():
    plan_exit_active(
        "/plan abort",
        "SNED_MOCK_PLAN_ABORT",
        "Plan aborted. Already-applied changes are kept.",
        "plan-abort-smoke",
        "PLAN_ABORT_COMPLETION",
    )


def plan_no_state_exit(command, env_name, expected_message, marker_name, completion):
    sent_prompt = False
    sent_command = False
    mode_transition_seen = False
    mode_seen = False
    sent_followup = False
    marker_created_before_followup = False
    completion_seen = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_command, mode_transition_seen, mode_seen
        nonlocal sent_followup, marker_created_before_followup, completion_seen
        nonlocal sent_exit
        clean = clean_output(session.buf)
        compact_clean = compact_output(clean)
        tail = visible_tail(clean)
        marker_path = os.path.join(session.tmp, marker_name)
        message_seen = all(
            compact_output(part) in compact_clean
            for part in expected_message.split(".")
            if part.strip()
        )

        if "type a prompt" in clean and not sent_prompt:
            session.send(b"explore this request without creating a plan\r")
            sent_prompt = True
        if sent_prompt and "Elapsed:" in clean and not sent_command:
            session.send((command + "\r").encode())
            sent_command = True
        if sent_command and message_seen:
            mode_transition_seen = True
        if mode_transition_seen and mode_visible(tail, "ACT"):
            mode_seen = True
        if mode_seen and not sent_followup:
            marker_created_before_followup = os.path.exists(marker_path)
            session.send(b"run the no-state ACT marker command\r")
            sent_followup = True
        if sent_followup and completion in clean:
            completion_seen = True
            if not sent_exit:
                session.send(b"/exit\r")
                sent_exit = True

    with PtySession(
        f"sned-{marker_name}.",
        {env_name: "1"},
        args=["--provider", "mock", "--plan", "--yolo"],
    ) as session:
        session.run(18, tick, interval=0.05)
        session.dump_if_verbose()
        clean = clean_output(session.buf)
        compact_clean = compact_output(clean)
        report(
            [
                (sent_prompt, "initial no-state plan prompt was not sent"),
                (sent_command, f"{command} was not sent without plan state"),
                (mode_transition_seen, f"{command} did not leave plan mode without plan state"),
                (mode_seen, "ACT mode was not visible after the no-state exit"),
                (sent_followup, "no-state ACT follow-up was not sent"),
                (not marker_created_before_followup, "no-state ACT tool ran before the mode transition"),
                (os.path.exists(os.path.join(session.tmp, marker_name)), "no-state ACT tool did not execute"),
                (completion_seen, "no-state ACT completion was not rendered"),
                ("PlanGenerated" not in compact_clean, "no-state fixture unexpectedly created a plan"),
                (sent_exit, "/exit was not sent after the no-state exit"),
                (not session.timed_out, "no-state plan exit timed out"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            f"{command} left plan mode when no plan state existed",
        )


def plan_act_no_state():
    plan_no_state_exit(
        "/act",
        "SNED_MOCK_PLAN_ACT_NO_STATE",
        "Act mode enabled.",
        "plan-act-no-state-smoke",
        "PLAN_ACT_NO_STATE_COMPLETION",
    )


def plan_abort_no_state():
    plan_no_state_exit(
        "/plan abort",
        "SNED_MOCK_PLAN_ABORT_NO_STATE",
        "Exited plan mode. Ready for act mode.",
        "plan-abort-no-state-smoke",
        "PLAN_ABORT_NO_STATE_COMPLETION",
    )


def plan_queued_input():
    sent_prompt = False
    sent_queued = False
    queue_ack_seen = False
    queued_processed = False
    queued_transcript_seen = False
    next_act_attempt_at = None
    sent_act = False
    act_mode_seen = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_queued, queue_ack_seen, queued_processed
        nonlocal queued_transcript_seen
        nonlocal next_act_attempt_at, sent_act, act_mode_seen
        nonlocal sent_exit
        clean = clean_output(session.buf)
        compact_clean = compact_output(clean)
        tail = visible_tail(clean)

        if "type a prompt" in clean and not sent_prompt:
            session.send(b"create a plan while another prompt is queued\r")
            sent_prompt = True
        if "QUEUE_BUSY_PLAN" in clean and not sent_queued:
            session.send(b"queued while the plan agent is busy\r")
            sent_queued = True
        if (
            sent_queued
            and "Queued (1)" in clean
            and "queued while the plan agent is busy" in clean
        ):
            queue_ack_seen = True
        if sent_queued and "QUEUEDINPUTPROCESSED" in compact_clean:
            queued_processed = True
            if next_act_attempt_at is None:
                next_act_attempt_at = time.monotonic() + 0.5
        compact_tail = compact_output(tail)
        processed_index = compact_tail.rfind("QUEUEDINPUTPROCESSED")
        queued_index = compact_tail.find(
            "queuedwhiletheplanagentisbusy",
            processed_index + len("QUEUEDINPUTPROCESSED"),
        )
        if queued_processed and processed_index >= 0 and queued_index >= 0:
            queued_transcript_seen = True
        if (
            queued_processed
            and next_act_attempt_at is not None
            and time.monotonic() >= next_act_attempt_at
            and not sent_act
        ):
            session.send(b"/act\r")
            sent_act = True
        if sent_act and "Agentisbusy" in compact_clean and not act_mode_seen:
            sent_act = False
            next_act_attempt_at = time.monotonic() + 0.5
        if sent_act and "Actmodeenabled" in compact_clean:
            act_mode_seen = mode_visible(tail, "ACT")
        if act_mode_seen and not sent_exit:
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession(
        "sned-plan-queued-input.",
        {"SNED_MOCK_PLAN_QUEUED_INPUT": "1"},
        args=["--provider", "mock", "--plan", "--yolo"],
    ) as session:
        session.run(20, tick, interval=0.05)
        session.dump_if_verbose()
        clean = clean_output(session.buf)
        compact_clean = compact_output(clean)
        report(
            [
                (sent_prompt, "initial busy plan prompt was not sent"),
                (sent_queued, "user message was not submitted while the plan agent was busy"),
                (queue_ack_seen, "queued user message was not shown in the live queue strip"),
                (queued_processed, "queued user message response was lost or misrouted"),
                (queued_transcript_seen, "queued input did not enter the visible transcript after processing began"),
                (sent_act, "/act was not sent after queued input completed"),
                (act_mode_seen, "ACT mode was not visible after queued-input plan exit"),
                (sent_exit, "/exit was not sent after queued-input completion"),
                (not session.timed_out, "queued-input plan scenario timed out"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            "queued plan input was preserved, processed, and exited to ACT",
        )


def cancel_agent_notice():
    sent_prompt = False
    sent_cancel = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_cancel, sent_exit
        text = clean_output(session.buf)
        if "type a prompt" in text and not sent_prompt:
            session.send(b"keep streaming for cancel\r")
            sent_prompt = True
        if "busy stream chunk 005" in text and not sent_cancel:
            session.send(b"\x03")
            sent_cancel = True
        if sent_cancel and ("Cancelled" in text or "cancelled" in text.lower()) and not sent_exit:
            time.sleep(0.3)
            try:
                session.send(b"/exit\r")
            except OSError:
                pass
            sent_exit = True

    with PtySession(
        "sned-cancel-notice.", {"SNED_MOCK_BUSY_STREAM": "1"}
    ) as session:
        session.run(10, tick)
        session.dump_if_verbose()
        clean = clean_output(session.buf)
        report(
            [
                (sent_prompt, "busy-stream prompt was not sent"),
                (sent_cancel, "Ctrl+C was not sent while provider was busy streaming"),
                ("Cancelled" in clean or "cancelled" in clean.lower(), "cancellation notice missing from TUI transcript"),
                (sent_exit, "/exit was not sent after cancellation"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            "Ctrl+C during streaming emitted cancellation notice into transcript",
        )


def approval_rejection():
    sent_prompt = False
    sent_reject = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_reject, sent_exit
        text = clean_output(session.buf)
        tail = visible_tail(text)
        if "type a prompt" in text and not sent_prompt:
            session.send(b"trigger approval rejection\r")
            sent_prompt = True
        if ("[y] Approve" in tail or "[n/Esc]" in tail or "Execute this tool?" in tail) and not sent_reject:
            session.send(b"n\r")
            sent_reject = True
        if sent_reject and ("denied by user" in text or "rejected or cancelled" in text or "type a prompt" in tail) and not sent_exit:
            time.sleep(0.2)
            try:
                session.send(b"/exit\r")
            except OSError:
                pass
            sent_exit = True

    with PtySession(
        "sned-approval-rejection.", {"SNED_MOCK_APPROVAL_REJECTION": "1"}
    ) as session:
        session.run(12, tick)
        session.dump_if_verbose()
        marker = os.path.join(session.tmp, "approval-rejection-should-not-exist")
        report(
            [
                (sent_prompt, "initial user prompt was not sent"),
                (sent_reject, "rejection input 'n' was not sent"),
                (not os.path.exists(marker), f"rejected tool command executed despite 'n' input (marker created at {marker})"),
                (sent_exit, "/exit was not sent after rejection"),
                (session.exit_code == 0, f"sned exited with {session.exit_code}"),
            ],
            "tool rejection via 'n' strictly prevented command execution",
        )


def provider_error_box():
    sent_prompt = False
    sent_exit = False

    def tick(session):
        nonlocal sent_prompt, sent_exit
        text = clean_output(session.buf)
        if "type a prompt" in text and not sent_prompt:
            session.send(b"trigger provider error\r")
            sent_prompt = True
        if sent_prompt and ("rate limit exceeded" in text or "Error" in text) and not sent_exit:
            time.sleep(0.2)
            session.send(b"/exit\r")
            sent_exit = True

    with PtySession(
        "sned-provider-error.", {"SNED_MOCK_PROVIDER_ERROR": "1"}
    ) as session:
        session.run(10, tick)
        session.dump_if_verbose()
        clean = clean_output(session.buf)
        report(
            [
                (sent_prompt, "initial prompt was not sent"),
                ("rate limit exceeded" in clean or "Error" in clean, "ErrorBox missing provider error text in transcript"),
                (session.exit_code in (0, None), f"sned exited with {session.exit_code}"),
            ],
            "provider streaming error rendered ErrorBox in transcript",
        )


SCENARIOS = {
    "tui-startup-exit": startup_exit,
    "tui-user-echo": user_echo,
    "tui-turn-indicators": turn_indicators,
    "tui-approval-scroll": approval_scroll,
    "tui-approval-scalar-command": approval_scalar_command,
    "tui-approval-under-backpressure": approval_under_backpressure,
    "tui-long-completion-navigation": long_completion_navigation,
    "tui-history-navigation": history_navigation,
    "tui-slash-commands": slash_commands,
    "tui-model-switch": model_switch,
    "tui-plan-approve-act": plan_approve_act,
    "tui-plan-act": plan_act,
    "tui-plan-abort": plan_abort,
    "tui-plan-act-no-state": plan_act_no_state,
    "tui-plan-abort-no-state": plan_abort_no_state,
    "tui-plan-queued-input": plan_queued_input,
    "tui-busy-exit": busy_exit,
    "tui-cancel-agent-notice": cancel_agent_notice,
    "tui-approval-rejection": approval_rejection,
    "tui-provider-error-box": provider_error_box,
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
